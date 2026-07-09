//! Construcción de la línea de comandos de ffmpeg (inputs + filter_complex).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ue_core::model::{ClipPayload, Id, Project};
use ue_core::TimeUs;

use crate::edl::{build_video_edl_with, edl_duration, Segment};
use crate::{ExportError, ExportResult, ExportSettings};

pub struct FfmpegPlan {
    pub args: Vec<String>,
    pub duration_us: TimeUs,
}

fn secs(us: TimeUs) -> String {
    format!("{:.6}", us as f64 / 1_000_000.0)
}

fn resolve_path(base: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { base.join(path) }
}

/// Clip audible: cualquier clip media (en pista de audio o video) cuyo asset
/// tenga audio, sin mute de clip ni de pista.
struct AudioItem {
    asset_id: Id,
    src_in: TimeUs,
    src_out: TimeUs,
    start: TimeUs,
    speed: f64,
    gain_db: f64,
    fade_in_us: TimeUs,
    fade_out_us: TimeUs,
}

/// Cadena atempo (preserva el pitch). atempo acepta 0.5–2 por instancia:
/// se encadenan varias para factores extremos.
fn atempo_chain(speed: f64) -> String {
    let mut parts: Vec<String> = vec![];
    let mut rest = speed;
    while rest > 2.0 {
        parts.push("atempo=2".into());
        rest /= 2.0;
    }
    while rest < 0.5 {
        parts.push("atempo=0.5".into());
        rest /= 0.5;
    }
    if (rest - 1.0).abs() > 1e-9 {
        parts.push(format!("atempo={}", (rest * 10000.0).round() / 10000.0));
    }
    parts.join(",")
}

fn collect_audio(project: &Project, sequence_id: Id) -> Vec<AudioItem> {
    let Some(seq) = project.sequence(sequence_id) else { return vec![] };
    let mut items = vec![];
    let any_solo = seq.tracks.iter().any(|t| t.solo);
    for track in &seq.tracks {
        if track.muted || (any_solo && !track.solo) {
            continue;
        }
        for clip in &track.clips {
            if clip.audio.muted {
                continue;
            }
            if let ClipPayload::Media { asset_id, src_in, src_out } = &clip.payload {
                let Some(asset) = project.asset(*asset_id) else { continue };
                if asset.probe.audio_channels == 0 {
                    continue;
                }
                items.push(AudioItem {
                    asset_id: *asset_id,
                    src_in: *src_in,
                    src_out: *src_out,
                    start: clip.start,
                    speed: clip.speed,
                    gain_db: clip.audio.gain_db.eval(0) + track.volume_db as f64,
                    fade_in_us: clip.audio.fade_in_us,
                    fade_out_us: clip.audio.fade_out_us,
                });
            }
        }
    }
    items
}

/// Escapado para el valor text='…' de drawtext dentro de un filter_complex.
fn escape_drawtext(text: &str) -> String {
    text.replace('\\', "\\\\\\\\")
        .replace('\'', "\u{2019}") // comilla tipográfica: evita el infierno de quoting
        .replace(':', "\\:")
        .replace('%', "\\%")
}

/// Primera fuente del sistema disponible para drawtext (fontfile);
/// si no hay ninguna, se confía en fontconfig (font=sans).
pub fn find_font() -> Option<&'static str> {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ];
    CANDIDATES.iter().copied().find(|p| Path::new(p).exists())
}

/// Un drawtext con estilo del proyecto, activo en [from, to) del timeline.
fn drawtext_for(
    font_part: &str,
    content: &str,
    style: &ue_core::model::TextStyle,
    scale: f64,
    from: TimeUs,
    to: TimeUs,
) -> String {
    let fontsize = ((style.size as f64) * scale).round().max(8.0) as u32;
    let color = style.color.trim_start_matches('#');
    let y_off = (style.y_offset as f64 * scale).round() as i64;
    format!(
        "drawtext={font_part}:text='{}':fontsize={fontsize}:fontcolor=0x{color}:\
         borderw={}:bordercolor=black@0.6:x=(w-text_w)/2:y=(h-text_h)/2+{y_off}:\
         enable='between(t,{},{})'",
        escape_drawtext(content),
        (2.0 * scale).round().max(1.0) as u32,
        secs(from),
        secs(to),
    )
}

