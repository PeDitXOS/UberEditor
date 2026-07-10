//! Tauri backend: exposes ue-core's ProjectStore as IPC commands.
//! The frontend queries the state after each mutation (v0); `state.patch`
//! events will arrive when the data volume justifies it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, Manager, State};
use ue_audio::items::{collect_specs, load_items};
use ue_audio::player::Player;
use ue_core::model::{AudioProps, Clip, Id, MediaKind, Project, TrackKind, Transform2D};
use ue_core::ops::InsertMode;
use ue_core::{dlog, ProjectStore, TimeUs};
use ue_media::stream::MjpegSession;

pub mod mcp;

pub struct AppState {
    pub store: Mutex<ProjectStore>,
    pub path: Mutex<Option<PathBuf>>,
    pub cache_dir: Mutex<Option<PathBuf>>,
    pub player: Mutex<Option<Player>>,
    pub frames: Mutex<Option<FrameService>>,
    pub export_cancel: Arc<AtomicBool>,
    /// Effective registry (core + user packs) and raw user packs.
    pub registry: Mutex<Arc<Vec<ue_render::EffectDef>>>,
    pub user_packs: Mutex<Vec<ue_render::EffectDef>>,
    pub effects_dir: Mutex<Option<PathBuf>>,
    pub mcp_port: Mutex<Option<u16>>,
    pub mcp_shutdown: AtomicBool,
    pub mcp_token: Mutex<String>,
    pub models_dir: Mutex<Option<PathBuf>>,
    /// Timeline visual caches (per asset).
    pub peaks_cache: Mutex<std::collections::HashMap<Id, Arc<Vec<f32>>>>,
    pub thumbs_cache: Mutex<std::collections::HashMap<Id, ue_media::thumbs::ThumbStrip>>,
    /// Background jobs for slow MCP tools (transcribe/export/avatar): an agent
    /// gets a job id immediately and polls, so the client never times out on a
    /// blocking call. Keyed by job id, newest kept.
    pub jobs: Mutex<std::collections::HashMap<String, Job>>,
}

/// State of one long-running background job.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    /// "transcribe" | "export" | "avatar".
    pub kind: String,
    /// "running" | "done" | "error".
    pub status: String,
    /// 0..1 (best effort; not every job reports fine-grained progress).
    pub progress: f64,
    pub message: String,
    /// The tool result once `status == "done"`.
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl Job {
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "job_id": self.id,
            "kind": self.kind,
            "status": self.status,
            "progress": self.progress,
            "message": self.message,
            "result": self.result,
            "error": self.error,
        })
    }
}

/// Registers a new running job and returns its id (a fresh ULID).
pub fn job_start(state: &AppState, kind: &str, message: &str) -> String {
    let id = Id::new().to_string();
    let job = Job {
        id: id.clone(),
        kind: kind.to_string(),
        status: "running".into(),
        progress: 0.0,
        message: message.to_string(),
        result: None,
        error: None,
    };
    let mut jobs = state.jobs.lock().unwrap();
    // keep the map from growing without bound across a long session
    if jobs.len() > 64 {
        let done: Vec<String> =
            jobs.values().filter(|j| j.status != "running").map(|j| j.id.clone()).collect();
        for k in done {
            jobs.remove(&k);
        }
    }
    jobs.insert(id.clone(), job);
    id
}

/// Updates a job's progress/message (no-op if it's gone).
pub fn job_progress(state: &AppState, id: &str, progress: f64, message: &str) {
    if let Some(j) = state.jobs.lock().unwrap().get_mut(id) {
        j.progress = progress.clamp(0.0, 1.0);
        if !message.is_empty() {
            j.message = message.to_string();
        }
    }
}

/// Marks a job finished (done with a result, or error).
pub fn job_finish(state: &AppState, id: &str, result: Result<serde_json::Value, String>) {
    if let Some(j) = state.jobs.lock().unwrap().get_mut(id) {
        match result {
            Ok(v) => {
                j.status = "done".into();
                j.progress = 1.0;
                j.result = Some(v);
                j.message = "done".into();
            }
            Err(e) => {
                j.status = "error".into();
                j.message = e.clone();
                j.error = Some(e);
            }
        }
    }
}

