//! Project data model (PLAN section 4).

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::keyframe::Param;
use crate::time::TimeUs;

pub type Id = Ulid;
pub const SCHEMA_VERSION: u32 = 1;

fn new_id() -> Id {
    Ulid::new()
}

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub schema_version: u32,
    pub id: Id,
    pub name: String,
    pub created_at: String,
    #[serde(default)]
    pub settings: ProjectSettings,
    #[serde(default)]
    pub assets: Vec<MediaAsset>,
    #[serde(default)]
    pub transcripts: Vec<TranscriptDoc>,
    /// Reusable avatar setups (persisted with the project, exportable/importable).
    #[serde(default)]
    pub avatars: Vec<AvatarConfig>,
    pub sequences: Vec<Sequence>,
    pub active_sequence: Id,
}

/// One avatar expression: a still image or a video loop, named by the
/// emotion the classifier matches ("angry", "calm", "wow" …) — the same
/// shape as the toolkit's `{ emotion: path }` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarExpression {
    /// Key used by the emotion classifier and stored in the transcript.
    pub name: String,
    /// Absolute or project-relative path to an image or video.
    pub path: String,
}

/// A complete avatar setup: expressions + look + which model classifies.
/// Persisted in the project and exportable as a standalone JSON that stays
/// compatible with the Youtubers-toolkit `avatar_config/config.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarConfig {
    pub id: Id,
    pub name: String,
    /// First entry is the default expression.
    pub expressions: Vec<AvatarExpression>,
    #[serde(default = "default_shake")]
    pub shake_factor: f64,
    #[serde(default = "default_avatar_scale")]
    pub scale: f64,
    /// LLM used to classify emotions ("" = offline heuristic).
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_base: String,
    /// Stored with the project; leave empty to read OPENAI_API_KEY instead.
    #[serde(default)]
    pub api_key: String,
}

fn default_shake() -> f64 {
    1.0
}
fn default_avatar_scale() -> f64 {
    0.25
}

impl AvatarConfig {
    pub fn new(name: &str) -> Self {
        AvatarConfig {
            id: new_id(),
            name: name.into(),
            expressions: vec![],
            shake_factor: default_shake(),
            scale: default_avatar_scale(),
            model: String::new(),
            api_base: String::new(),
            api_key: String::new(),
        }
    }

    /// name → path map, the shape the render paths consume.
    pub fn avatars_map(&self) -> std::collections::BTreeMap<String, String> {
        self.expressions.iter().map(|e| (e.name.clone(), e.path.clone())).collect()
    }

    pub fn default_expression(&self) -> Option<&AvatarExpression> {
        self.expressions.first()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettings {
    #[serde(default = "default_lang")]
    pub whisper_language: String,
    #[serde(default = "default_whisper_model")]
    pub whisper_model: String,
    #[serde(default = "default_autosave")]
    pub autosave_secs: u32,
}

fn default_whisper_model() -> String {
    "base".into()
}

fn default_lang() -> String {
    "auto".into()
}
fn default_autosave() -> u32 {
    60
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            whisper_language: default_lang(),
            whisper_model: default_whisper_model(),
            autosave_secs: default_autosave(),
        }
    }
}

impl Project {
    /// New project with a 1920x1080@30 sequence and V1/A1 tracks.
    pub fn new(name: &str) -> Self {
        let seq = Sequence::new("Main", (1920, 1080), (30, 1));
        let seq_id = seq.id;
        Project {
            schema_version: SCHEMA_VERSION,
            id: new_id(),
            name: name.to_string(),
            created_at: String::new(), // the app layer fills it in (ue-core never touches the clock)
            settings: ProjectSettings::default(),
            assets: vec![],
            transcripts: vec![],
            avatars: vec![],
            sequences: vec![seq],
            active_sequence: seq_id,
        }
    }

    pub fn sequence(&self, id: Id) -> Option<&Sequence> {
        self.sequences.iter().find(|s| s.id == id)
    }
    pub fn sequence_mut(&mut self, id: Id) -> Option<&mut Sequence> {
        self.sequences.iter_mut().find(|s| s.id == id)
    }
    pub fn asset(&self, id: Id) -> Option<&MediaAsset> {
        self.assets.iter().find(|a| a.id == id)
    }