/// Tiempo de asset → timeline via el primer clip media que lo contiene.
fn asset_time_to_timeline(
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

/// Cadena drawtext para títulos y subtítulos automáticos de la secuencia.
/// El tamaño/offset del estilo está referido a 1080p y se escala a `out_h`.
fn build_text_overlays(
    project: &Project,
    seq: &ue_core::model::Sequence,
    out_h: u32,
) -> Option<String> {
    use ue_core::model::{ClipPayload, TrackKind};
    let scale = out_h as f64 / 1080.0;
    let font_part = match find_font() {
        Some(f) => format!("fontfile={f}"),
        None => "font=sans".to_string(),
    };
    let mut parts: Vec<String> = vec![];
    for track in seq.tracks.iter().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            match &clip.payload {
                ClipPayload::Text { content, style } => {
                    if content.trim().is_empty() {
                        continue;
                    }
                    parts.push(drawtext_for(
                        &font_part,
                        content,
                        style,
                        scale,
                        clip.start,
                        clip.end(),
                    ));
                }
                ClipPayload::Subtitles { transcript_id, style, mode } => {
                    let Some(doc) =
                        project.transcripts.iter().find(|t| t.id == *transcript_id)
                    else {
                        continue;
                    };
                    use ue_core::model::SubtitleMode;
                    // modo frase: una línea por segmento; modo palabra/karaoke:
                    // una palabra grande cada vez (estilo shorts)
                    let items: Vec<(&str, i64, i64)> = match mode {
                        SubtitleMode::Phrase => doc
                            .segments
                            .iter()
                            .map(|s| (s.text.as_str(), s.start_us, s.end_us))
                            .collect(),
                        SubtitleMode::Word | SubtitleMode::Karaoke => doc
                            .words
                            .iter()
                            .filter(|w| !w.rejected)
                            .map(|w| (w.text.as_str(), w.start_us, w.end_us))
                            .collect(),
                    };
                    let word_scale = match mode {
                        SubtitleMode::Phrase => 1.0,
                        _ => 1.6, // palabras sueltas más grandes
                    };
                    let mut wstyle = style.clone();
                    wstyle.size *= word_scale as f32;
                    for (text, s_us, e_us) in items {
                        if text.trim().is_empty() {
                            continue;
                        }
                        let Some(tl_start) = asset_time_to_timeline(seq, doc.asset_id, s_us)
                        else {
                            continue; // ese trozo del asset no está en el timeline
                        };
                        let tl_end = tl_start + (e_us - s_us);
                        let from = tl_start.max(clip.start);
                        let to = tl_end.min(clip.end());
                        if to <= from {
                            continue;
                        }
                        parts.push(drawtext_for(&font_part, text, &wstyle, scale, from, to));
                    }
                }
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// Escapado del filename del filtro movie (dentro de filter_complex).
fn escape_movie_path(p: &str) -> String {
    p.replace('\\', "/").replace(':', "\\\\:").replace('\'', "\\\\'")
}

/// Tramos (emoción, from, to) de un clip Avatar: los segmentos del transcript
/// mapeados al timeline, con los huecos rellenos con la emoción por defecto
/// (comportamiento de avatar_video_generation.py del toolkit).
fn avatar_spans(
    project: &Project,
    seq: &ue_core::model::Sequence,
    clip: &ue_core::model::Clip,
    driver_asset: Id,
    default_emotion: &str,
) -> Vec<(String, TimeUs, TimeUs, f64)> {
    let doc = project.transcripts.iter().find(|t| t.asset_id == driver_asset);
    let (cs, ce) = (clip.start, clip.end());
    let mut spans: Vec<(String, TimeUs, TimeUs, f64)> = vec![];
    let mut cursor = cs;
    if let Some(doc) = doc {
        let avg = if doc.global_avg_volume > 1e-9 { doc.global_avg_volume } else { 1.0 };
        for seg in &doc.segments {
            let Some(tl_start) = asset_time_to_timeline(seq, driver_asset, seg.start_us) else {
                continue;
            };
            let tl_end = tl_start + (seg.end_us - seg.start_us);
            let from = tl_start.max(cs);
            let to = tl_end.min(ce);
            if to <= from {
                continue;
            }
            if from > cursor {
                spans.push((default_emotion.to_string(), cursor, from, 1.0));
            }
            let emotion = seg.emotion.clone().unwrap_or_else(|| default_emotion.to_string());
            spans.push((emotion, from, to, (seg.volume_rms / avg).clamp(0.0, 3.0)));
            cursor = to;
        }
    }
    if cursor < ce {
        spans.push((default_emotion.to_string(), cursor, ce, 1.0));
    }
    spans
}

/// Cadenas movie+overlay para los clips Avatar. Devuelve (cadenas, etiqueta final).
fn build_avatar_overlays(
    project: &Project,
    seq: &ue_core::model::Sequence,
    base_dir: &Path,
    in_label: &str,
    out_w: u32,
) -> (Vec<String>, String) {
    use ue_core::model::{ClipPayload, TrackKind};
    let mut fc: Vec<String> = vec![];
    let mut current = in_label.to_string();
    let mut n = 0usize;
    for track in seq.tracks.iter().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            let ClipPayload::Avatar { driver_asset, avatars, shake_factor, scale } =
                &clip.payload
            else {
                continue;
            };
            let Some(default_emotion) = avatars.keys().next().cloned() else { continue };
            // ancho del avatar en px (par)
            let aw = (((out_w as f64) * scale.clamp(0.05, 1.0)) as u32) & !1;
            let margin = 24;
            let base_x = out_w as i64 - aw as i64 - margin;
            for (emotion, from, to, vol_ratio) in
                avatar_spans(project, seq, clip, *driver_asset, &default_emotion)
            {
                let path_str = avatars
                    .get(&emotion)
                    .or_else(|| avatars.get(&default_emotion))
                    .cloned()
                    .unwrap_or_default();
                if path_str.is_empty() {
                    continue;
                }
                let abs = {
                    let p = Path::new(&path_str);
                    if p.is_absolute() { p.to_path_buf() } else { base_dir.join(p) }
                };
                if !abs.exists() {
                    continue; // avatar faltante: se omite ese tramo sin romper
                }
                let amp = (shake_factor * vol_ratio * 8.0).round();
                let av = format!("av{n}");
                let nx = format!("avo{n}");
                fc.push(format!(
                    "movie=filename='{}':loop=999,setpts=PTS-STARTPTS+{}/TB,scale={aw}:-2[{av}]",
                    escape_movie_path(&abs.to_string_lossy()),
                    secs(from),
                ));
                fc.push(format!(
                    "[{current}][{av}]overlay=x='{base_x}+{amp}*sin(t*37)':\
                     y='H-h-{margin}+{amp}*cos(t*51)':enable='between(t,{},{})':\
                     eof_action=pass[{nx}]",
                    secs(from),
                    secs(to),
                ));
                current = nx;
                n += 1;
            }
        }
    }
    (fc, current)
}

pub fn build_ffmpeg_args(
    project: &Project,
    sequence_id: Id,
    base_dir: &Path,
    output: &Path,
    settings: &ExportSettings,
) -> ExportResult<FfmpegPlan> {
    let seq = project
        .sequence(sequence_id)
        .ok_or(ExportError::NoSequence(sequence_id))?;
    let edl = build_video_edl_with(project, sequence_id, &settings.extra_packs)?;
    let total_us = edl_duration(&edl);
    let audio_items = collect_audio(project, sequence_id);

    let (mut out_w, mut out_h) = seq.resolution;
    if let Some(mh) = settings.max_height {
        if out_h > mh {
            out_w = (out_w as u64 * mh as u64 / out_h as u64) as u32 & !1;
            out_h = mh & !1;
        }
    }
    let fps = format!("{}/{}", seq.fps.0, seq.fps.1);

    // inputs únicos por asset
    let mut input_index: BTreeMap<Id, usize> = BTreeMap::new();
    let mut inputs: Vec<PathBuf> = vec![];
    let mut input_of = |asset_id: Id, project: &Project| -> usize {
        *input_index.entry(asset_id).or_insert_with(|| {
            let asset = project.asset(asset_id).expect("validado en la EDL");
            inputs.push(resolve_path(base_dir, &asset.path));
            inputs.len() - 1
        })
    };

    // ---- cadenas de video ----
    let mut fc: Vec<String> = vec![];
    let norm = format!(
        "fps={fps},scale={out_w}:{out_h}:force_original_aspect_ratio=decrease,\
         pad={out_w}:{out_h}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p"
    );
    for (k, seg) in edl.iter().enumerate() {
        let label = format!("v{k}");
        match seg {
            Segment::Source { asset_id, src_in, src_out, speed, vf, .. } => {
                let idx = input_of(*asset_id, project);
                let effects = match vf {
                    Some(chain) => format!("{chain},"),
                    None => String::new(),
                };
                let setpts = if (*speed - 1.0).abs() > 1e-9 {
                    format!("setpts=(PTS-STARTPTS)/{speed}")
                } else {
                    "setpts=PTS-STARTPTS".to_string()
                };
                fc.push(format!(
                    "[{idx}:v]trim=start={}:end={},{setpts},{effects}{norm}[{label}]",
                    secs(*src_in),
                    secs(*src_out),
                ));
            }
            Segment::Black { duration } => {
                fc.push(format!(
                    "color=black:size={out_w}x{out_h}:rate={fps}:duration={}[{label}]",
                    secs(*duration),
                ));
            }
        }
    }

    // Combinación secuencial: concat en cortes duros, xfade en transiciones.
    let mut current = "v0".to_string();
    let mut acc_dur = edl[0].duration();
    for (k, seg) in edl.iter().enumerate().skip(1) {
        let out_label = format!("m{k}");
        let transition = match seg {
            Segment::Source { transition_in, .. } => *transition_in,
            _ => None,
        };
        match transition {
            Some(d) => {
                let offset = acc_dur - d;
                fc.push(format!(
                    "[{current}][v{k}]xfade=transition=fade:duration={}:offset={}[{out_label}]",
                    secs(d),
                    secs(offset),
                ));
                acc_dur += seg.duration() - d;
            }
            None => {
                fc.push(format!(
                    "[{current}][v{k}]concat=n=2:v=1:a=0[{out_label}]"
                ));
                acc_dur += seg.duration();
            }
        }
        current = out_label;
    }
    // avatares reactivos (movie+overlay por segmento)
    let (avatar_chains, after_avatars) =
        build_avatar_overlays(project, seq, base_dir, &current, out_w);
    fc.extend(avatar_chains);
    current = after_avatars;

    // quemar títulos y subtítulos sobre el video combinado
    let text_chain = build_text_overlays(project, seq, out_h);
    match text_chain {
        Some(chain) => fc.push(format!("[{current}]{chain}[vout]")),
        None => fc.push(format!("[{current}]null[vout]")),
    }

    // ---- cadenas de audio ----
    let mut alabels: Vec<String> = vec![];
    for (k, item) in audio_items.iter().enumerate() {
        let idx = input_of(item.asset_id, project);
        let label = format!("a{k}");
        let dur_us = (((item.src_out - item.src_in) as f64) / item.speed).round() as TimeUs;
        let mut chain = format!(
            "[{idx}:a]atrim=start={}:end={},asetpts=PTS-STARTPTS,\
             aresample=48000,aformat=channel_layouts=stereo",
            secs(item.src_in),
            secs(item.src_out),
        );
        if (item.speed - 1.0).abs() > 1e-9 {
            chain.push(',');
            chain.push_str(&atempo_chain(item.speed));
        }
        if item.gain_db.abs() > 1e-9 {
            chain.push_str(&format!(",volume={:.2}dB", item.gain_db));
        }
        if item.fade_in_us > 0 {
            chain.push_str(&format!(",afade=t=in:st=0:d={}", secs(item.fade_in_us)));
        }
        if item.fade_out_us > 0 {
            chain.push_str(&format!(
                ",afade=t=out:st={}:d={}",
                secs(dur_us - item.fade_out_us),
                secs(item.fade_out_us),
            ));
        }
        if item.start > 0 {
            chain.push_str(&format!(",adelay={}:all=1", item.start / 1000)); // ms
        }
        chain.push_str(&format!("[{label}]"));
        fc.push(chain);
        alabels.push(format!("[{label}]"));
    }
    let has_audio = !alabels.is_empty();
    if has_audio {
        fc.push(format!(
            "{}amix=inputs={}:duration=longest:normalize=0,atrim=0:{}[aout]",
            alabels.join(""),
            alabels.len(),
            secs(total_us),
        ));
    }

    // ---- línea de comandos ----
    let mut args: Vec<String> = vec!["-y".into(), "-v".into(), "error".into()];
    for input in &inputs {
        args.push("-i".into());
        args.push(input.to_string_lossy().into_owned());
    }
    args.push("-filter_complex".into());
    args.push(fc.join(";"));
    args.extend(["-map".into(), "[vout]".into()]);
    if has_audio {
        args.extend(["-map".into(), "[aout]".into()]);
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{}k", settings.audio_bitrate_k),
        ]);
    } else {
        args.push("-an".into());
    }
    args.extend([
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        settings.preset.clone(),
        "-crf".into(),
        settings.crf.to_string(),
        "-movflags".into(),
        "+faststart".into(),
    ]);
    args.push(output.to_string_lossy().into_owned());

    Ok(FfmpegPlan { args, duration_us: total_us })
}