impl AppState {
    /// Initial state (also used by the MCP server tests).
    pub fn new_default() -> Self {
        AppState {
            store: Mutex::new(ProjectStore::new(Project::new("Untitled project"))),
            path: Mutex::new(None),
            cache_dir: Mutex::new(None),
            player: Mutex::new(None),
            frames: Mutex::new(None),
            export_cancel: Arc::new(AtomicBool::new(false)),
            registry: Mutex::new(Arc::new(ue_render::core_registry())),
            user_packs: Mutex::new(vec![]),
            effects_dir: Mutex::new(None),
            mcp_port: Mutex::new(None),
            mcp_shutdown: AtomicBool::new(false),
            mcp_token: Mutex::new(Id::new().to_string().to_lowercase()),
            models_dir: Mutex::new(None),
            peaks_cache: Mutex::new(std::collections::HashMap::new()),
            thumbs_cache: Mutex::new(std::collections::HashMap::new()),
            jobs: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// Reloads the user packs from disk and rebuilds the registry.
/// Returns errors from invalid manifests (they break nothing).
pub(crate) fn reload_packs(state: &AppState) -> Vec<String> {
    let dir = state.effects_dir.lock().unwrap().clone();
    let (user, errors) = match dir {
        Some(d) => ue_render::load_packs_from_dir(&d),
        None => (vec![], vec![]),
    };
    let merged = ue_render::merge_registries(ue_render::core_registry(), user.clone());
    *state.user_packs.lock().unwrap() = user;
    *state.registry.lock().unwrap() = Arc::new(merged);
    errors
}

/// Playback frame service: a thread follows the audio clock with a persistent
/// MJPEG session and publishes the latest decoded frame.
pub struct FrameService {
    pub latest: Arc<Mutex<Vec<u8>>>,
    pub running: Arc<AtomicBool>,
}

const PLAYBACK_FPS: u32 = 24;
const PLAYBACK_MAX_W: u32 = 960;

/// Stable identity of a playback stream: same clip content + composition ⇒
/// same key across ticks (unlike the vf string, whose graph labels are
/// unique per build). Pure and unit-tested.
pub fn playback_session_key(
    r: &ue_media::frame::ResolvedFrame,
    canvas: Option<(u32, u32)>,
    avatar_canonical: Option<&str>,
) -> String {
    format!(
        "{}|{}|{:?}|{}|{}",
        r.asset_path,
        r.speed,
        canvas,
        serde_json::to_string(&(&r.effects, &r.transform)).unwrap_or_default(),
        avatar_canonical.unwrap_or(""),
    )
}

/// Production session-reuse rule (also exercised by the playback tests):
/// same file, same composition key, and the requested source time reachable
/// by the running stream (up to 400 ms behind for shuttle-reverse, up to
/// 1.5 s ahead to let the decoder catch up).
pub fn should_reuse_session(
    session: Option<(&Path, i64)>,
    key_matches: bool,
    path: &Path,
    src_t: i64,
) -> bool {
    session.is_some_and(|(spath, next_src)| {
        spath == path
            && key_matches
            && src_t >= next_src - 400_000
            && src_t <= next_src + 1_500_000
    })
}

fn frame_service_loop(app: tauri::AppHandle, latest: Arc<Mutex<Vec<u8>>>, running: Arc<AtomicBool>) {
    let mut session: Option<MjpegSession> = None;
    // Session identity = the DATA that defines the stream, never the vf
    // string: transform_vf embeds unique graph labels (p0fg, p1fg, …) so the
    // string differs on every build — comparing it reopened the session on
    // every tick (field bug: black playback whenever a transform was active).
    let mut session_key: Option<String> = None;
    while running.load(Ordering::SeqCst) {
        let state = app.state::<AppState>();
        let (t, playing) = {
            let guard = state.player.lock().unwrap();
            match guard.as_ref() {
                Some(p) => (p.position_us(), p.is_playing()),
                None => (0, false),
            }
        };
        if !playing {
            break;
        }
        let resolved = {
            let store = state.store.lock().unwrap();
            ue_media::frame::resolve_top_video(
                &store.project,
                store.project.active_sequence,
                t,
            )
        };
        let Some(r) = resolved else {
            latest.lock().unwrap().clear();
            session = None;
            std::thread::sleep(Duration::from_millis(40));
            continue;
        };
        let path = PathBuf::from(&r.asset_path);
        let src_t = r.src_t_us;
        let reg = state.registry.lock().unwrap().clone();
        let canvas = {
            let store = state.store.lock().unwrap();
            store
                .project
                .sequence(store.project.active_sequence)
                .map(|s| s.resolution)
        };
        // canonical vf (tvar="t") only to COMPARE: it does not change with the playhead
        let (av_canonical, av_open) = {
            let store = state.store.lock().unwrap();
            let seq_id = store.project.active_sequence;
            (
                avatar_vf_stream_suffix(&store.project, seq_id, 0, 1.0, PLAYBACK_MAX_W),
                // tl0 = timeline position at this tick (t=0 of the new session)
                avatar_vf_stream_suffix(&store.project, seq_id, t, r.speed, PLAYBACK_MAX_W),
            )
        };
        let key = playback_session_key(&r, canvas, av_canonical.as_deref());

        // is the current session usable? (same file, same effect chain,
        // position reachable going forward)
        let reusable = should_reuse_session(
            session.as_ref().map(|s| (s.asset_path.as_path(), s.next_src_us())),
            session_key.as_deref() == Some(key.as_str()),
            &path,
            src_t,
        );
        if !reusable {
            let reason = match session.as_ref() {
                None => "no session".to_string(),
                Some(s) if s.asset_path != path => "clip changed".to_string(),
                Some(_) if session_key.as_deref() != Some(key.as_str()) => {
                    "effects/transform changed".to_string()
                }
                Some(s) => format!(
                    "seek (stream at {:.3}s)",
                    s.next_src_us() as f64 / 1e6
                ),
            };
            dlog(
                "frame",
                &format!(
                    "open session {} @ {:.3}s ({reason})",
                    path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
                    src_t as f64 / 1e6,
                ),
            );
            // the stream runs with -ss: t=0 at the open point, at SOURCE
            // rate → clip time = t/speed + offset_at_open. This way the
            // transform curves ANIMATE during playback too.
            let rel0 = r.clip_rel_us as f64 / 1_000_000.0;
            let tvar = if (r.speed - 1.0).abs() > 1e-9 {
                format!("(t/{}+{rel0:.6})", r.speed)
            } else {
                format!("(t+{rel0:.6})")
            };
            let mut open_vf = ue_render::clip_vf_at(&reg, &r.effects, &r.transform, canvas, &tvar);
            if let Some(av) = &av_open {
                open_vf = Some(format!("{}{}", open_vf.as_deref().unwrap_or("null"), av));
            }
            session =
                MjpegSession::open(&path, src_t, PLAYBACK_MAX_W, PLAYBACK_FPS, open_vf.as_deref())
                    .ok();
            session_key = Some(key);
        }
        if let Some(s) = session.as_mut() {
            let mut newest: Option<Vec<u8>> = None;
            let mut dead = false;
            while s.next_src_us() <= src_t {
                match s.next_frame() {
                    Ok(Some(f)) => newest = Some(f),
                    _ => {
                        dead = true;
                        break;
                    }
                }
            }
            if dead {
                session = None;
            }
            if let Some(f) = newest {
                *latest.lock().unwrap() = f;
            }
        }
        std::thread::sleep(Duration::from_millis(1000 / PLAYBACK_FPS as u64 / 2));
    }
    running.store(false, Ordering::SeqCst);
}

fn start_frame_service(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let mut guard = state.frames.lock().unwrap();
    if let Some(fs) = guard.as_ref() {
        if fs.running.load(Ordering::SeqCst) {
            return; // already running
        }
    }
    let latest = Arc::new(Mutex::new(Vec::new()));
    let running = Arc::new(AtomicBool::new(true));
    *guard = Some(FrameService { latest: latest.clone(), running: running.clone() });
    let app2 = app.clone();
    std::thread::spawn(move || frame_service_loop(app2, latest, running));
}

fn stop_frame_service(state: &AppState) {
    if let Some(fs) = state.frames.lock().unwrap().as_ref() {
        fs.running.store(false, Ordering::SeqCst);
    }
}

/// Autosave path: next to the .uep if it exists, otherwise in app_data.
fn autosave_path(project_path: Option<&Path>, data_dir: Option<&Path>) -> Option<PathBuf> {
    match project_path {
        Some(p) => Some(p.with_extension("uep.autosave")),
        None => data_dir.map(|d| d.join("recovery.uep.autosave")),
    }
}

/// Autosave thread: every `settings.autosave_secs`, if there are unsaved
/// changes it writes a recovery copy (atomic, portable).
fn autosave_loop(app: tauri::AppHandle, data_dir: Option<PathBuf>) {
    let mut last_version: u64 = 0;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let state = app.state::<AppState>();
        let (dirty, version, secs, project_path) = {
            let store = state.store.lock().unwrap();
            (
                store.dirty,
                store.version,
                store.project.settings.autosave_secs.max(10) as u64,
                state.path.lock().unwrap().clone(),
            )
        };
        // respect the configured cadence by sampling every 5 s
        static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let t = TICKS.fetch_add(5, Ordering::Relaxed) + 5;
        if t % secs.max(5) >= 5 || !dirty || version == last_version {
            continue;
        }
        let Some(target) = autosave_path(project_path.as_deref(), data_dir.as_deref()) else {
            continue;
        };
        let json = {
            let store = state.store.lock().unwrap();
            let portable = make_portable(&store.project, target.parent());
            portable.to_json()
        };
        if let Ok(json) = json {
            let tmp = target.with_extension("autosave.tmp");
            if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &target).is_ok() {
                last_version = version;
            }
        }
    }
}

/// Project copy ready for disk: media paths relative to the .uep when
/// possible and WITHOUT local caches (they belong to this machine).
pub fn make_portable(project: &Project, dir: Option<&Path>) -> Project {
    let mut portable = project.clone();
    for asset in &mut portable.assets {
        if let Some(dir) = dir {
            let p = Path::new(&asset.path);
            if p.is_absolute() {
                if let Ok(rel) = p.strip_prefix(dir) {
                    asset.path = rel.to_string_lossy().into_owned();
                }
            }
        }
        asset.proxy = None;
        asset.audio_conform = None;
        asset.peaks = None;
        asset.thumbnails = None;
    }
    portable
}

/// Resolves relative paths against the project folder and marks offline.
pub fn resolve_project_paths(project: &mut Project, dir: Option<&Path>) {
    for asset in &mut project.assets {
        let p = Path::new(&asset.path);
        if !p.is_absolute() {
            if let Some(d) = dir {
                asset.path = d.join(p).to_string_lossy().into_owned();
            }
        }
        asset.offline = !Path::new(&asset.path).exists();
    }
}

/// Path of an asset's conformed WAV in the app cache.
fn conform_target(cache_dir: &Path, content_hash: &str) -> PathBuf {
    cache_dir.join(content_hash.replace(':', "-")).join("audio.wav")
}

/// Syncs the mixer items with the current state (if it changed).
/// Lock order ALWAYS: store → player.
fn sync_player(state: &AppState) -> Result<(), String> {
    let store = state.store.lock().unwrap();
    let mut player_guard = state.player.lock().unwrap();
    if player_guard.is_none() {
        *player_guard = Some(Player::new().map_err(|e| e.to_string())?);
    }
    let player = player_guard.as_ref().unwrap();
    // version+1 to distinguish from the player's initial 0
    if player.items_version() != store.version + 1 {
        let specs = collect_specs(&store.project, store.project.active_sequence);
        let (items, _skipped) =
            load_items(&store.project, &specs, |a| a.audio_conform.as_ref().map(PathBuf::from));
        player.set_items(items, store.version + 1);
    }
    Ok(())
}

#[derive(Serialize)]
pub struct StateSnapshot {
    pub project: Project,
    pub version: u64,
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub undo_labels: Vec<String>,
}

fn snapshot(store: &ProjectStore) -> StateSnapshot {
    StateSnapshot {
        project: store.project.clone(),
        version: store.version,
        dirty: store.dirty,
        can_undo: store.can_undo(),
        can_redo: store.can_redo(),
        undo_labels: store.undo_labels().iter().map(|s| s.to_string()).collect(),
    }
}

fn parse_id(s: &str) -> Result<Id, String> {
    s.parse::<Id>().map_err(|e| format!("invalid id '{s}': {e}"))
}

type Res<T> = Result<T, String>;

/// Transcription defaults. Not undoable (settings, not an edit).
pub(crate) fn set_project_settings_impl(
    state: &AppState,
    whisper_language: Option<String>,
    whisper_model: Option<String>,
) {
    let mut store = state.store.lock().unwrap();
    if let Some(lang) = whisper_language {
        store.project.settings.whisper_language = lang;
    }
    if let Some(model) = whisper_model {
        store.project.settings.whisper_model = model;
    }
    store.version += 1;
    store.dirty = true;
}

#[tauri::command]
fn set_project_settings(
    state: State<AppState>,
    whisper_language: String,
    whisper_model: String,
) -> Res<StateSnapshot> {
    set_project_settings_impl(&state, Some(whisper_language), Some(whisper_model));
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Frontend log bridge: UI errors/warnings become terminal lines so the
/// user can copy-paste them when reporting bugs.
#[tauri::command]
fn ui_log(level: String, message: String) {
    dlog(&format!("ui:{level}"), &message);
}

#[tauri::command]
fn get_state(state: State<AppState>) -> Res<StateSnapshot> {
    Ok(snapshot(&state.store.lock().unwrap()))
}

#[tauri::command]
fn split_clip(state: State<AppState>, clip_id: String, t_us: TimeUs) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.split_clip(parse_id(&clip_id)?, t_us).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn delete_clips(state: State<AppState>, ids: Vec<String>, ripple: bool) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let ids: Result<Vec<Id>, String> = ids.iter().map(|s| parse_id(s)).collect();
    store.delete_clips(&ids?, ripple).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn move_clip(
    state: State<AppState>,
    clip_id: String,
    to_track: String,
    to_start_us: TimeUs,
    overwrite: bool,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let mode = if overwrite { InsertMode::Overwrite } else { InsertMode::Strict };
    store
        .move_clip(parse_id(&clip_id)?, parse_id(&to_track)?, to_start_us, mode)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn trim_clip(
    state: State<AppState>,
    clip_id: String,
    left: bool,
    new_edge_us: TimeUs,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.trim_clip(parse_id(&clip_id)?, left, new_edge_us).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn cut_ranges(
    state: State<AppState>,
    sequence_id: String,
    ranges: Vec<(TimeUs, TimeUs)>,
    ripple: bool,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store
        .cut_ranges(parse_id(&sequence_id)?, &ranges, ripple)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// System fonts (family, path) for the text picker.
#[tauri::command]
fn list_fonts() -> Vec<(String, String)> {
    ue_export::graph::list_system_fonts()
}

fn templates_path(state: &AppState) -> Option<PathBuf> {
    state.effects_dir.lock().unwrap().as_ref().map(|d| {
        d.parent().unwrap_or(d).join("text_templates.json")
    })
}

/// Saved title templates (name → style). An empty object when there are none.
pub(crate) fn text_templates(state: &AppState) -> serde_json::Value {
    templates_path(state)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Saved title templates (name → style).
#[tauri::command]
fn list_text_templates(state: State<AppState>) -> Res<serde_json::Value> {
    Ok(text_templates(&state))
}

#[tauri::command]
fn save_text_template(
    state: State<AppState>,
    name: String,
    style: ue_core::model::TextStyle,
) -> Res<serde_json::Value> {
    let Some(path) = templates_path(&state) else { return Err("no config folder".into()) };
    let mut all: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    all.insert(name, serde_json::to_value(style).map_err(|e| e.to_string())?);
    std::fs::write(&path, serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::Value::Object(all))
}

/// Breaks a clip's video↔audio link (its whole group, 1 undo).
pub(crate) fn unlink_clip_impl(state: &AppState, clip_id: Id) -> Res<usize> {
    let mut store = state.store.lock().unwrap();
    let members = ue_core::ops::linked_ids(&store.project, clip_id);
    if members.len() < 2 {
        return Err("the clip is not linked".into());
    }
    let n = members.len();
    let actions = members
        .into_iter()
        .map(|clip_id| ue_core::Action::SetClipGroup { clip_id, group: None })
        .collect();
    store.dispatch("Unlink clips", actions).map_err(|e| e.to_string())?;
    Ok(n)
}

#[tauri::command]
fn unlink_clip(state: State<AppState>, clip_id: String) -> Res<StateSnapshot> {
    unlink_clip_impl(&state, parse_id(&clip_id)?)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

#[tauri::command]
fn set_subtitles_props(
    state: State<AppState>,
    clip_id: String,
    style: ue_core::model::TextStyle,
    mode: ue_core::model::SubtitleMode,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Edit subtitles",
            vec![ue_core::Action::SetClipSubtitles { clip_id: id, style, mode }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Changes a clip's speed (rate stretch, pitch preserved on export).
#[tauri::command]
fn set_clip_speed(state: State<AppState>, clip_id: String, speed: f64) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.set_clip_speed(parse_id(&clip_id)?, speed).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Moves a timeline range to another point (all tracks, 1 undo).
#[tauri::command]
fn move_range(
    state: State<AppState>,
    sequence_id: String,
    from_us: TimeUs,
    to_us: TimeUs,
    dest_us: TimeUs,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store
        .move_range(parse_id(&sequence_id)?, from_us, to_us, dest_us)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn undo(state: State<AppState>) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.undo().map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn redo(state: State<AppState>) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.redo().map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Renders the denoised variant of a clip's conformed audio in the background
/// (the mixer picks it up on the next `state-changed` resync). No-op when the
/// clip is not media, the conform is missing, or the variant already exists.
///
/// Callers that batch several actions into ONE undo entry dispatch first and
/// then call this: it never touches the history.
pub(crate) fn spawn_denoise_job(app: &tauri::AppHandle, state: &AppState, clip_id: Id) {
    let Some(conform) = ({
        let store = state.store.lock().unwrap();
        store
            .project
            .clip(clip_id)
            .and_then(|c| match &c.payload {
                ue_core::model::ClipPayload::Media { asset_id, .. } => Some(*asset_id),
                _ => None,
            })
            .and_then(|aid| store.project.asset(aid))
            .and_then(|a| a.audio_conform.clone())
    }) else {
        return;
    };
    let conform = PathBuf::from(conform);
    if ue_media::denoise::denoised_path(&conform).exists() {
        return;
    }
    // self-contained: the app provisions its own denoiser venv under its data
    // dir on first use (a system python3/python is required)
    let env_dir = state
        .models_dir
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.parent().map(|d| d.join("denoiser")));
    let app = app.clone();
    std::thread::spawn(move || {
        match ue_media::denoise::denoise_wav(&conform, env_dir.as_deref(), true) {
            Ok(_) => {
                // bump the version so sync_player rebuilds the items
                let state = app.state::<AppState>();
                state.store.lock().unwrap().version += 1;
                let _ = app.emit("state-changed", ());
            }
            Err(e) => eprintln!("[denoise] {conform:?}: {e}"),
        }
    });
}

/// Writes a clip's audio props and, when denoise is on, renders the denoised
/// conform variant in the background. `app` is `None` for headless callers,
/// which then only get the denoise on export.
pub(crate) fn set_clip_audio_impl(
    app: Option<&tauri::AppHandle>,
    state: &AppState,
    id: Id,
    audio: AudioProps,
) -> Res<()> {
    let wants_denoise = audio.denoise;
    state
        .store
        .lock()
        .unwrap()
        .dispatch_coalesced("Edit audio", vec![ue_core::Action::SetClipAudio { clip_id: id, audio }])
        .map_err(|e| e.to_string())?;
    if let (true, Some(app)) = (wants_denoise, app) {
        spawn_denoise_job(app, state, id);
    }
    Ok(())
}

#[tauri::command]
fn set_clip_audio(
    app: tauri::AppHandle,
    state: State<AppState>,
    clip_id: String,
    audio: AudioProps,
) -> Res<StateSnapshot> {
    set_clip_audio_impl(Some(&app), &state, parse_id(&clip_id)?, audio)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

#[tauri::command]
fn set_clip_transform(
    state: State<AppState>,
    clip_id: String,
    transform: Transform2D,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch_coalesced(
            "Edit transform",
            vec![ue_core::Action::SetClipTransform { clip_id: id, transform }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Imports files into the pool (probe + hash). Does not enter the history (PLAN §6.10).
/// The audio conform runs in the background; when done it emits
/// `state-changed` so the UI refreshes.
///
/// Returns the asset id of every path (existing id when the content was
/// already imported: importing twice is idempotent, keyed by content hash).
/// `app` is `None` in tests and headless callers: the conform/proxy jobs are
/// then skipped, so `audio_conform` stays empty until the app imports it.
pub(crate) fn import_media_impl(
    app: Option<&tauri::AppHandle>,
    state: &AppState,
    paths: &[String],
) -> Res<Vec<Id>> {
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    let mut store = state.store.lock().unwrap();
    let mut errors: Vec<String> = vec![];
    let mut ids: Vec<Id> = vec![];
    for p in paths {
        match ue_media::import_file(Path::new(p)) {
            Ok(asset) => {
                // re-import of the same content → don't duplicate
                match store.project.assets.iter().find(|a| a.content_hash == asset.content_hash) {
                    Some(existing) => ids.push(existing.id),
                    None => {
                        if let (Some(app), Some(cache)) = (app, &cache_dir) {
                            if asset.probe.audio_channels > 0 {
                                spawn_conform_job(app, &asset, cache);
                            }
                            spawn_proxy_job(app, &asset, cache);
                        }
                        ids.push(asset.id);
                        store.project.assets.push(asset);
                    }
                }
            }
            Err(e) => errors.push(format!("{p}: {e}")),
        }
    }
    if !ids.is_empty() {
        store.version += 1;
        store.dirty = true;
    }
    if ids.is_empty() && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(ids)
}

#[tauri::command]
fn import_media(
    app: tauri::AppHandle,
    state: State<AppState>,
    paths: Vec<String>,
) -> Res<StateSnapshot> {
    import_media_impl(Some(&app), &state, &paths)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

fn spawn_conform_job(app: &tauri::AppHandle, asset: &ue_core::model::MediaAsset, cache: &Path) {
    let app = app.clone();
    let asset_id = asset.id;
    let src = PathBuf::from(&asset.path);
    let out = conform_target(cache, &asset.content_hash);
    std::thread::spawn(move || {
        match ue_media::conform_audio(&src, &out) {
            Ok(()) => {
                let state = app.state::<AppState>();
                {
                    let mut store = state.store.lock().unwrap();
                    if let Some(a) = store.project.assets.iter_mut().find(|a| a.id == asset_id) {
                        a.audio_conform = Some(out.to_string_lossy().into_owned());
                    }
                    store.version += 1;
                }
                let _ = app.emit("state-changed", ());
            }
            Err(e) => eprintln!("[conform] {src:?}: {e}"),
        }
    });
}

/// Generates the preview proxy in the background for large videos.
fn spawn_proxy_job(app: &tauri::AppHandle, asset: &ue_core::model::MediaAsset, cache: &Path) {
    // only worth it if the original is wider than the proxy
    if asset.kind != ue_core::model::MediaKind::Video
        || asset.probe.width <= ue_media::proxy::PROXY_MAX_W
    {
        return;
    }
    let app = app.clone();
    let asset_id = asset.id;
    let src = PathBuf::from(&asset.path);
    let cache = cache.to_path_buf();
    let hash = asset.content_hash.clone();
    std::thread::spawn(move || {
        match ue_media::proxy::generate_proxy(&src, &cache, &hash) {
            Ok(out) => {
                let state = app.state::<AppState>();
                {
                    let mut store = state.store.lock().unwrap();
                    if let Some(a) = store.project.assets.iter_mut().find(|a| a.id == asset_id) {
                        a.proxy = Some(out.to_string_lossy().into_owned());
                    }
                    store.version += 1;
                }
                // the FrameService detects the path change and reopens the session
                let _ = app.emit("state-changed", ());
            }
            Err(e) => eprintln!("[proxy] {src:?}: {e}"),
        }
    });
}

// ---- transport (audio is the master clock) ----

#[tauri::command]
fn playback_play(app: tauri::AppHandle, state: State<AppState>, from_us: TimeUs) -> Res<()> {
    playback_play_impl(&state, Some(&app), from_us)
}

/// Shared by the playback_play command and the MCP debug tool.
pub fn playback_play_impl(
    state: &AppState,
    app: Option<&tauri::AppHandle>,
    from_us: TimeUs,
) -> Res<()> {
    dlog("play", &format!("play from {:.3}s", from_us as f64 / 1e6));
    sync_player(state)?;
    {
        let guard = state.player.lock().unwrap();
        guard.as_ref().ok_or("no player")?.play(from_us);
    }
    match app {
        Some(a) => start_frame_service(a),
        None => return Err("no app handle (headless)".into()),
    }
    Ok(())
}

#[tauri::command]
fn playback_pause(state: State<AppState>) -> Res<TimeUs> {
    stop_frame_service(&state);
    let guard = state.player.lock().unwrap();
    match guard.as_ref() {
        Some(p) => Ok(p.pause()),
        None => Err("no player".into()),
    }
}

/// Latest frame of the playback stream (empty = no signal yet).
#[tauri::command]
fn playback_frame(state: State<AppState>) -> Res<tauri::ipc::Response> {
    let bytes = match state.frames.lock().unwrap().as_ref() {
        Some(fs) => fs.latest.lock().unwrap().clone(),
        None => vec![],
    };
    Ok(tauri::ipc::Response::new(bytes))
}

/// Real audio peaks of the asset (25 bins/s, mono mix), for the timeline
/// waveform. Caches in memory and on disk next to the conform.
#[tauri::command]
async fn get_audio_peaks(state: State<'_, AppState>, asset_id: String) -> Res<Vec<f32>> {
    let id: Id = asset_id.parse().map_err(|_| "invalid id")?;
    if let Some(p) = state.peaks_cache.lock().unwrap().get(&id) {
        return Ok(p.as_ref().clone());
    }
    let conform = {
        let store = state.store.lock().unwrap();
        store
            .project
            .asset(id)
            .ok_or("asset not found")?
            .audio_conform
            .clone()
            .ok_or("audio not conformed yet")?
    };
    let peaks = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<f32>, String> {
        let disk = PathBuf::from(format!("{conform}.peaks"));
        if let Ok(bytes) = std::fs::read(&disk) {
            return Ok(bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect());
        }
        let wav =
            ue_audio::wav::WavMap::open(Path::new(&conform)).map_err(|e| e.to_string())?;
        let peaks = ue_audio::wav::compute_peaks(&wav, 25);
        let bytes: Vec<u8> = peaks.iter().flat_map(|f| f.to_le_bytes()).collect();
        let _ = std::fs::write(&disk, bytes); // best-effort cache
        Ok(peaks)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.peaks_cache.lock().unwrap().insert(id, Arc::new(peaks.clone()));
    Ok(peaks)
}

/// Generates (or returns from cache) the asset's thumbnail strip.
#[tauri::command]
async fn ensure_thumbs(
    state: State<'_, AppState>,
    asset_id: String,
) -> Res<ue_media::thumbs::ThumbStrip> {
    let id: Id = asset_id.parse().map_err(|_| "invalid id")?;
    if let Some(t) = state.thumbs_cache.lock().unwrap().get(&id) {
        return Ok(t.clone());
    }
    let (src, dur, hash, cache_dir) = {
        let store = state.store.lock().unwrap();
        let asset = store.project.asset(id).ok_or("asset not found")?;
        if asset.kind == ue_core::model::MediaKind::Audio {
            return Err("audio assets have no thumbnails".into());
        }
        let cache = state.cache_dir.lock().unwrap().clone().ok_or("no cache")?;
        (
            PathBuf::from(&asset.path),
            asset.probe.duration_us.max(1_000_000),
            asset.content_hash.clone(),
            cache,
        )
    };
    let strip = tauri::async_runtime::spawn_blocking(move || {
        ue_media::thumbs::generate_thumb_strip(&src, dur, &cache_dir, &hash)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    state.thumbs_cache.lock().unwrap().insert(id, strip.clone());
    Ok(strip)
}

/// JPEG bytes of the already-generated thumbnail strip.
#[tauri::command]
fn get_thumb_strip(state: State<AppState>, asset_id: String) -> Res<tauri::ipc::Response> {
    let id: Id = asset_id.parse().map_err(|_| "invalid id")?;
    let path = state
        .thumbs_cache
        .lock()
        .unwrap()
        .get(&id)
        .map(|t| t.path.clone())
        .ok_or("thumbnails not generated (call ensure_thumbs)")?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Shuttle JKL: sets the playback rate (negative = reverse).
/// If it wasn't playing, starts from the given playhead.
#[tauri::command]
fn playback_set_rate(
    app: tauri::AppHandle,
    state: State<AppState>,
    rate: f64,
    from_us: TimeUs,
) -> Res<()> {
    sync_player(&state)?;
    {
        let guard = state.player.lock().unwrap();
        let p = guard.as_ref().ok_or("no player")?;
        if !p.is_playing() {
            p.play(from_us);
        }
        p.set_rate(rate);
    }
    start_frame_service(&app);
    Ok(())
}

#[tauri::command]
fn playback_seek(state: State<AppState>, t_us: TimeUs) -> Res<()> {
    dlog("play", &format!("seek to {:.3}s", t_us as f64 / 1e6));
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.seek(t_us);
    }
    Ok(())
}

/// (position µs, playing). Also re-syncs the items if the project changed
/// during playback (editing while it plays).
#[tauri::command]
fn playback_position(state: State<AppState>) -> Res<(TimeUs, bool, f32, f32)> {
    let _ = sync_player(&state); // cheap if the version didn't change
    let guard = state.player.lock().unwrap();
    match guard.as_ref() {
        Some(p) => {
            let (ml, mr) = p.meters();
            Ok((p.position_us(), p.is_playing(), ml, mr))
        }
        None => Err("no player".into()),
    }
}

/// Adds a clip of the asset to the first compatible track: at `at_us` if it
/// fits, otherwise at the end of the track.
#[tauri::command]
fn add_clip(state: State<AppState>, asset_id: String, at_us: TimeUs) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let asset_id = parse_id(&asset_id)?;
    let asset = store
        .project
        .asset(asset_id)
        .ok_or_else(|| format!("asset {asset_id} does not exist"))?
        .clone();
    let duration = ue_media::default_clip_duration(&asset);
    if duration <= 0 {
        return Err("the file has no usable duration".into());
    }
    let at = at_us.max(0);

    // video WITH audio → linked pair: video clip (embedded audio muted) on a
    // video track + a separate audio clip on an audio track, same group, so
    // they behave as one (split/move/trim/speed/delete propagate).
    if asset.kind == MediaKind::Video && asset.probe.audio_channels > 0 {
        let vtrack = ensure_free_video_track(&mut store, at, duration)?;
        let atrack = ensure_free_audio_track(&mut store, at, duration)?;
        let group = Id::new();
        let mut vclip = Clip::new_media(asset.id, 0, duration, at);
        vclip.audio.muted = true;
        vclip.group = Some(group);
        let mut aclip = Clip::new_media(asset.id, 0, duration, at);
        aclip.group = Some(group);
        store
            .dispatch(
                "Add clip",
                vec![
                    ue_core::Action::InsertClip { track_id: vtrack, clip: vclip },
                    ue_core::Action::InsertClip { track_id: atrack, clip: aclip },
                ],
            )
            .map_err(|e| e.to_string())?;
        return Ok(snapshot(&store));
    }

    // single clip (audio-only, image, or video without audio)
    let want_kind = if asset.kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video };
    let track_id = if want_kind == TrackKind::Audio {
        ensure_free_audio_track(&mut store, at, duration)?
    } else {
        ensure_free_video_track(&mut store, at, duration)?
    };
    let clip = Clip::new_media(asset.id, 0, duration, at);
    store
        .insert_clip(track_id, clip, InsertMode::Strict)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Overlay of the active avatar at `t` for the paused frame: -vf suffix with
/// movie+overlay (static, no shake: it's a still frame).
fn avatar_vf_suffix(
    project: &Project,
    seq_id: Id,
    t_us: TimeUs,
    out_w: u32,
) -> Option<String> {
    use ue_core::model::{ClipPayload, TrackKind};
    let seq = project.sequence(seq_id)?;
    for track in seq.tracks.iter().rev().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            let ClipPayload::Avatar { driver_asset, avatars, scale, .. } = &clip.payload else {
                continue;
            };
            if !(clip.start <= t_us && t_us < clip.end()) {
                continue;
            }
            let default = avatars.keys().next()?.clone();
            // emotion of the active segment (or default)
            let emotion = project
                .transcripts
                .iter()
                .find(|d| d.asset_id == *driver_asset)
                .and_then(|doc| {
                    doc.segments.iter().find_map(|seg| {
                        let tl = crate::asset_tl(project, seq, *driver_asset, seg.start_us)?;
                        let end = tl + (seg.end_us - seg.start_us);
                        (tl <= t_us && t_us < end).then(|| seg.emotion.clone()).flatten()
                    })
                })
                .unwrap_or(default.clone());
            let path = avatars.get(&emotion).or_else(|| avatars.get(&default))?;
            if !Path::new(path).exists() {
                return None;
            }
            let aw = (((out_w as f64) * scale.clamp(0.05, 1.0)) as u32) & !1;
            let escaped = path.replace('\\', "/").replace(':', "\\\\:").replace('\'', "\\\\'");
            // scale the main BEFORE the overlay so aw is proportional
            return Some(format!(
                ",scale='min({out_w},iw)':-2[main];movie=filename='{escaped}',\
                 scale={aw}:-2[av];[main][av]overlay=W-w-16:H-h-16"
            ));
        }
    }
    None
}

/// Avatar overlays for the playback STREAM. Unlike the export (one overlay
/// per segment), it groups by EMOTION — a single `movie` instance per avatar
/// video — and turns each one on with `enable` windows expressed in the
/// session's t domain: t runs in SOURCE seconds from the -ss point, so
/// timeline = tl0 + t/speed ⇒ a timeline window [from,to] is
/// [(from-tl0)·speed, (to-tl0)·speed] in t. With `tl0_us=0, speed=1` it
/// produces the CANONICAL form (stable across ticks) that the FrameService
/// uses to decide whether to reopen the session.
pub fn avatar_vf_stream_suffix(
    project: &Project,
    seq_id: Id,
    tl0_us: TimeUs,
    speed: f64,
    out_w: u32,
) -> Option<String> {
    use std::collections::BTreeMap;
    use ue_core::model::{ClipPayload, TrackKind};
    let seq = project.sequence(seq_id)?;
    for track in seq.tracks.iter().rev().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            let ClipPayload::Avatar { driver_asset, avatars, scale, .. } = &clip.payload else {
                continue;
            };
            let default = avatars.keys().next()?.clone();
            // spans (emotion, from_tl, to_tl) filling gaps with the default
            let ce = clip.end();
            let mut spans: Vec<(String, TimeUs, TimeUs)> = vec![];
            let mut cursor = clip.start;
            if let Some(doc) = project.transcripts.iter().find(|d| d.asset_id == *driver_asset)
            {
                for seg in &doc.segments {
                    let Some(tl) = asset_tl(project, seq, *driver_asset, seg.start_us) else {
                        continue;
                    };
                    let from = tl.max(clip.start);
                    let to = (tl + (seg.end_us - seg.start_us)).min(ce);
                    if to <= from {
                        continue;
                    }
                    if from > cursor {
                        spans.push((default.clone(), cursor, from));
                    }
                    spans.push((seg.emotion.clone().unwrap_or_else(|| default.clone()), from, to));
                    cursor = to.max(cursor);
                }
            }
            if cursor < ce {
                spans.push((default.clone(), cursor, ce));
            }
            // group windows by avatar video (emotions with no video → default)
            let mut windows: BTreeMap<String, Vec<(TimeUs, TimeUs)>> = BTreeMap::new();
            for (emotion, from, to) in spans {
                let key = if avatars.contains_key(&emotion) { emotion } else { default.clone() };
                windows.entry(key).or_default().push((from, to));
            }
            let aw = (((out_w as f64) * scale.clamp(0.05, 1.0)) as u32) & !1;
            let mut out = format!(",scale='min({out_w},iw)':-2[avm0]");
            let mut stage = 0usize;
            let n_total = windows.len();
            for (i, (emotion, wins)) in windows.iter().enumerate() {
                let Some(path) = avatars.get(emotion) else { continue };
                if !Path::new(path).exists() {
                    return None;
                }
                let escaped =
                    path.replace('\\', "/").replace(':', "\\\\:").replace('\'', "\\\\'");
                let enable = wins
                    .iter()
                    .map(|(f, t)| {
                        format!(
                            "between(t,{:.4},{:.4})",
                            ((f - tl0_us) as f64 / 1e6 * speed).max(0.0),
                            ((t - tl0_us) as f64 / 1e6 * speed).max(0.0),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("+");
                // loop=0 = infinite; monotonic setpts so overlay doesn't stall
                out.push_str(&format!(
                    ";movie=filename='{escaped}':loop=0,setpts=N/(FRAME_RATE*TB),\
                     scale={aw}:-2[ave{stage}]"
                ));
                let dst = if i + 1 == n_total {
                    String::new()
                } else {
                    format!("[avm{}]", stage + 1)
                };
                out.push_str(&format!(
                    ";[avm{stage}][ave{stage}]overlay=W-w-16:H-h-16:enable='{enable}'{dst}"
                ));
                stage += 1;
            }
            if stage == 0 {
                return None;
            }
            return Some(out);
        }
    }
    None
}

/// Asset time → timeline (helper shared with avatar_vf_suffix).
pub(crate) fn asset_tl(
    _project: &Project,
    seq: &ue_core::model::Sequence,
    asset_id: Id,
    t_asset: TimeUs,
) -> Option<TimeUs> {
    use ue_core::model::ClipPayload;
    for track in &seq.tracks {
        for clip in &track.clips {
            if let ClipPayload::Media { asset_id: aid, src_in, src_out } = &clip.payload {
                if *aid == asset_id && t_asset >= *src_in && t_asset < *src_out {
                    return Some(clip.start + (t_asset - src_in));
                }
            }
        }
    }
    None
}

/// Real JPEG frame at the given time (raw bytes; empty = no signal).
/// Shared by the render_frame command and the MCP debug tool.
pub fn render_frame_impl(state: &AppState, t_us: TimeUs, max_width: u32) -> Res<Vec<u8>> {
    let (project, seq_id, base_dir) = {
        let store = state.store.lock().unwrap();
        let base = state
            .path
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        (store.project.clone(), store.project.active_sequence, base)
    }; // release the lock before invoking ffmpeg
    let reg = state.registry.lock().unwrap().clone();
    let canvas = project.sequence(seq_id).map(|s| s.resolution);
    // animated scrub: evaluate transform AND effect curves at the clip time
    let mut vf = ue_media::frame::resolve_top_video(&project, seq_id, t_us).and_then(|r| {
        ue_render::clip_vf_sampled(&reg, &r.effects, &r.transform, canvas, r.clip_rel_us)
    });
    // titles + subtitles active at t_us, built from the EXACT export chain so
    // the paused preview matches what export burns in (golden rule). They live
    // in canvas coordinates, so when no transform already fit the frame to the
    // canvas we fit it here before drawing, then the outer scale downsizes it.
    let text = project.sequence(seq_id).and_then(|seq| {
        ue_export::graph::text_overlays_at(&project, seq, seq.resolution.1, seq.resolution.0, t_us)
    });
    if text.is_some() && vf.is_none() {
        if let Some((cw, ch)) = canvas {
            vf = Some(format!(
                "scale={cw}:{ch}:force_original_aspect_ratio=decrease,\
                 pad={cw}:{ch}:(ow-iw)/2:(oh-ih)/2:color=black"
            ));
        }
    }
    // avatar over the paused frame (movie+overlay graph in the same -vf)
    if let Some(suffix) = avatar_vf_suffix(&project, seq_id, t_us, max_width) {
        vf = Some(format!("{}{}", vf.unwrap_or_else(|| "null".into()), suffix));
    }
    // text last, on top (same order as the export)
    if let Some(text) = text {
        vf = Some(format!("{},{text}", vf.unwrap_or_else(|| "null".into())));
    }
    let t0 = std::time::Instant::now();
    let result =
        ue_media::frame::render_frame(&project, seq_id, t_us, max_width, &base_dir, vf.as_deref());
    let ms = t0.elapsed().as_millis();
    let bytes = match result {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => {
            dlog("frame", &format!("render_frame @ {:.3}s FAILED: {e}", t_us as f64 / 1e6));
            return Err(e.to_string());
        }
    };
    if ms > 400 {
        dlog("frame", &format!("slow render_frame @ {:.3}s: {ms} ms", t_us as f64 / 1e6));
    }
    Ok(bytes)
}

#[tauri::command]
fn render_frame(
    state: State<AppState>,
    t_us: TimeUs,
    max_width: u32,
) -> Res<tauri::ipc::Response> {
    render_frame_impl(&state, t_us, max_width).map(tauri::ipc::Response::new)
}

/// Catalog of available effects (for the UI and MCP).
#[tauri::command]
fn get_effects_catalog(state: State<AppState>) -> serde_json::Value {
    ue_render::catalog_json(&state.registry.lock().unwrap())
}

/// Reloads the user packs from disk (effects/ folder in the config).
#[tauri::command]
fn reload_effect_packs(state: State<AppState>) -> Res<serde_json::Value> {
    let errors = reload_packs(&state);
    Ok(serde_json::json!({
        "catalog": ue_render::catalog_json(&state.registry.lock().unwrap()),
        "errors": errors,
        "dir": state.effects_dir.lock().unwrap().as_ref().map(|d| d.display().to_string()),
    }))
}

#[tauri::command]
fn set_clip_effects(
    state: State<AppState>,
    clip_id: String,
    effects: Vec<ue_core::model::EffectInstance>,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch_coalesced(
            "Edit effects",
            vec![ue_core::Action::SetClipEffects { clip_id: id, effects }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Duplicates a sequence with new IDs (ids are globally unique).
fn duplicate_sequence(seq: &ue_core::model::Sequence) -> ue_core::model::Sequence {
    let mut copy = seq.clone();
    copy.id = Id::new();
    for track in &mut copy.tracks {
        track.id = Id::new();
        for clip in &mut track.clips {
            clip.id = Id::new();
        }
    }
    for marker in &mut copy.markers {
        marker.id = Id::new();
    }
    copy
}

/// Generates the vertical version (1080x1920) of the active sequence: a new
/// sequence with the core.vertical_fill effect on each video clip (the
/// blurred-background look from the toolkit). A single undo entry.
pub(crate) fn generate_vertical_impl(state: &AppState) -> Result<String, String> {
    use ue_core::model::{ClipPayload, EffectInstance};
    let mut store = state.store.lock().unwrap();
    let seq = store
        .project
        .sequence(store.project.active_sequence)
        .ok_or("no active sequence")?;
    // already portrait? refuse instead of stacking "(Vertical) (Vertical)"
    if seq.resolution.1 > seq.resolution.0 {
        return Err("the active sequence is already vertical".into());
    }
    // a vertical twin already exists → just switch to it
    let twin_name = format!("{} (Vertical)", seq.name);
    if let Some(existing) = store.project.sequences.iter().find(|s| s.name == twin_name) {
        let id = existing.id;
        store
            .dispatch(
                "Switch to vertical",
                vec![ue_core::Action::SetActiveSequence { sequence_id: id }],
            )
            .map_err(|e| e.to_string())?;
        return Ok(id.to_string());
    }
    let mut vertical = duplicate_sequence(seq);
    vertical.name = twin_name;
    vertical.resolution = (seq.resolution.1, seq.resolution.0);
    for track in vertical.tracks.iter_mut().filter(|t| t.kind == TrackKind::Video) {
        for clip in &mut track.clips {
            if matches!(clip.payload, ClipPayload::Media { .. }) {
                clip.effects.push(EffectInstance {
                    effect_id: "core.vertical_fill".into(),
                    enabled: true,
                    params: Default::default(),
                    color_params: Default::default(),
                });
            }
        }
    }
    let new_id = vertical.id;
    store
        .dispatch(
            "Generate vertical",
            vec![
                ue_core::Action::AddSequence { sequence: vertical },
                ue_core::Action::SetActiveSequence { sequence_id: new_id },
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(new_id.to_string())
}

#[tauri::command]
fn generate_vertical(state: State<AppState>) -> Res<StateSnapshot> {
    generate_vertical_impl(&state)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Delete a sequence (never the last one). If it is active, switches to the
/// first remaining sequence first. Single undo entry.
#[tauri::command]
fn remove_sequence(state: State<AppState>, sequence_id: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&sequence_id)?;
    if store.project.sequences.len() <= 1 {
        return Err("cannot delete the last sequence".into());
    }
    if store.project.sequence(id).is_none() {
        return Err("sequence not found".into());
    }
    let mut actions = vec![];
    if store.project.active_sequence == id {
        let fallback = store
            .project
            .sequences
            .iter()
            .find(|s| s.id != id)
            .map(|s| s.id)
            .ok_or("no remaining sequence")?;
        actions.push(ue_core::Action::SetActiveSequence { sequence_id: fallback });
    }
    actions.push(ue_core::Action::RemoveSequence { sequence_id: id });
    store.dispatch("Delete sequence", actions).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Sequence resolution/frame rate (undoable). 4K, portrait, 60 fps, etc.
#[tauri::command]
fn set_sequence_props(
    state: State<AppState>,
    sequence_id: String,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&sequence_id)?;
    store
        .dispatch_coalesced(
            "Sequence settings",
            vec![ue_core::Action::SetSequenceProps {
                sequence_id: id,
                resolution: (width.clamp(16, 8192) & !1, height.clamp(16, 8192) & !1),
                fps: (fps_num.clamp(1, 240), fps_den.clamp(1, 1001)),
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Correct one transcribed word ("godo" → "godot"): same audio, new label.
/// Empty text reverts to the original. Undoable.
#[tauri::command]
fn set_word_text(
    state: State<AppState>,
    transcript_id: String,
    index: usize,
    text: String,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&transcript_id)?;
    let display = if text.trim().is_empty() { None } else { Some(text.trim().to_string()) };
    store
        .dispatch(
            "Correct word",
            vec![ue_core::Action::SetWordText { transcript_id: id, index, display }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Replace every whole-word occurrence (case-insensitive, against the shown
/// label) in a transcript. One undo entry. Returns the count.
#[tauri::command]
fn replace_words(
    state: State<AppState>,
    transcript_id: String,
    from: String,
    to: String,
) -> Res<serde_json::Value> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&transcript_id)?;
    let needle = from.trim().to_lowercase();
    if needle.is_empty() {
        return Err("nothing to search".into());
    }
    let doc = store
        .project
        .transcripts
        .iter()
        .find(|t| t.id == id)
        .ok_or("transcript not found")?;
    let matches: Vec<usize> = doc
        .words
        .iter()
        .enumerate()
        .filter(|(_, w)| w.label().trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase() == needle
            || w.label().trim().to_lowercase() == needle)
        .map(|(i, _)| i)
        .collect();
    if matches.is_empty() {
        return Ok(serde_json::json!({ "replaced": 0, "snapshot": snapshot(&store) }));
    }
    let display = if to.trim().is_empty() { None } else { Some(to.trim().to_string()) };
    let actions: Vec<ue_core::Action> = matches
        .iter()
        .map(|i| ue_core::Action::SetWordText {
            transcript_id: id,
            index: *i,
            display: display.clone(),
        })
        .collect();
    let n = actions.len();
    store.dispatch("Replace words", actions).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "replaced": n, "snapshot": snapshot(&store) }))
}

#[tauri::command]
fn set_active_sequence(state: State<AppState>, sequence_id: String) -> Res<StateSnapshot> {
    let id = parse_id(&sequence_id)?;
    let mut store = state.store.lock().unwrap();
    store
        .dispatch(
            "Change sequence",
            vec![ue_core::Action::SetActiveSequence { sequence_id: id }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Creates an auto-subtitles clip over a transcribed media clip.
/// Returns the id of the new subtitles clip.
pub(crate) fn add_subtitles_clip_impl(state: &AppState, id: Id) -> Res<Id> {
    use ue_core::model::{ClipPayload, SubtitleMode, TextStyle};
    let mut store = state.store.lock().unwrap();
    let media = store.project.clip(id).ok_or("clip not found")?.clone();
    let ClipPayload::Media { asset_id, .. } = media.payload else {
        return Err("the clip is not media".into());
    };
    let transcript_id = store
        .project
        .transcripts
        .iter()
        .find(|t| t.asset_id == asset_id)
        .map(|t| t.id)
        .ok_or("the media has no transcript; transcribe it first (T button)")?;
    let track_id = ensure_free_video_track(&mut store, media.start, media.duration)?;
    // lower third at 1080p
    let style = TextStyle { size: 48.0, y_offset: 380.0, ..Default::default() };
    let clip = Clip {
        id: ue_core::model::Id::new(),
        payload: ClipPayload::Subtitles { transcript_id, style, mode: SubtitleMode::Phrase },
        start: media.start,
        duration: media.duration,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
        name: None,
        group: None,
    };
    let clip_id = clip.id;
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(clip_id)
}

#[tauri::command]
fn add_subtitles_clip(state: State<AppState>, clip_id: String) -> Res<StateSnapshot> {
    add_subtitles_clip_impl(&state, parse_id(&clip_id)?)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Creates an Avatar clip over a transcribed media clip, from a config.json
/// compatible with the Youtubers-toolkit avatar_config. Classifies emotions
/// (OpenAI-compatible API if OPENAI_API_KEY is set, otherwise offline
/// heuristic) and measures volumes per segment.
/// All avatar setups stored in the project.
#[tauri::command]
fn list_avatar_configs(state: State<AppState>) -> Vec<ue_core::model::AvatarConfig> {
    state.store.lock().unwrap().project.avatars.clone()
}

/// Create or update an avatar setup (undoable, persisted with the project).
/// A draft with a missing or empty `id` gets a fresh one. Returns the id.
pub(crate) fn save_avatar_config_impl(state: &AppState, config: serde_json::Value) -> Res<Id> {
    // a brand-new draft arrives with id:"" — mint one before deserializing
    let mut config = config;
    let fresh = config.get("id").and_then(|v| v.as_str()).map(|s| s.is_empty()).unwrap_or(true);
    if fresh {
        config["id"] = serde_json::json!(Id::new().to_string());
    }
    let mut config: ue_core::model::AvatarConfig =
        serde_json::from_value(config).map_err(|e| format!("invalid avatar setup: {e}"))?;
    if config.expressions.is_empty() {
        return Err("add at least one expression".into());
    }
    if config.expressions.iter().any(|e| e.name.trim().is_empty()) {
        return Err("every expression needs a name".into());
    }
    if config.id.is_nil() {
        config.id = Id::new();
    }
    let id = config.id;
    state
        .store
        .lock()
        .unwrap()
        .dispatch("Save avatar", vec![ue_core::Action::UpsertAvatarConfig { config }])
        .map_err(|e| e.to_string())?;
    Ok(id)
}

#[tauri::command]
fn save_avatar_config(
    state: State<AppState>,
    config: serde_json::Value,
) -> Res<(String, StateSnapshot)> {
    let id = save_avatar_config_impl(&state, config)?;
    Ok((id.to_string(), snapshot(&state.store.lock().unwrap())))
}

#[tauri::command]
fn remove_avatar_config(state: State<AppState>, config_id: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&config_id)?;
    store
        .dispatch("Delete avatar", vec![ue_core::Action::RemoveAvatarConfig { config_id: id }])
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Pick expression media files (images or videos).
#[tauri::command]
async fn pick_avatar_media(app: tauri::AppHandle) -> Res<Vec<String>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .add_filter("Images and videos", &["png", "jpg", "jpeg", "webp", "gif", "mp4", "mov", "webm", "mkv"])
        .pick_files(move |paths| {
            let _ = tx.send(paths);
        });
    let picked = rx.recv().map_err(|e| e.to_string())?;
    Ok(picked
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect())
}

/// Export an avatar setup as a standalone JSON (toolkit-compatible shape,
/// plus our extra fields). The API key is NEVER written to the file.
/// Writes an avatar setup to a shareable JSON. The `api_key` is deliberately
/// NOT serialized: a setup can be shared without leaking the user's key.
pub(crate) fn export_avatar_config_impl(state: &AppState, id: Id, path: &str) -> Res<String> {
    let store = state.store.lock().unwrap();
    let cfg = store
        .project
        .avatars
        .iter()
        .find(|c| c.id == id)
        .ok_or("avatar config not found")?;
    let json = serde_json::json!({
        "name": cfg.name,
        "avatars": cfg.avatars_map(),
        "expressions": cfg.expressions,
        "shake_factor": cfg.shake_factor,
        "scale": cfg.scale,
        "model": cfg.model,
        "api_base": cfg.api_base,
    });
    std::fs::write(path, serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(path.to_string())
}

#[tauri::command]
fn export_avatar_config(state: State<AppState>, config_id: String, path: String) -> Res<String> {
    export_avatar_config_impl(&state, parse_id(&config_id)?, &path)
}

/// Import an avatar setup from JSON: ours or the toolkit's config.json.
/// Imports an avatar setup. Returns the id of the setup so the UI can select
/// it. Re-importing a setup with the same NAME replaces it instead of piling
/// up duplicates.
pub(crate) fn import_avatar_config_impl(state: &AppState, path: &str) -> Res<Id> {
    use ue_core::model::{AvatarConfig, AvatarExpression};
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let base = Path::new(path).parent().unwrap_or(Path::new("."));
    let resolve = |p: &str| -> Option<String> {
        let pp = Path::new(p);
        for cand in [pp.to_path_buf(), base.join(pp), base.join(pp.file_name()?)] {
            if cand.exists() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
        None
    };

    // the toolkit's config.json has no name: fall back to its folder
    // ("avatar_config" → "avatar config"), so two different setups don't
    // both land as "Imported avatar" and collide on re-import
    let fallback_name = base
        .file_name()
        .map(|f| f.to_string_lossy().replace(['_', '-'], " "))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Imported avatar".to_string());
    let mut cfg = AvatarConfig::new(
        v.get("name").and_then(|n| n.as_str()).unwrap_or(&fallback_name),
    );
    // our richer format first
    if let Some(list) = v.get("expressions").and_then(|e| e.as_array()) {
        for e in list {
            let (Some(name), Some(p)) = (
                e.get("name").and_then(|n| n.as_str()),
                e.get("path").and_then(|n| n.as_str()),
            ) else {
                continue;
            };
            if let Some(path) = resolve(p) {
                cfg.expressions.push(AvatarExpression {
                    name: name.to_string(),
                    path,
                    description: e
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
    }
    // toolkit shape: { "avatars": { emotion: path } }
    if cfg.expressions.is_empty() {
        for (name, p) in v.get("avatars").and_then(|a| a.as_object()).ok_or("no avatars in file")? {
            if let Some(path) = p.as_str().and_then(resolve) {
                cfg.expressions.push(AvatarExpression {
                    name: name.clone(),
                    path,
                    description: String::new(),
                });
            }
        }
    }
    if cfg.expressions.is_empty() {
        return Err("no expression file from the config exists on disk".into());
    }
    if let Some(s) = v.get("shake_factor").and_then(|x| x.as_f64()) {
        cfg.shake_factor = s;
    }
    if let Some(s) = v.get("scale").and_then(|x| x.as_f64()) {
        cfg.scale = s;
    }
    cfg.model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default().to_string();
    cfg.api_base = v.get("api_base").and_then(|m| m.as_str()).unwrap_or_default().to_string();

    let mut store = state.store.lock().unwrap();
    // same name → update it in place (Upsert matches on id)
    if let Some(existing) = store.project.avatars.iter().find(|c| c.name == cfg.name) {
        cfg.id = existing.id;
    }
    let id = cfg.id;
    store
        .dispatch("Import avatar", vec![ue_core::Action::UpsertAvatarConfig { config: cfg }])
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Import an avatar setup from JSON: ours or the toolkit's config.json.
/// Returns the id of the setup so the UI can select it. Re-importing a setup
/// with the same NAME replaces it instead of piling up duplicates.
#[tauri::command]
fn import_avatar_config(state: State<AppState>, path: String) -> Res<(String, StateSnapshot)> {
    let id = import_avatar_config_impl(&state, &path)?;
    Ok((id.to_string(), snapshot(&state.store.lock().unwrap())))
}

/// Renders the avatar video on the CALLING thread and imports it as media.
/// Steps: measure per-segment volume → classify each segment's emotion (LLM if
/// configured, offline heuristic otherwise) → render with ffmpeg → import.
/// Returns the id of the generated media asset.
///
/// The driver is the VOICE: only the asset's transcript and conformed audio
/// matter, never its video.
pub(crate) fn avatar_generate_blocking(
    state: &AppState,
    config_id: Id,
    driver_asset: Id,
    emit: &dyn Fn(&str, f64, String),
) -> Res<Id> {
    let (config, mut doc, conform, seq_size, seq_fps, cache_dir) = {
        let store = state.store.lock().unwrap();
        let config = store
            .project
            .avatars
            .iter()
            .find(|c| c.id == config_id)
            .ok_or("avatar setup not found")?
            .clone();
        if config.expressions.is_empty() {
            return Err("the avatar has no expressions".into());
        }
        let doc = store
            .project
            .transcripts
            .iter()
            .find(|t| t.asset_id == driver_asset)
            .ok_or("transcribe the driver asset first (transcribe_asset)")?
            .clone();
        let asset = store.project.asset(driver_asset).ok_or("asset not found")?;
        let conform = asset.audio_conform.clone();
        let seq = store
            .project
            .sequence(store.project.active_sequence)
            .ok_or("no active sequence")?;
        (
            config,
            doc,
            conform,
            seq.resolution,
            seq.fps,
            state.cache_dir.lock().unwrap().clone().ok_or("no cache dir")?,
        )
    };

    emit("analyzing", 0.05, "Measuring volume per segment…".into());
    if let Some(c) = &conform {
        if let Ok(wav) = ue_audio::wav::WavMap::open(Path::new(c)) {
            ue_ai::emotion::measure_volumes(&mut doc, &wav);
        }
    }

    // classify each segment: LLM when configured, heuristic otherwise
    let described: Vec<(String, String)> = config
        .expressions
        .iter()
        .map(|e| (e.name.clone(), e.description.clone()))
        .collect();
    let labels: Vec<String> = described.iter().map(|(n, _)| n.clone()).collect();
    let api_key = if config.api_key.is_empty() {
        std::env::var("OPENAI_API_KEY").unwrap_or_default()
    } else {
        config.api_key.clone()
    };
    let api_base = if config.api_base.is_empty() {
        "https://api.openai.com/v1".to_string()
    } else {
        config.api_base.clone()
    };
    let use_api = !config.model.is_empty() && !api_key.is_empty();
    let total = doc.segments.len().max(1);
    let avg = doc.global_avg_volume;
    let segs: Vec<(String, f64, f64)> = doc
        .segments
        .iter()
        .map(|s| (s.text.clone(), s.volume_rms, (s.end_us - s.start_us) as f64 / 1e6))
        .collect();
    for (i, seg) in doc.segments.iter_mut().enumerate() {
        emit(
            "classifying",
            0.05 + 0.55 * (i as f64 / total as f64),
            format!("Emotion {}/{}", i + 1, total),
        );
        let (text, vol, secs) = &segs[i];
        let emotion = if use_api {
            ue_ai::emotion::classify_via_api_described(
                &api_base,
                &api_key,
                &config.model,
                text,
                &described,
            )
        } else {
            None
        };
        seg.emotion = Some(emotion.unwrap_or_else(|| {
            let wps = text.split_whitespace().count() as f64 / secs.max(0.1);
            ue_ai::emotion::classify_heuristic(*vol, avg, wps, &labels)
        }));
    }

    emit("rendering", 0.65, "Rendering the avatar video…".into());
    let out = cache_dir.join(format!("avatar_{}_{}.mov", config.id, driver_asset));
    let duration = doc.segments.last().map(|s| s.end_us).unwrap_or(0).max(1_000_000);
    ue_export::avatar_gen::generate(&config, &doc, duration, seq_size, seq_fps, &out, |_p| {})
        .map_err(|e| e.to_string())?;

    emit("importing", 0.95, "Adding it to Media…".into());
    let asset = ue_media::import_file(&out).map_err(|e| format!("could not import: {e}"))?;
    let mut store = state.store.lock().unwrap();
    let asset_id = match store.project.assets.iter().find(|a| a.content_hash == asset.content_hash)
    {
        Some(existing) => existing.id,
        None => {
            let id = asset.id;
            store.project.assets.push(asset);
            id
        }
    };
    store.version += 1;
    store.dirty = true;
    emit("done", 1.0, "Avatar ready in Media".into());
    Ok(asset_id)
}

/// Generates the avatar video in the BACKGROUND and imports it as media.
/// Emits "avatar-progress" ({stage, progress, message}) and "state-changed".
#[tauri::command]
fn generate_avatar_video(
    app: tauri::AppHandle,
    config_id: String,
    driver_asset: String,
) -> Res<()> {
    let cfg_id = parse_id(&config_id)?;
    let asset_id = parse_id(&driver_asset)?;
    std::thread::spawn(move || {
        let emit = |stage: &str, progress: f64, message: String| {
            let _ = app.emit(
                "avatar-progress",
                serde_json::json!({ "stage": stage, "progress": progress, "message": message }),
            );
        };
        let state = app.state::<AppState>();
        match avatar_generate_blocking(&state, cfg_id, asset_id, &emit) {
            Ok(_) => {
                let _ = app.emit("state-changed", ());
            }
            Err(e) => {
                dlog("avatar", &format!("generation failed: {e}"));
                emit("error", 1.0, e);
            }
        }
    });
    Ok(())
}

#[tauri::command]
fn add_avatar_clip(state: State<AppState>, clip_id: String, config_path: String) -> Res<StateSnapshot> {
    use ue_core::model::ClipPayload;
    let id = parse_id(&clip_id)?;

    // 1. parse the toolkit config
    let raw = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let cfg: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let base = Path::new(&config_path).parent().unwrap_or(Path::new("."));
    let mut avatars = std::collections::BTreeMap::new();
    for (emotion, p) in cfg
        .get("avatars")
        .and_then(|v| v.as_object())
        .ok_or("config without 'avatars' map")?
    {
        let path = p.as_str().ok_or("invalid avatar path")?;
        let abs = {
            let pp = Path::new(path);
            if pp.is_absolute() { pp.to_path_buf() } else { base.join(pp.file_name().unwrap_or_default()) }
        };
        // the toolkit config uses paths like "avatar_config/x.mp4": try as-is and by basename
        let candidate = if abs.exists() { abs } else { base.join(path) };
        if candidate.exists() {
            avatars.insert(emotion.clone(), candidate.to_string_lossy().into_owned());
        }
    }
    if avatars.is_empty() {
        return Err("no avatar file from the config exists on disk".into());
    }
    let shake_factor = cfg.get("shake_factor").and_then(|v| v.as_f64()).unwrap_or(1.0);

    let mut store = state.store.lock().unwrap();
    let media = store.project.clip(id).ok_or("clip not found")?.clone();
    let ClipPayload::Media { asset_id, .. } = media.payload else {
        return Err("the clip is not media".into());
    };
    let conform = store
        .project
        .asset(asset_id)
        .and_then(|a| a.audio_conform.clone())
        .ok_or("the audio is still being prepared (conform)")?;

    // 2. analysis: volumes + emotions over the existing transcript
    {
        let doc = store
            .project
            .transcripts
            .iter_mut()
            .find(|t| t.asset_id == asset_id)
            .ok_or("the media has no transcript; transcribe it first (T button)")?;
        let wav = ue_audio::wav::WavMap::open(Path::new(&conform)).map_err(|e| e.to_string())?;
        ue_ai::emotion::measure_volumes(doc, &wav);
        let api = ue_ai::emotion::ApiConfig::from_env();
        ue_ai::emotion::classify_segments(doc, &avatars, api.as_ref());
        store.version += 1;
    }

    // 3. Avatar clip in a free video track (created if needed)
    let track_id = ensure_free_video_track(&mut store, media.start, media.duration)?;
    let clip = Clip {
        id: Id::new(),
        payload: ClipPayload::Avatar {
            driver_asset: asset_id,
            avatars,
            shake_factor,
            scale: 0.3,
        },
        start: media.start,
        duration: media.duration,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
        name: None,
        group: None,
    };
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Everything Whisper needs, read under one lock: (conform wav, models dir,
/// model name, language). Fails early with an actionable message.
fn transcribe_plan(
    state: &AppState,
    id: Id,
    model: Option<String>,
) -> Res<(PathBuf, PathBuf, String, Option<String>)> {
    let store = state.store.lock().unwrap();
    let asset = store.project.asset(id).ok_or("asset not found")?;
    if asset.probe.audio_channels == 0 {
        return Err("the file has no audio".into());
    }
    let conform = asset
        .audio_conform
        .clone()
        .ok_or("the audio is still being prepared (conform); try again in a few seconds")?;
    let models = state.models_dir.lock().unwrap().clone().ok_or("no models folder")?;
    let model_name = model.unwrap_or_else(|| store.project.settings.whisper_model.clone());
    let lang = store.project.settings.whisper_language.clone();
    let lang = (lang != "auto").then_some(lang);
    Ok((PathBuf::from(conform), models, model_name, lang))
}

/// Stores a finished transcript on the asset (replacing any previous one).
fn transcribe_commit(state: &AppState, asset_id: Id, mut doc: ue_core::model::TranscriptDoc) -> Id {
    let mut store = state.store.lock().unwrap();
    // Re-transcribing must KEEP the transcript id: existing subtitles clips
    // reference it, and minting a new id would leave them dangling (they'd
    // render nothing). Reuse the old id, then replace the doc in place.
    if let Some(old) = store.project.transcripts.iter().find(|t| t.asset_id == asset_id) {
        doc.id = old.id;
    }
    let doc_id = doc.id;
    store.project.transcripts.retain(|t| t.asset_id != asset_id);
    store.project.transcripts.push(doc);
    if let Some(a) = store.project.assets.iter_mut().find(|a| a.id == asset_id) {
        a.transcript = Some(doc_id);
    }
    store.version += 1;
    store.dirty = true;
    doc_id
}

/// Transcribes on the CALLING thread and returns (transcript_id, words).
/// Used by the MCP server, where an agent wants the result, not an event.
/// Downloads the ggml model on first use, so the first call can take minutes.
pub(crate) fn transcribe_blocking(
    state: &AppState,
    asset_id: Id,
    model: Option<String>,
) -> Res<(Id, usize)> {
    let (conform, models_dir, model_name, lang) = transcribe_plan(state, asset_id, model)?;
    let doc = ue_whisper::ensure_model(&models_dir, &model_name)
        .and_then(|m| ue_whisper::transcribe(&conform, &m, lang.as_deref(), asset_id))
        .map_err(|e| e.to_string())?;
    let words = doc.words.len();
    Ok((transcribe_commit(state, asset_id, doc), words))
}

/// Transcribes an asset with Whisper (word-level) in the background.
/// Downloads the ggml model if needed. Emits state-changed when done.
#[tauri::command]
fn transcribe_asset(
    app: tauri::AppHandle,
    state: State<AppState>,
    asset_id: String,
    model: Option<String>,
) -> Res<()> {
    let id = parse_id(&asset_id)?;
    let (conform, models_dir, model_name, lang) = transcribe_plan(&state, id, model)?;
    std::thread::spawn(move || {
        let result = ue_whisper::ensure_model(&models_dir, &model_name)
            .and_then(|m| ue_whisper::transcribe(&conform, &m, lang.as_deref(), id));
        let state = app.state::<AppState>();
        match result {
            Ok(doc) => {
                transcribe_commit(&state, id, doc);
            }
            Err(e) => eprintln!("[whisper] {conform:?}: {e}"),
        }
        let _ = app.emit("state-changed", ());
    });
    Ok(())
}

/// Removes the silences from a clip (cuts and closes gaps on ALL tracks: a
/// single undo entry). Requires the conformed audio.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn remove_silences(
    state: State<AppState>,
    clip_id: String,
    mode: Option<String>,
    threshold_db: Option<f64>,
    min_silence_ms: Option<i64>,
    pad_ms: Option<i64>,
) -> Res<serde_json::Value> {
    let id = parse_id(&clip_id)?;
    let mut store = state.store.lock().unwrap();
    let clip = store.project.clip(id).ok_or("clip not found")?.clone();
    let ue_core::model::ClipPayload::Media { asset_id, src_in, src_out } = clip.payload else {
        return Err("the clip is not media".into());
    };
    let asset = store.project.asset(asset_id).ok_or("asset not found")?;
    let conform = asset
        .audio_conform
        .clone()
        .ok_or("the audio is still being prepared (conform); try again in a few seconds")?;
    let wav = ue_audio::wav::WavMap::open(Path::new(&conform)).map_err(|e| e.to_string())?;
    let mut params = ue_ai::silence::SilenceParams::default();
    if let Some(db) = threshold_db {
        params.threshold_db = db.clamp(-80.0, -10.0);
    }
    if let Some(ms) = min_silence_ms {
        params.min_silence_us = (ms.clamp(50, 5000)) * 1000;
    }
    if let Some(ms) = pad_ms {
        params.pad_pre_us = (ms.clamp(0, 1000)) * 1000;
        params.pad_post_us = (ms.clamp(0, 1000)) * 1000;
    }
    let ranges =
        ue_ai::silence::clip_silences_on_timeline(&wav, clip.start, src_in, src_out, &params);
    if ranges.is_empty() {
        return Ok(serde_json::json!({ "removed": 0, "removed_us": 0, "snapshot": snapshot(&store) }));
    }
    let removed_us: i64 = ranges.iter().map(|(s, e)| e - s).sum();
    let seq_id = store.project.active_sequence;
    match mode.as_deref() {
        Some("speedup") => {
            store.speedup_ranges(seq_id, &ranges, 4.0).map_err(|e| e.to_string())?;
        }
        Some("split") => {
            // segment only: split at every silence edge, delete nothing
            store.split_ranges(seq_id, &ranges).map_err(|e| e.to_string())?;
        }
        _ => {
            store.cut_ranges(seq_id, &ranges, true).map_err(|e| e.to_string())?;
        }
    }
    Ok(serde_json::json!({
        "removed": ranges.len(),
        "removed_us": removed_us,
        "snapshot": snapshot(&store),
    }))
}

/// Video track with a free gap in [start, start+duration); if none exists,
/// adds a new track on top (not within the caller's SAME transaction:
/// it's dispatched separately with its own undo grouped by the label).
/// First unlocked audio track with room at [start, start+duration), or a new
/// A(n+1) if none. Mirror of ensure_free_video_track.
fn ensure_free_audio_track(
    store: &mut ProjectStore,
    start: TimeUs,
    duration: TimeUs,
) -> Result<Id, String> {
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("no active sequence")?;
    if let Some(t) = seq
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio && !t.locked && !t.collides(start, duration, None))
    {
        return Ok(t.id);
    }
    let n = seq.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count();
    let track = ue_core::model::Track::new(TrackKind::Audio, &format!("A{}", n + 1));
    let track_id = track.id;
    // keep audio tracks grouped at the start of the vec
    let index = seq
        .tracks
        .iter()
        .rposition(|t| t.kind == TrackKind::Audio)
        .map(|i| i + 1)
        .unwrap_or(0);
    store
        .dispatch(
            "Add track",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index, track }],
        )
        .map_err(|e| e.to_string())?;
    Ok(track_id)
}

fn ensure_free_video_track(
    store: &mut ProjectStore,
    start: TimeUs,
    duration: TimeUs,
) -> Result<Id, String> {
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("no active sequence")?;
    if let Some(t) = seq
        .tracks
        .iter()
        .rev()
        .find(|t| t.kind == TrackKind::Video && !t.locked && !t.collides(start, duration, None))
    {
        return Ok(t.id);
    }
    // create V(n+1) on top of everything
    let n = seq.tracks.iter().filter(|t| t.kind == TrackKind::Video).count();
    let track = ue_core::model::Track::new(TrackKind::Video, &format!("V{}", n + 1));
    let track_id = track.id;
    let index = seq.tracks.len();
    store
        .dispatch(
            "Add track",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index, track }],
        )
        .map_err(|e| e.to_string())?;
    Ok(track_id)
}

/// Adds a text (title) clip on the top video track. Returns its clip id.
pub(crate) fn add_text_clip_impl(
    state: &AppState,
    content: &str,
    at_us: TimeUs,
    duration_us: TimeUs,
) -> Res<Id> {
    let mut store = state.store.lock().unwrap();
    let duration = duration_us.max(1);
    let start = at_us.max(0);
    let track_id = ensure_free_video_track(&mut store, start, duration)?;
    let clip = Clip::new_text(content, start, duration);
    let clip_id = clip.id;
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(clip_id)
}

#[tauri::command]
fn add_text_clip(state: State<AppState>, content: String, at_us: TimeUs) -> Res<StateSnapshot> {
    add_text_clip_impl(&state, &content, at_us, 4_000_000)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Catalog of generators (core manifests) for the UI.
#[tauri::command]
fn get_generators() -> serde_json::Value {
    ue_render::generators_catalog_json(&ue_render::core_generators())
}

/// Adds a generator clip (rectangle, gradient, …) at the playhead, in a free
/// video track (created if needed). Returns its clip id.
pub(crate) fn add_generator_clip_impl(
    state: &AppState,
    generator_id: &str,
    at_us: TimeUs,
    duration_us: TimeUs,
) -> Res<Id> {
    if ue_render::find_generator(&ue_render::core_generators(), generator_id).is_none() {
        return Err(format!("unknown generator: {generator_id}"));
    }
    let mut store = state.store.lock().unwrap();
    let duration = duration_us.max(1);
    let start = at_us.max(0);
    let track_id = ensure_free_video_track(&mut store, start, duration)?;
    let clip = Clip::new_generator(generator_id, start, duration);
    let clip_id = clip.id;
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(clip_id)
}

#[tauri::command]
fn add_generator_clip(
    state: State<AppState>,
    generator_id: String,
    at_us: TimeUs,
) -> Res<StateSnapshot> {
    add_generator_clip_impl(&state, &generator_id, at_us, 4_000_000)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Edits a generator's parameters (undoable).
#[tauri::command]
fn set_clip_generator(
    state: State<AppState>,
    clip_id: String,
    generator_id: String,
    params: std::collections::BTreeMap<String, ue_core::keyframe::Param>,
    color_params: std::collections::BTreeMap<String, String>,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch_coalesced(
            "Edit generator",
            vec![ue_core::Action::SetClipGenerator {
                clip_id: id,
                generator_id,
                params,
                color_params,
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_clip_text(
    state: State<AppState>,
    clip_id: String,
    content: String,
    style: ue_core::model::TextStyle,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Edit text",
            vec![ue_core::Action::SetClipText { clip_id: id, content, style }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_track_prop(
    state: State<AppState>,
    track_id: String,
    prop: String,
    value: bool,
) -> Res<StateSnapshot> {
    use ue_core::action::TrackProp;
    let prop = match prop.as_str() {
        "muted" => TrackProp::Muted(value),
        "solo" => TrackProp::Solo(value),
        "locked" => TrackProp::Locked(value),
        other => return Err(format!("unknown property: {other}")),
    };
    set_track_prop_impl(&state, parse_id(&track_id)?, prop)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Adds a track at the end of its group (video on top, audio below). Undoable.
/// Returns the new track id.
pub(crate) fn add_track_impl(state: &AppState, kind: &str) -> Res<Id> {
    let mut store = state.store.lock().unwrap();
    let seq_id = store.project.active_sequence;
    let kind = match kind {
        "video" => TrackKind::Video,
        "audio" => TrackKind::Audio,
        other => return Err(format!("unknown track kind: {other} (use video|audio)")),
    };
    let seq = store.project.sequence(seq_id).ok_or("no sequence")?;
    let n = seq.tracks.iter().filter(|t| t.kind == kind).count();
    let prefix = if kind == TrackKind::Video { "V" } else { "A" };
    let track = ue_core::model::Track::new(kind, &format!("{prefix}{}", n + 1));
    let track_id = track.id;
    // video: at the end of the vec (drawn on top); audio: also at the end
    let index = seq.tracks.len();
    store
        .dispatch(
            "Add track",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index, track }],
        )
        .map_err(|e| e.to_string())?;
    Ok(track_id)
}

#[tauri::command]
fn add_track(state: State<AppState>, kind: String) -> Res<StateSnapshot> {
    add_track_impl(&state, &kind)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Removes a track (any clips it held go with it; 1 undo restores it).
pub(crate) fn remove_track_impl(state: &AppState, id: Id) -> Res<()> {
    let mut store = state.store.lock().unwrap();
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("no sequence")?;
    // don't leave the sequence without tracks of a kind
    let kind = seq.tracks.iter().find(|t| t.id == id).ok_or("track not found")?.kind;
    if seq.tracks.iter().filter(|t| t.kind == kind).count() <= 1 {
        return Err("cannot remove the last track of its kind".into());
    }
    store
        .dispatch("Delete track", vec![ue_core::Action::RemoveTrack { track_id: id }])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn remove_track(state: State<AppState>, track_id: String) -> Res<StateSnapshot> {
    remove_track_impl(&state, parse_id(&track_id)?)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Sets one track property (name / muted / solo / locked / volume_db).
/// Clamps the volume to the mixer's range; validates the name.
pub(crate) fn set_track_prop_impl(
    state: &AppState,
    track_id: Id,
    prop: ue_core::action::TrackProp,
) -> Res<()> {
    use ue_core::action::TrackProp;
    let mut store = state.store.lock().unwrap();
    let (label, prop) = match prop {
        TrackProp::Name(n) => {
            let n = n.trim().to_string();
            if n.is_empty() || n.len() > 24 {
                return Err("invalid track name (1..24 chars)".into());
            }
            ("Rename track", TrackProp::Name(n))
        }
        TrackProp::VolumeDb(db) => ("Track volume", TrackProp::VolumeDb(db.clamp(-60.0, 12.0))),
        other => ("Track", other),
    };
    store
        .dispatch(label, vec![ue_core::Action::SetTrackProp { track_id, prop }])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Renames a track (undoable).
#[tauri::command]
fn rename_track(state: State<AppState>, track_id: String, name: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&track_id)?;
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 24 {
        return Err("invalid track name".into());
    }
    store
        .dispatch(
            "Rename track",
            vec![ue_core::Action::SetTrackProp {
                track_id: id,
                prop: ue_core::action::TrackProp::Name(name),
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Track volume in dB (undoable).
#[tauri::command]
fn set_track_volume(state: State<AppState>, track_id: String, db: f32) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&track_id)?;
    store
        .dispatch_coalesced(
            "Track volume",
            vec![ue_core::Action::SetTrackProp {
                track_id: id,
                prop: ue_core::action::TrackProp::VolumeDb(db.clamp(-60.0, 12.0)),
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_clip_transition(
    state: State<AppState>,
    clip_id: String,
    transition: Option<ue_core::model::TransitionRef>,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Edit transition",
            vec![ue_core::Action::SetClipTransition { clip_id: id, transition }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// (port, token) of the embedded MCP server (None if it couldn't start).
#[tauri::command]
fn mcp_status(state: State<AppState>) -> Option<(u16, String)> {
    let port = (*state.mcp_port.lock().unwrap())?;
    Some((port, state.mcp_token.lock().unwrap().clone()))
}

#[tauri::command]
fn cancel_export(state: State<AppState>) -> Res<()> {
    state.export_cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Exports the active sequence to MP4 (blocking on a separate thread).
/// Emits `export-progress` events (0..1); `cancel_export` aborts it.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn export_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    max_height: Option<u32>,
    crf: Option<u8>,
    preset: Option<String>,
    audio_bitrate_k: Option<u32>,
    loudnorm: Option<bool>,
    range_in_us: Option<i64>,
    range_out_us: Option<i64>,
    ranges: Option<Vec<(i64, i64)>>,
    format: Option<String>,
) -> Res<String> {
    let (project, seq_id, base_dir) = {
        let store = state.store.lock().unwrap();
        let base = state
            .path
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        (store.project.clone(), store.project.active_sequence, base)
    };
    let cancel = state.export_cancel.clone();
    cancel.store(false, Ordering::SeqCst);
    let out = PathBuf::from(&path);
    dlog("export", &format!("start → {path}"));
    let extra_packs = state.user_packs.lock().unwrap().clone();
    let defaults = ue_export::ExportSettings::default();
    let range = match (range_in_us, range_out_us) {
        (Some(a), Some(b)) if b > a => Some((a.max(0), b)),
        _ => None,
    };
    let ranges: Vec<(i64, i64)> = ranges
        .unwrap_or_default()
        .into_iter()
        .map(|(a, b)| (a.max(0), b))
        .filter(|(a, b)| b > a)
        .collect();
    let format = match format.as_deref() {
        None | Some("mp4") => ue_export::ExportFormat::Mp4,
        Some("m4a") => ue_export::ExportFormat::M4a,
        Some("gif") => ue_export::ExportFormat::Gif,
        Some(other) => return Err(format!("unknown format: {other}")),
    };
    let settings = ue_export::ExportSettings {
        format,
        max_height,
        crf: crf.map(|c| c.clamp(10, 40)).unwrap_or(defaults.crf),
        preset: preset.unwrap_or(defaults.preset),
        audio_bitrate_k: audio_bitrate_k.unwrap_or(defaults.audio_bitrate_k),
        loudnorm: loudnorm.unwrap_or(false),
        range,
        ranges,
        extra_packs,
    };
    tauri::async_runtime::spawn_blocking(move || {
        ue_export::export_sequence_with_progress(
            &project,
            seq_id,
            &base_dir,
            &out,
            &settings,
            |p| {
                let _ = app.emit("export-progress", p);
            },
            &cancel,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| {
        dlog("export", &format!("join error: {e}"));
        e.to_string()
    })?
    .map_err(|e| {
        dlog("export", &format!("FAILED: {e}"));
        e
    })?;
    dlog("export", &format!("done → {path}"));
    Ok(path)
}

/// Is there an autosave newer than the given project (or orphaned)? → its path.
#[tauri::command]
fn check_recovery(app: tauri::AppHandle, state: State<AppState>, path: Option<String>) -> Res<Option<String>> {
    let data_dir = app.path().app_data_dir().ok();
    let project_path = path.map(PathBuf::from).or_else(|| state.path.lock().unwrap().clone());
    let Some(auto) = autosave_path(project_path.as_deref(), data_dir.as_deref()) else {
        return Ok(None);
    };
    if !auto.exists() {
        return Ok(None);
    }
    let newer = match &project_path {
        Some(p) if p.exists() => {
            let (Ok(ma), Ok(mp)) = (std::fs::metadata(&auto), std::fs::metadata(p)) else {
                return Ok(None);
            };
            matches!((ma.modified(), mp.modified()), (Ok(a), Ok(b)) if a > b)
        }
        _ => true, // project never saved: any autosave counts
    };
    Ok(newer.then(|| auto.display().to_string()))
}

/// Removes the active autosave (after saving or discarding the recovery).
#[tauri::command]
fn discard_recovery(app: tauri::AppHandle, state: State<AppState>) -> Res<()> {
    let data_dir = app.path().app_data_dir().ok();
    let project_path = state.path.lock().unwrap().clone();
    if let Some(auto) = autosave_path(project_path.as_deref(), data_dir.as_deref()) {
        let _ = std::fs::remove_file(auto);
    }
    // the orphan autosave too, in case it was just saved with a name
    if let Ok(d) = app.path().app_data_dir() {
        let _ = std::fs::remove_file(d.join("recovery.uep.autosave"));
    }
    Ok(())
}

/// Loads a recovery copy keeping the original project's path (the next Save
/// writes the real .uep) and marking changes.
#[tauri::command]
fn recover_project(
    app: tauri::AppHandle,
    state: State<AppState>,
    autosave: String,
    original: Option<String>,
) -> Res<StateSnapshot> {
    let json = std::fs::read_to_string(&autosave).map_err(|e| e.to_string())?;
    let mut project = Project::from_json(&json).map_err(|e| e.to_string())?;
    let dir = original
        .as_deref()
        .and_then(|p| Path::new(p).parent().map(|d| d.to_path_buf()))
        .or_else(|| Path::new(&autosave).parent().map(|d| d.to_path_buf()));
    resolve_project_paths(&mut project, dir.as_deref());
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    for asset in &mut project.assets {
        if let Some(cache) = &cache_dir {
            let conform = conform_target(cache, &asset.content_hash);
            if conform.exists() {
                asset.audio_conform = Some(conform.to_string_lossy().into_owned());
            } else if !asset.offline && asset.probe.audio_channels > 0 {
                spawn_conform_job(&app, asset, cache);
            }
            let proxy = cache.join(format!("{}.proxy.mp4", asset.content_hash));
            if proxy.exists() {
                asset.proxy = Some(proxy.to_string_lossy().into_owned());
            }
        }
    }
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(project);
    store.dirty = true;
    *state.path.lock().unwrap() = original.map(PathBuf::from);
    Ok(snapshot(&store))
}

/// Saves the project to `path` (or to the path it was opened from).
/// Paths under the .uep folder are relativized so the project is portable.
pub(crate) fn save_project_impl(state: &AppState, path: Option<String>) -> Res<String> {
    let mut store = state.store.lock().unwrap();
    let mut stored_path = state.path.lock().unwrap();
    let target = match path.map(PathBuf::from).or_else(|| stored_path.clone()) {
        Some(p) => p,
        None => return Err("no save path; pass a path".into()),
    };
    // PORTABILITY: serialize with paths relative to the .uep when possible
    let portable = make_portable(&store.project, target.parent());
    let json = portable.to_json().map_err(|e| e.to_string())?;
    // atomic write: tmp + rename
    let tmp = target.with_extension("uep.tmp");
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    store.dirty = false;
    // the real save invalidates the recovery copies
    let _ = std::fs::remove_file(target.with_extension("uep.autosave"));
    *stored_path = Some(target.clone());
    Ok(target.display().to_string())
}

#[tauri::command]
fn save_project(state: State<AppState>, path: Option<String>) -> Res<String> {
    save_project_impl(&state, path)
}

/// Opens a .uep, resolving relative paths and re-deriving the local caches.
/// DESTRUCTIVE: replaces the in-memory project and clears the history.
pub(crate) fn open_project_impl(
    app: Option<&tauri::AppHandle>,
    state: &AppState,
    path: &str,
) -> Res<()> {
    let json = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut project = Project::from_json(&json).map_err(|e| e.to_string())?;
    let issues = ue_core::validate::validate(&project);
    if !issues.is_empty() {
        return Err(format!("invalid project: {}", issues.join("; ")));
    }
    // resolve relative paths against the .uep folder and mark offline;
    // re-derive local caches by hash and relaunch conform if missing
    let dir = Path::new(path).parent().map(|d| d.to_path_buf());
    resolve_project_paths(&mut project, dir.as_deref());
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    for asset in &mut project.assets {
        if let Some(cache) = &cache_dir {
            let conform = conform_target(cache, &asset.content_hash);
            if conform.exists() {
                asset.audio_conform = Some(conform.to_string_lossy().into_owned());
            } else if !asset.offline && asset.probe.audio_channels > 0 {
                if let Some(app) = app {
                    spawn_conform_job(app, asset, cache);
                }
            }
            let proxy = cache.join(format!("{}.proxy.mp4", asset.content_hash));
            if proxy.exists() {
                asset.proxy = Some(proxy.to_string_lossy().into_owned());
            } else if !asset.offline {
                if let Some(app) = app {
                    spawn_proxy_job(app, asset, cache);
                }
            }
        }
    }
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(project);
    *state.path.lock().unwrap() = Some(PathBuf::from(path));
    Ok(())
}

#[tauri::command]
fn open_project(
    app: tauri::AppHandle,
    state: State<AppState>,
    path: String,
) -> Res<StateSnapshot> {
    open_project_impl(Some(&app), &state, &path)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Relinks an offline media: new path, re-probe and conform. Not undoable.
pub(crate) fn relink_asset_impl(
    app: Option<&tauri::AppHandle>,
    state: &AppState,
    id: Id,
    new_path: String,
) -> Res<()> {
    let fresh = ue_media::import_file(Path::new(&new_path)).map_err(|e| e.to_string())?;
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    let mut store = state.store.lock().unwrap();
    let asset = store
        .project
        .assets
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or("asset not found")?;
    asset.path = new_path;
    asset.content_hash = fresh.content_hash;
    asset.probe = fresh.probe;
    asset.offline = false;
    asset.audio_conform = None;
    asset.proxy = None;
    let asset_snapshot = asset.clone();
    if let (Some(app), Some(cache)) = (app, &cache_dir) {
        if asset_snapshot.probe.audio_channels > 0 {
            spawn_conform_job(app, &asset_snapshot, cache);
        }
        spawn_proxy_job(app, &asset_snapshot, cache);
    }
    store.version += 1;
    store.dirty = true;
    Ok(())
}

#[tauri::command]
fn relink_asset(
    app: tauri::AppHandle,
    state: State<AppState>,
    asset_id: String,
    new_path: String,
) -> Res<StateSnapshot> {
    relink_asset_impl(Some(&app), &state, parse_id(&asset_id)?, new_path)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

/// Replaces the in-memory project with an empty one. DESTRUCTIVE: unsaved
/// changes and the whole undo history are lost.
pub(crate) fn new_project_impl(state: &AppState, name: &str) {
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(Project::new(name));
    *state.path.lock().unwrap() = None;
}

#[tauri::command]
fn new_project(state: State<AppState>, name: String) -> Res<StateSnapshot> {
    new_project_impl(&state, &name);
    Ok(snapshot(&state.store.lock().unwrap()))
}

pub fn run() {
    let state = AppState::new_default();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(|app| {
            let state = app.state::<AppState>();
            if let Ok(dir) = app.path().app_cache_dir() {
                let _ = std::fs::create_dir_all(&dir);
                *state.cache_dir.lock().unwrap() = Some(dir);
            }
            if let Ok(dir) = app.path().app_config_dir() {
                let effects = dir.join("effects");
                let _ = std::fs::create_dir_all(&effects);
                *state.effects_dir.lock().unwrap() = Some(effects);
                let errors = reload_packs(&state);
                for e in errors {
                    eprintln!("[packs] invalid manifest: {e}");
                }
            }
            if let Ok(dir) = app.path().app_data_dir() {
                let models = dir.join("models");
                let _ = std::fs::create_dir_all(&models);
                *state.models_dir.lock().unwrap() = Some(models);
            }
            match mcp::start(app.handle().clone()) {
                Some(port) => {
                    *state.mcp_port.lock().unwrap() = Some(port);
                    let token = state.mcp_token.lock().unwrap().clone();
                    eprintln!(
                        "[mcp] listening on http://127.0.0.1:{port}/mcp (token: {token})"
                    );
                    // persist for local tooling (the server only binds loopback)
                    if let Ok(dir) = app.path().app_config_dir() {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::write(dir.join("mcp_token"), &token);
                    }
                }
                None => eprintln!("[mcp] could not open port {}", mcp::MCP_PORT),
            }
            let data_dir = app.path().app_data_dir().ok();
            let handle = app.handle().clone();
            std::thread::spawn(move || autosave_loop(handle, data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            ui_log,
            set_project_settings,
            split_clip,
            delete_clips,
            move_clip,
            trim_clip,
            cut_ranges,
            move_range,
            set_clip_speed,
            set_subtitles_props,
            unlink_clip,
            list_fonts,
            list_text_templates,
            save_text_template,
            undo,
            redo,
            set_clip_audio,
            set_clip_transform,
            import_media,
            add_clip,
            render_frame,
            get_effects_catalog,
            reload_effect_packs,
            mcp_status,
            set_clip_effects,
            set_clip_transition,
            set_track_prop,
            add_track,
            remove_track,
            rename_track,
            set_track_volume,
            add_text_clip,
            get_generators,
            add_generator_clip,
            set_clip_generator,
            set_clip_text,
            remove_silences,
            transcribe_asset,
            add_subtitles_clip,
            generate_vertical,
            set_active_sequence,
            remove_sequence,
            set_sequence_props,
            set_word_text,
            replace_words,
            add_avatar_clip,
            list_avatar_configs,
            generate_avatar_video,
            save_avatar_config,
            remove_avatar_config,
            pick_avatar_media,
            export_avatar_config,
            import_avatar_config,
            export_video,
            cancel_export,
            playback_play,
            playback_pause,
            playback_seek,
            playback_position,
            playback_set_rate,
            get_audio_peaks,
            ensure_thumbs,
            get_thumb_strip,
            playback_frame,
            save_project,
            check_recovery,
            recover_project,
            discard_recovery,
            open_project,
            relink_asset,
            new_project,
        ])
        .run(tauri::generate_context!())
        .expect("failed to start UberEditor");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ue_core::model::{Segment, TranscriptDoc, Word};

    fn doc(asset_id: Id, word: &str) -> TranscriptDoc {
        TranscriptDoc {
            id: Id::new(),
            asset_id,
            language: "es".into(),
            model: "t".into(),
            words: vec![Word {
                text: word.into(),
                start_us: 0,
                end_us: 1_000_000,
                confidence: 1.0,
                rejected: false,
                display: None,
            }],
            segments: vec![Segment {
                text: word.into(),
                start_us: 0,
                end_us: 1_000_000,
                word_range: (0, 1),
                emotion: None,
                volume_rms: 0.0,
            }],
            global_avg_volume: 0.0,
        }
    }

    /// Re-transcribing keeps the transcript id, so subtitles clips that
    /// reference it don't dangle (field bug: had to delete+recreate the clip).
    #[test]
    fn retranscribe_preserves_the_transcript_id() {
        let state = AppState::new_default();
        let asset_id = Id::new();
        let first = transcribe_commit(&state, asset_id, doc(asset_id, "uno"));
        let second = transcribe_commit(&state, asset_id, doc(asset_id, "dos"));
        assert_eq!(first, second, "the id is stable across re-transcription");
        let store = state.store.lock().unwrap();
        assert_eq!(store.project.transcripts.len(), 1, "the old doc was replaced, not stacked");
        assert_eq!(store.project.transcripts[0].words[0].text, "dos", "content updated");
    }
}