    /// Locates a clip by id: (seq_idx, track_idx, clip_idx).
    pub fn locate_clip(&self, id: Id) -> Option<(usize, usize, usize)> {
        for (si, s) in self.sequences.iter().enumerate() {
            for (ti, t) in s.tracks.iter().enumerate() {
                if let Some(ci) = t.clips.iter().position(|c| c.id == id) {
                    return Some((si, ti, ci));
                }
            }
        }
        None
    }

    pub fn clip(&self, id: Id) -> Option<&Clip> {
        let (si, ti, ci) = self.locate_clip(id)?;
        Some(&self.sequences[si].tracks[ti].clips[ci])
    }

    pub fn track(&self, id: Id) -> Option<&Track> {
        self.sequences.iter().flat_map(|s| s.tracks.iter()).find(|t| t.id == id)
    }
    pub fn track_mut(&mut self, id: Id) -> Option<&mut Track> {
        self.sequences.iter_mut().flat_map(|s| s.tracks.iter_mut()).find(|t| t.id == id)
    }
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub duration_us: TimeUs,
    #[serde(default)]
    pub fps: Option<(u32, u32)>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub rotation: i32,
    #[serde(default)]
    pub vcodec: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub audio_channels: u32,
    #[serde(default)]
    pub vfr: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaAsset {
    pub id: Id,
    pub kind: MediaKind,
    /// Path relative to the .uep file (or absolute until it is saved).
    pub path: String,
    pub content_hash: String,
    pub probe: ProbeInfo,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub audio_conform: Option<String>,
    #[serde(default)]
    pub peaks: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<String>,
    #[serde(default)]
    pub transcript: Option<Id>,
    #[serde(default)]
    pub offline: bool,
}

// ---------------------------------------------------------------------------
// Sequence / tracks / clips
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub id: Id,
    pub name: String,
    pub resolution: (u32, u32),
    pub fps: (u32, u32),
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub markers: Vec<Marker>,
}

fn default_sample_rate() -> u32 {
    48000
}

impl Sequence {
    pub fn new(name: &str, resolution: (u32, u32), fps: (u32, u32)) -> Self {
        Sequence {
            id: new_id(),
            name: name.to_string(),
            resolution,
            fps,
            sample_rate: 48000,
            tracks: vec![
                Track::new(TrackKind::Audio, "A1"),
                Track::new(TrackKind::Video, "V1"),
            ],
            markers: vec![],
        }
    }

    /// Duration = end of the last clip across all tracks.
    pub fn duration_us(&self) -> TimeUs {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.start + c.duration)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Id,
    pub kind: TrackKind,
    pub name: String,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub volume_db: f32,
    /// ALWAYS ordered by `start` and non-overlapping (validated invariant).
    #[serde(default)]
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn new(kind: TrackKind, name: &str) -> Self {
        Track {
            id: new_id(),
            kind,
            name: name.to_string(),
            muted: false,
            solo: false,
            locked: false,
            volume_db: 0.0,
            clips: vec![],
        }
    }

    pub fn clip_index(&self, id: Id) -> Option<usize> {
        self.clips.iter().position(|c| c.id == id)
    }

    /// Insertion index that keeps ordering by start.
    pub fn insertion_index(&self, start: TimeUs) -> usize {
        self.clips.partition_point(|c| c.start < start)
    }

    /// Does a clip [start, start+dur) collide with the existing ones (excluding `exclude`)?
    pub fn collides(&self, start: TimeUs, duration: TimeUs, exclude: Option<Id>) -> bool {
        let end = start + duration;
        self.clips.iter().any(|c| {
            Some(c.id) != exclude && c.start < end && start < c.start + c.duration
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: Id,
    pub t: TimeUs,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

// ---------------------------------------------------------------------------
// Clip
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: Id,
    pub payload: ClipPayload,
    pub start: TimeUs,
    pub duration: TimeUs,
    #[serde(default = "default_speed")]
    pub speed: f64,
    #[serde(default)]
    pub effects: Vec<EffectInstance>,
    #[serde(default)]
    pub transform: Transform2D,
    #[serde(default)]
    pub audio: AudioProps,
    #[serde(default)]
    pub transition_in: Option<TransitionRef>,
    /// Exit transition over the clip's tail (to black on the base track,
    /// to transparent on upper layers).
    #[serde(default)]
    pub transition_out: Option<TransitionRef>,
    #[serde(default)]
    pub label_color: Option<String>,
    /// Optional human-friendly name (agents/users; the UI and MCP fall back to
    /// a derived label when it's None). Not the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Linked clips (video+audio of the same media): they share a group and
    /// operations (move, split, trim, delete, speed) propagate across them.
    #[serde(default)]
    pub group: Option<Id>,
}

impl Clip {
    /// Human-friendly label: the custom name, else something derived from the
    /// payload (asset file, text content, generator…). Never the raw id.
    pub fn display_label(&self, project: &Project) -> String {
        if let Some(n) = &self.name {
            if !n.trim().is_empty() {
                return n.clone();
            }
        }
        match &self.payload {
            ClipPayload::Media { asset_id, .. } => project
                .asset(*asset_id)
                .map(|a| {
                    std::path::Path::new(&a.path)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| a.path.clone())
                })
                .unwrap_or_else(|| "media".into()),
            ClipPayload::Text { content, .. } => {
                let t = content.trim();
                if t.is_empty() {
                    "Text".into()
                } else if t.chars().count() > 24 {
                    format!("{}…", t.chars().take(24).collect::<String>())
                } else {
                    t.to_string()
                }
            }
            ClipPayload::Subtitles { .. } => "Subtitles".into(),
            ClipPayload::Generator { generator_id, .. } => generator_id.clone(),
        }
    }
}

fn default_speed() -> f64 {
    1.0
}

impl Clip {
    pub fn new_media(asset_id: Id, src_in: TimeUs, src_out: TimeUs, start: TimeUs) -> Self {
        Clip {
            id: new_id(),
            payload: ClipPayload::Media { asset_id, src_in, src_out },
            start,
            duration: src_out - src_in,
            speed: 1.0,
            effects: vec![],
            transform: Transform2D::default(),
            audio: AudioProps::default(),
            transition_in: None,
            transition_out: None,
            label_color: None,
            name: None,
            group: None,
        }
    }

    pub fn new_generator(generator_id: &str, start: TimeUs, duration: TimeUs) -> Self {
        Clip {
            id: new_id(),
            payload: ClipPayload::Generator {
                generator_id: generator_id.into(),
                params: Default::default(),
                color_params: Default::default(),
            },
            start,
            duration,
            speed: 1.0,
            effects: vec![],
            transform: Default::default(),
            audio: AudioProps { muted: true, ..Default::default() },
            transition_in: None,
            transition_out: None,
            label_color: None,
            name: None,
            group: None,
        }
    }

    pub fn new_text(content: &str, start: TimeUs, duration: TimeUs) -> Self {
        Clip {
            id: new_id(),
            payload: ClipPayload::Text { content: content.to_string(), style: TextStyle::default() },
            start,
            duration,
            speed: 1.0,
            effects: vec![],
            transform: Transform2D::default(),
            audio: AudioProps::default(),
            transition_in: None,
            transition_out: None,
            label_color: None,
            name: None,
            group: None,
        }
    }

    pub fn end(&self) -> TimeUs {
        self.start + self.duration
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ClipPayload {
    Media {
        asset_id: Id,
        /// Range of the source file in µs.
        src_in: TimeUs,
        src_out: TimeUs,
    },
    Text {
        content: String,
        style: TextStyle,
    },
    Subtitles {
        transcript_id: Id,
        style: TextStyle,
        mode: SubtitleMode,
        /// Hard cap on the words a caption may hold. `None` = fit the line to
        /// the frame width (the old behaviour). Older projects deserialize to
        /// `None`, so nothing changes for them.
        #[serde(default)]
        max_words: Option<u32>,
    },
    /// Generated content (solid rectangle, gradient, …): a generator manifest
    /// (ue-render) produces the lavfi source from the params.
    Generator {
        generator_id: String,
        #[serde(default)]
        params: std::collections::BTreeMap<String, Param>,
        #[serde(default)]
        color_params: std::collections::BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode {
    Phrase,
    Word,
    Karaoke,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_font_size")]
    pub size: f32,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub stroke_color: Option<String>,
    #[serde(default)]
    pub stroke_width: f32,
    #[serde(default)]
    pub highlight_color: Option<String>,
    #[serde(default)]
    pub x_offset: f32,
    #[serde(default)]
    pub y_offset: f32,
    #[serde(default)]
    pub align: TextAlign,
    /// Text direction for RTL languages (Persian, Arabic, Hebrew).
    #[serde(default)]
    pub direction: TextDirection,
    /// Line spacing as a multiple of the font size, for wrapped captions.
    /// Old projects deserialize to 1.2.
    #[serde(default = "default_line_height")]
    pub line_height: f32,
}

fn default_line_height() -> f32 {
    1.2
}

fn default_font() -> String {
    "sans-serif".into()
}
fn default_font_size() -> f32 {
    60.0
}
fn default_color() -> String {
    "#ffffff".into()
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font: default_font(),
            size: default_font_size(),
            color: default_color(),
            bg: None,
            stroke_color: None,
            stroke_width: 0.0,
            highlight_color: None,
            x_offset: 0.0,
            y_offset: 0.0,
            align: TextAlign::Center,
            direction: TextDirection::LTR,
            line_height: default_line_height(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TextDirection {
    #[default]
    LTR,
    RTL,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transform2D {
    pub position: (Param, Param),
    pub scale: (Param, Param),
    /// Degrees.
    pub rotation: Param,
    /// Crop left/top/right/bottom as a fraction [0, 1].
    pub crop: (Param, Param, Param, Param),
    pub opacity: Param,
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
}

impl Transform2D {
    /// Every animatable param sanitized (see `Param::sanitized`): UI gestures
    /// can emit unsorted or duplicated curve keys, which would otherwise break
    /// the strictly-increasing-keys invariant (and abort the process).
    pub fn sanitized(self) -> Transform2D {
        Transform2D {
            position: (self.position.0.sanitized(), self.position.1.sanitized()),
            scale: (self.scale.0.sanitized(), self.scale.1.sanitized()),
            rotation: self.rotation.sanitized(),
            crop: (
                self.crop.0.sanitized(),
                self.crop.1.sanitized(),
                self.crop.2.sanitized(),
                self.crop.3.sanitized(),
            ),
            opacity: self.opacity.sanitized(),
            flip_h: self.flip_h,
            flip_v: self.flip_v,
        }
    }

    /// Snapshot with all curves evaluated at `at` (µs relative to the
    /// clip). Used for the preview while scrubbing.
    pub fn sampled(&self, at: TimeUs) -> Transform2D {
        let ev = |p: &Param| Param::Const(p.eval(at));
        Transform2D {
            position: (ev(&self.position.0), ev(&self.position.1)),
            scale: (ev(&self.scale.0), ev(&self.scale.1)),
            rotation: ev(&self.rotation),
            crop: (ev(&self.crop.0), ev(&self.crop.1), ev(&self.crop.2), ev(&self.crop.3)),
            opacity: ev(&self.opacity),
            flip_h: self.flip_h,
            flip_v: self.flip_v,
        }
    }
}

impl Default for Transform2D {
    fn default() -> Self {
        Self {
            position: (0.0.into(), 0.0.into()),
            scale: (1.0.into(), 1.0.into()),
            rotation: 0.0.into(),
            crop: (0.0.into(), 0.0.into(), 0.0.into(), 0.0.into()),
            opacity: 1.0.into(),
            flip_h: false,
            flip_v: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioProps {
    pub gain_db: Param,
    pub pan: Param,
    #[serde(default)]
    pub fade_in_us: TimeUs,
    #[serde(default)]
    pub fade_out_us: TimeUs,
    #[serde(default)]
    pub muted: bool,
    /// Background-noise reduction (afftdn): live uses a denoised conform
    /// variant, export inserts the same filter in the audio chain.
    #[serde(default)]
    pub denoise: bool,
}

impl AudioProps {
    /// Every animatable param sanitized (see `Param::sanitized`).
    pub fn sanitized(self) -> AudioProps {
        AudioProps {
            gain_db: self.gain_db.sanitized(),
            pan: self.pan.sanitized(),
            ..self
        }
    }
}

impl Default for AudioProps {
    fn default() -> Self {
        Self {
            gain_db: 0.0.into(),
            pan: 0.0.into(),
            fade_in_us: 0,
            fade_out_us: 0,
            muted: false,
            denoise: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectInstance {
    pub effect_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, Param>,
    /// Color params ("#rrggbb"), kept separate from the numeric/curve ones.
    #[serde(default)]
    pub color_params: std::collections::BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionRef {
    pub effect_id: String,
    pub duration: TimeUs,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, Param>,
}

// ---------------------------------------------------------------------------
// Transcript (word-level)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptDoc {
    pub id: Id,
    pub asset_id: Id,
    pub language: String,
    pub model: String,
    #[serde(default)]
    pub words: Vec<Word>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub global_avg_volume: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_us: TimeUs,
    pub end_us: TimeUs,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub rejected: bool,
    /// User correction ("godo" → "godot"): same audio span, relabeled.
    /// `text` stays as what Whisper heard; captions show `label()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl Word {
    /// What captions and the text editor show.
    pub fn label(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.text)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub start_us: TimeUs,
    pub end_us: TimeUs,
    /// Range [from, to) in `words`.
    pub word_range: (usize, usize),
    #[serde(default)]
    pub emotion: Option<String>,
    #[serde(default)]
    pub volume_rms: f64,
}

// ---------------------------------------------------------------------------
// Project (de)serialization
// ---------------------------------------------------------------------------

impl Project {
    pub fn to_json(&self) -> Result<String, crate::UeError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(s: &str) -> Result<Project, crate::UeError> {
        #[derive(Deserialize)]
        struct VersionProbe {
            schema_version: u32,
        }
        let v: VersionProbe = serde_json::from_str(s)?;
        if v.schema_version > SCHEMA_VERSION {
            return Err(crate::UeError::SchemaVersion(v.schema_version, SCHEMA_VERSION));
        }
        // The v1→v2… migrations will go here once they exist.
        Ok(serde_json::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_json_roundtrip() {
        let mut p = Project::new("Demo");
        let seq_id = p.active_sequence;
        let track_id = p.sequence(seq_id).unwrap().tracks[1].id;
        let asset = MediaAsset {
            id: Ulid::new(),
            kind: MediaKind::Video,
            path: "media/a.mp4".into(),
            content_hash: "xxh3:abc".into(),
            probe: ProbeInfo {
                duration_us: 10_000_000,
                fps: Some((30, 1)),
                width: 1920,
                height: 1080,
                rotation: 0,
                vcodec: Some("h264".into()),
                acodec: Some("aac".into()),
                audio_channels: 2,
                vfr: false,
            },
            proxy: None,
            audio_conform: None,
            peaks: None,
            thumbnails: None,
            transcript: None,
            offline: false,
        };
        let clip = Clip::new_media(asset.id, 0, 5_000_000, 0);
        p.assets.push(asset);
        p.sequence_mut(seq_id).unwrap();
        p.track_mut(track_id).unwrap().clips.push(clip);

        let json = p.to_json().unwrap();
        let back = Project::from_json(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn future_schema_rejected() {
        let p = Project::new("X");
        let mut val: serde_json::Value = serde_json::from_str(&p.to_json().unwrap()).unwrap();
        val["schema_version"] = serde_json::json!(999);
        let err = Project::from_json(&val.to_string()).unwrap_err();
        assert!(matches!(err, crate::UeError::SchemaVersion(999, _)));
    }

    #[test]
    fn collision_detection() {
        let mut t = Track::new(TrackKind::Video, "V1");
        let a = Ulid::new();
        t.clips.push(Clip::new_media(a, 0, 1_000_000, 0)); // [0, 1s)
        assert!(t.collides(500_000, 1_000_000, None));
        assert!(!t.collides(1_000_000, 1_000_000, None)); // exactly adjacent does not collide
        let id = t.clips[0].id;
        assert!(!t.collides(0, 1_000_000, Some(id))); // excluding itself
    }
}
