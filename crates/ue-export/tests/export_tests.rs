//! Tests de ue-export: EDL (unitarios) y exportación real con ffmpeg
//! (integración, verificable visualmente con el contador de testsrc).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use std::sync::atomic::AtomicBool;

use ue_core::model::*;
use ue_core::ops::InsertMode;
use ue_core::ProjectStore;
use ue_export::edl::{build_video_edl, edl_duration, Segment};
use ue_export::{
    export_sequence, export_sequence_with_progress, ExportError, ExportSettings,
};

const SEC: i64 = 1_000_000;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn fake_asset(kind: MediaKind, path: &str, dur_s: i64, with_audio: bool) -> MediaAsset {
    MediaAsset {
        id: Id::new(),
        kind,
        path: path.into(),
        content_hash: format!("xxh3:{path}"),
        probe: ProbeInfo {
            duration_us: dur_s * SEC,
            fps: if kind == MediaKind::Video { Some((30, 1)) } else { None },
            width: if kind == MediaKind::Video { 640 } else { 0 },
            height: if kind == MediaKind::Video { 360 } else { 0 },
            rotation: 0,
            vcodec: None,
            acodec: None,
            audio_channels: if with_audio { 2 } else { 0 },
            vfr: false,
        },
        proxy: None,
        audio_conform: None,
        peaks: None,
        thumbnails: None,
        transcript: None,
        offline: false,
    }
}

/// (store, seq, v1, v2, a1, asset_a, asset_b)
fn project_two_video_tracks() -> (ProjectStore, Id, Id, Id, Id, Id, Id) {
    let mut p = Project::new("t");
    let seq_id = p.active_sequence;
    let a = fake_asset(MediaKind::Video, "a.mp4", 10, false);
    let b = fake_asset(MediaKind::Video, "b.mp4", 10, false);
    let (aid, bid) = (a.id, b.id);
    p.assets.push(a);
    p.assets.push(b);
    // añadir V2 sobre V1
    let seq = p.sequence_mut(seq_id).unwrap();
    seq.tracks.push(Track::new(TrackKind::Video, "V2"));
    let v2 = seq.tracks.last().unwrap().id;
    let v1 = seq.tracks.iter().find(|t| t.kind == TrackKind::Video && t.name == "V1").unwrap().id;
    let a1 = seq.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap().id;
    (ProjectStore::new(p), seq_id, v1, v2, a1, aid, bid)
}

// ---------------------------------------------------------------------------
// EDL
// ---------------------------------------------------------------------------

#[test]
fn edl_with_gap_and_two_sources() {
    let (mut store, seq, v1, _v2, _a1, a, b) = project_two_video_tracks();
    store.insert_clip(v1, Clip::new_media(a, 1 * SEC, 3 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(v1, Clip::new_media(b, 4 * SEC, 6 * SEC, 2 * SEC), InsertMode::Strict).unwrap();
    // hueco [4, 5) y luego otra vez A
    store.insert_clip(v1, Clip::new_media(a, 8 * SEC, 9 * SEC, 5 * SEC), InsertMode::Strict).unwrap();

    let edl = build_video_edl(&store.project, seq).unwrap();
    assert_eq!(
        edl,
        vec![
            Segment::Source { asset_id: a, src_in: 1 * SEC, src_out: 3 * SEC, speed: 1.0, vf: None, transition_in: None },
            Segment::Source { asset_id: b, src_in: 4 * SEC, src_out: 6 * SEC, speed: 1.0, vf: None, transition_in: None },
            Segment::Black { duration: 1 * SEC },
            Segment::Source { asset_id: a, src_in: 8 * SEC, src_out: 9 * SEC, speed: 1.0, vf: None, transition_in: None },
        ]
    );
    assert_eq!(edl_duration(&edl), 6 * SEC);
}

#[test]
fn edl_top_track_wins() {
    let (mut store, seq, v1, v2, _a1, a, b) = project_two_video_tracks();
    // V1: A cubre [0, 6); V2: B cubre [2, 4) → B debe ganar en el medio
    store.insert_clip(v1, Clip::new_media(a, 0, 6 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(v2, Clip::new_media(b, 0, 2 * SEC, 2 * SEC), InsertMode::Strict).unwrap();

    let edl = build_video_edl(&store.project, seq).unwrap();
    assert_eq!(
        edl,
        vec![
            Segment::Source { asset_id: a, src_in: 0, src_out: 2 * SEC, speed: 1.0, vf: None, transition_in: None },
            Segment::Source { asset_id: b, src_in: 0, src_out: 2 * SEC, speed: 1.0, vf: None, transition_in: None },
            Segment::Source { asset_id: a, src_in: 4 * SEC, src_out: 6 * SEC, speed: 1.0, vf: None, transition_in: None },
        ]
    );
}

#[test]
fn edl_merges_contiguous_and_trims_trailing_black() {
    let (mut store, seq, v1, _v2, _a1, a, _b) = project_two_video_tracks();
    // dos clips contiguos con fuente continua → se fusionan
    store.insert_clip(v1, Clip::new_media(a, 1 * SEC, 3 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(v1, Clip::new_media(a, 3 * SEC, 5 * SEC, 2 * SEC), InsertMode::Strict).unwrap();
    let edl = build_video_edl(&store.project, seq).unwrap();
    assert_eq!(
        edl,
        vec![Segment::Source { asset_id: a, src_in: 1 * SEC, src_out: 5 * SEC, speed: 1.0, vf: None, transition_in: None }]
    );
}

#[test]
fn edl_supports_speed_and_rejects_empty() {
    let (mut store, seq, v1, _v2, _a1, a, _b) = project_two_video_tracks();
    assert!(matches!(
        build_video_edl(&store.project, seq),
        Err(ExportError::EmptyTimeline)
    ));
    // clip a 2×: usa [0..2s) de la fuente en 1 s de timeline
    let mut clip = Clip::new_media(a, 0, 2 * SEC, 0);
    clip.speed = 2.0;
    clip.duration = 1 * SEC;
    store.insert_clip(v1, clip, InsertMode::Strict).unwrap();
    let edl = build_video_edl(&store.project, seq).unwrap();
    match &edl[0] {
        Segment::Source { src_in, src_out, speed, .. } => {
            assert_eq!((*src_in, *src_out), (0, 2 * SEC), "consume toda la fuente");
            assert_eq!(*speed, 2.0);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(edl_duration(&edl), 1 * SEC, "1 s de salida a 2×");
}

#[test]
fn muted_track_is_invisible() {
    let (mut store, seq, v1, v2, _a1, a, b) = project_two_video_tracks();
    store.insert_clip(v1, Clip::new_media(a, 0, 2 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(v2, Clip::new_media(b, 0, 2 * SEC, 0), InsertMode::Strict).unwrap();
    // sin mute gana V2 (B)
    let edl = build_video_edl(&store.project, seq).unwrap();
    assert!(matches!(edl[0], Segment::Source { asset_id, .. } if asset_id == b));
    // con V2 muteada gana V1 (A)
    store
        .dispatch(
            "mute",
            vec![ue_core::Action::SetTrackProp {
                track_id: v2,
                prop: ue_core::action::TrackProp::Muted(true),
            }],
        )
        .unwrap();
    let edl = build_video_edl(&store.project, seq).unwrap();
    assert!(matches!(edl[0], Segment::Source { asset_id, .. } if asset_id == a));
}

// ---------------------------------------------------------------------------
// Exportación real (ffmpeg)
// ---------------------------------------------------------------------------

fn ffmpeg_available() -> bool {
    Command::new(ue_media::ffmpeg_bin())
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn media_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        if !ffmpeg_available() {
            eprintln!("AVISO: ffmpeg no disponible; test de export saltado");
            return None;
        }
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-export-media");
        std::fs::create_dir_all(&dir).unwrap();
        let st = Command::new(ue_media::ffmpeg_bin())
            .args([
                "-y", "-v", "error",
                "-f", "lavfi", "-i", "testsrc=duration=8:size=640x360:rate=30",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=8",
                "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-shortest",
            ])
            .arg(dir.join("counter.mp4"))
            .status()
            .unwrap();
        assert!(st.success());
        Some(dir)
    })
    .as_ref()
}

fn ffprobe_json(path: &Path) -> serde_json::Value {
    let out = Command::new(ue_media::ffprobe_bin())
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Edición: [1..3s) + [4..6s) del mismo archivo, pegadas. El resultado dura 4 s
/// y el contador quemado debe saltar 1,2 → 4,5. Se validan metadatos con
/// ffprobe y se extraen frames para verificación visual.
#[test]
fn export_cut_and_reordered_timeline() {
    let Some(dir) = media_dir() else { return };
    let mut project = Project::new("export-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&dir.join("counter.mp4")).unwrap();
    let asset_id = asset.id;
    project.assets.push(asset);
    let v1 = project
        .sequence(seq_id)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    store.insert_clip(v1, Clip::new_media(asset_id, 1 * SEC, 3 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(v1, Clip::new_media(asset_id, 4 * SEC, 6 * SEC, 2 * SEC), InsertMode::Strict).unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-export-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    // metadatos
    let meta = ffprobe_json(&out);
    let dur: f64 = meta["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((3.9..=4.2).contains(&dur), "duración ≈ 4 s, fue {dur}");
    let streams = meta["streams"].as_array().unwrap();
    let v = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
    assert_eq!(v["codec_name"], "h264");
    assert_eq!(v["width"], 1920); // resolución de la secuencia (con pad)
    assert!(streams.iter().any(|s| s["codec_type"] == "audio"), "lleva el audio del clip");

    // frames para verificación visual: 0.5s → contador "1"; 2.5s → contador "4"
    let frames_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-export-frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    for (name, t) in [("salida_0.5s", 0.5f64), ("salida_2.5s", 2.5f64)] {
        let st = Command::new(ue_media::ffmpeg_bin())
            .args(["-y", "-v", "error", "-ss", &t.to_string(), "-i"])
            .arg(&out)
            .args(["-frames:v", "1"])
            .arg(frames_dir.join(format!("{name}.jpg")))
            .status()
            .unwrap();
        assert!(st.success());
    }
    eprintln!("export verificable en {} y frames en {}", out.display(), frames_dir.display());
}

// ---------------------------------------------------------------------------
// Progreso, cancelación y efectos (chroma key de punta a punta)
// ---------------------------------------------------------------------------

/// Timeline mínimo listo para exportar sobre counter.mp4.
fn simple_store(dir: &Path) -> (ProjectStore, Id) {
    let mut project = Project::new("fx-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&dir.join("counter.mp4")).unwrap();
    let asset_id = asset.id;
    project.assets.push(asset);
    let v1 = project
        .sequence(seq_id)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    store
        .insert_clip(v1, Clip::new_media(asset_id, 0, 4 * SEC, 0), InsertMode::Strict)
        .unwrap();
    (store, seq_id)
}

#[test]
fn export_reports_monotonic_progress() {
    let Some(dir) = media_dir() else { return };
    let (store, seq_id) = simple_store(dir);
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-progress-out.mp4");
    let mut values: Vec<f32> = vec![];
    let never = AtomicBool::new(false);
    export_sequence_with_progress(
        &store.project,
        seq_id,
        dir,
        &out,
        &ExportSettings::default(),
        |p| values.push(p),
        &never,
    )
    .unwrap();
    assert!(!values.is_empty(), "hubo reportes de progreso");
    assert!(values.windows(2).all(|w| w[0] <= w[1]), "monótono: {values:?}");
    assert_eq!(*values.last().unwrap(), 1.0, "termina en 1.0");
}

#[test]
fn export_cancellation_kills_and_cleans() {
    let Some(dir) = media_dir() else { return };
    let (store, seq_id) = simple_store(dir);
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-cancel-out.mp4");
    let cancel = AtomicBool::new(true); // cancelado desde el arranque
    let result = export_sequence_with_progress(
        &store.project,
        seq_id,
        dir,
        &out,
        &ExportSettings::default(),
        |_| {},
        &cancel,
    );
    assert!(matches!(result, Err(ExportError::Cancelled)));
    assert!(!out.exists(), "el parcial se borra al cancelar");
}

/// Lee el píxel RGB (x, y) del frame en `t` segundos de un video de 1920 de ancho.
fn pixel_at(video: &Path, t: f64, x: u32, y: u32) -> (u8, u8, u8) {
    pixel_at_w(video, t, x, y, 1920)
}

/// Igual que pixel_at pero para anchos arbitrarios.
fn pixel_at_w(video: &Path, t: f64, x: u32, y: u32, w: usize) -> (u8, u8, u8) {
    let out = Command::new(ue_media::ffmpeg_bin())
        .args(["-v", "error", "-ss", &t.to_string(), "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let idx = (y as usize * w + x as usize) * 3;
    (out.stdout[idx], out.stdout[idx + 1], out.stdout[idx + 2])
}

/// Chroma key de punta a punta: fondo verde con caja roja → export con el
/// efecto → el verde desaparece (negro v0) y el rojo sobrevive.
#[test]
fn chroma_key_effect_applies_in_export() {
    let Some(dir) = media_dir() else { return };
    // fuente: fondo verde puro con caja roja centrada
    let src = dir.join("greenscreen.mp4");
    let st = Command::new(ue_media::ffmpeg_bin())
        .args([
            "-y", "-v", "error",
            "-f", "lavfi", "-i",
            "color=c=0x00FF00:s=640x360:d=2,drawbox=x=220:y=100:w=200:h=160:color=red:t=fill",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());

    let mut project = Project::new("chroma-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&src).unwrap();
    let asset_id = asset.id;
    project.assets.push(asset);
    let v1 = project
        .sequence(seq_id)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    let mut clip = Clip::new_media(asset_id, 0, 2 * SEC, 0);
    clip.effects.push(EffectInstance {
        effect_id: "core.chroma_key".into(),
        enabled: true,
        params: [("similarity".to_string(), ue_core::keyframe::Param::Const(0.25))]
            .into_iter()
            .collect(),
        color_params: [("key_color".to_string(), "#00ff00".to_string())]
            .into_iter()
            .collect(),
    });
    store.insert_clip(v1, clip, InsertMode::Strict).unwrap();

    // la EDL lleva la cadena renderizada
    let edl = build_video_edl(&store.project, seq_id).unwrap();
    match &edl[0] {
        Segment::Source { vf: Some(vf), .. } => {
            assert!(vf.contains("chromakey=color=0x00FF00"), "cadena: {vf}");
            assert!(vf.contains("despill"));
        }
        other => panic!("se esperaba Source con vf, fue {other:?}"),
    }

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-chroma-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    // El video fuente 640x360 se letterboxea a 1920x1080 (escala ×3):
    // fondo verde en (100,540)→fuente(33,180); caja roja en (960,540)→fuente(320,180).
    let (r, g, b) = pixel_at(&out, 1.0, 100, 540);
    assert!(
        r < 40 && g < 40 && b < 40,
        "el fondo verde quedó keyeado (negro v0), fue rgb({r},{g},{b})"
    );
    let (r, g, b) = pixel_at(&out, 1.0, 960, 540);
    assert!(
        r > 150 && g < 90 && b < 90,
        "la caja roja sobrevive al keying, fue rgb({r},{g},{b})"
    );

    // y sin el efecto, el fondo sigue verde (control)
    let mut store2 = {
        let mut project = Project::new("control");
        let seq_id2 = project.active_sequence;
        let asset = ue_media::import_file(&src).unwrap();
        let aid = asset.id;
        project.assets.push(asset);
        let v1 = project
            .sequence(seq_id2)
            .unwrap()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        let mut s = ProjectStore::new(project);
        s.insert_clip(v1, Clip::new_media(aid, 0, 2 * SEC, 0), InsertMode::Strict).unwrap();
        (s, seq_id2)
    };
    let out2 = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-chroma-control.mp4");
    export_sequence(&store2.0.project, store2.1, dir, &out2, &ExportSettings::default()).unwrap();
    let (r, g, _b) = pixel_at(&out2, 1.0, 100, 540);
    assert!(g > 150 && r < 90, "control sin efecto: sigue verde");
    let _ = &mut store2;
}

// ---------------------------------------------------------------------------
// Transiciones (crossfade)
// ---------------------------------------------------------------------------

#[test]
fn transition_extends_handles_and_survives_edl() {
    let (mut store, seq, v1, _v2, _a1, a, b) = project_two_video_tracks();
    // A usa [1..3) del archivo (hay material después); B usa [4..6) (hay antes)
    store.insert_clip(v1, Clip::new_media(a, 1 * SEC, 3 * SEC, 0), InsertMode::Strict).unwrap();
    let clip_b = Clip::new_media(b, 4 * SEC, 6 * SEC, 2 * SEC);
    let b_id = clip_b.id;
    store.insert_clip(v1, clip_b, InsertMode::Strict).unwrap();
    store
        .dispatch(
            "transición",
            vec![ue_core::Action::SetClipTransition {
                clip_id: b_id,
                transition: Some(TransitionRef {
                    effect_id: "core.crossfade".into(),
                    duration: 1 * SEC,
                    params: Default::default(),
                }),
            }],
        )
        .unwrap();

    let edl = build_video_edl(&store.project, seq).unwrap();
    assert_eq!(edl.len(), 2);
    // handles: A extendida +0.5s, B adelantada -0.5s, transición efectiva 1s
    match (&edl[0], &edl[1]) {
        (
            Segment::Source { src_in: a_in, src_out: a_out, .. },
            Segment::Source { src_in: b_in, src_out: b_out, transition_in, .. },
        ) => {
            assert_eq!((*a_in, *a_out), (1 * SEC, 3 * SEC + 500_000));
            assert_eq!((*b_in, *b_out), (4 * SEC - 500_000, 6 * SEC));
            assert_eq!(*transition_in, Some(1 * SEC));
        }
        other => panic!("EDL inesperada: {other:?}"),
    }
    // la duración de salida no cambia: 4 s
    assert_eq!(edl_duration(&edl), 4 * SEC);

    // sin material suficiente (clip A pegado al final del archivo) → se reduce
    let (mut store2, seq2, v1b, _v, _a, a2, b2) = project_two_video_tracks();
    store2.insert_clip(v1b, Clip::new_media(a2, 8 * SEC, 10 * SEC, 0), InsertMode::Strict).unwrap();
    let cb = Clip::new_media(b2, 4 * SEC, 6 * SEC, 2 * SEC);
    let cb_id = cb.id;
    store2.insert_clip(v1b, cb, InsertMode::Strict).unwrap();
    store2
        .dispatch(
            "transición",
            vec![ue_core::Action::SetClipTransition {
                clip_id: cb_id,
                transition: Some(TransitionRef {
                    effect_id: "core.crossfade".into(),
                    duration: 1 * SEC,
                    params: Default::default(),
                }),
            }],
        )
        .unwrap();
    let edl2 = build_video_edl(&store2.project, seq2).unwrap();
    match &edl2[1] {
        Segment::Source { transition_in, .. } => {
            assert_eq!(*transition_in, None, "sin handle a la izquierda → sin transición");
        }
        other => panic!("{other:?}"),
    }
}

/// Rojo→azul con crossfade de 1 s: la duración total se conserva y el punto
/// medio de la transición es una mezcla de ambos.
#[test]
fn crossfade_export_blends_and_keeps_duration() {
    let Some(dir) = media_dir() else { return };
    for (name, color) in [("red.mp4", "red"), ("blue.mp4", "blue")] {
        let st = Command::new(ue_media::ffmpeg_bin())
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("color=c={color}:s=640x360:d=4:r=30"))
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
            .arg(dir.join(name))
            .status()
            .unwrap();
        assert!(st.success());
    }
    let mut project = Project::new("xfade-test");
    let seq_id = project.active_sequence;
    let red = ue_media::import_file(&dir.join("red.mp4")).unwrap();
    let blue = ue_media::import_file(&dir.join("blue.mp4")).unwrap();
    let (rid, bid) = (red.id, blue.id);
    project.assets.push(red);
    project.assets.push(blue);
    let v1 = project
        .sequence(seq_id)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    // rojo [0.5..3.5) en t=0; azul [0.5..3.5) en t=3 → hay handles a ambos lados
    store.insert_clip(v1, Clip::new_media(rid, 500_000, 3_500_000, 0), InsertMode::Strict).unwrap();
    let cb = Clip::new_media(bid, 500_000, 3_500_000, 3 * SEC);
    let cb_id = cb.id;
    store.insert_clip(v1, cb, InsertMode::Strict).unwrap();
    store
        .dispatch(
            "transición",
            vec![ue_core::Action::SetClipTransition {
                clip_id: cb_id,
                transition: Some(TransitionRef {
                    effect_id: "core.crossfade".into(),
                    duration: 1 * SEC,
                    params: Default::default(),
                }),
            }],
        )
        .unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-xfade-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    let meta = ffprobe_json(&out);
    let dur: f64 = meta["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((5.9..=6.2).contains(&dur), "6 s exactos pese al crossfade, fue {dur}");

    // t=1.5: rojo puro; t=4.5: azul puro; t=3.0 (centro de la transición): mezcla
    let (r, _g, b) = pixel_at(&out, 1.5, 960, 540);
    assert!(r > 180 && b < 60, "rojo puro, fue r={r} b={b}");
    let (r, _g, b) = pixel_at(&out, 4.5, 960, 540);
    assert!(b > 180 && r < 60, "azul puro, fue r={r} b={b}");
    let (r, _g, b) = pixel_at(&out, 3.0, 960, 540);
    assert!(
        (50..=200).contains(&(r as i32)) && (50..=200).contains(&(b as i32)),
        "mezcla en el centro del fundido, fue r={r} b={b}"
    );
}

// ---------------------------------------------------------------------------
// Títulos quemados en el export (drawtext)
// ---------------------------------------------------------------------------

#[test]
fn text_clips_burn_into_export() {
    let Some(dir) = media_dir() else { return };
    // base: video negro de 3 s
    let src = dir.join("black.mp4");
    let st = Command::new(ue_media::ffmpeg_bin())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:d=3:r=30"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());

    let mut project = Project::new("text-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&src).unwrap();
    let aid = asset.id;
    project.assets.push(asset);
    // V2 encima para el título
    let seq = project.sequence_mut(seq_id).unwrap();
    seq.tracks.push(Track::new(TrackKind::Video, "V2"));
    let v2 = seq.tracks.last().unwrap().id;
    let v1 = seq
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video && t.name == "V1")
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    store.insert_clip(v1, Clip::new_media(aid, 0, 3 * SEC, 0), InsertMode::Strict).unwrap();
    // título centrado, blanco, tamaño 120 (grande para el muestreo), 1..3 s
    let mut title = Clip::new_text("HOLA MUNDO", 1 * SEC, 2 * SEC);
    if let ClipPayload::Text { style, .. } = &mut title.payload {
        style.size = 120.0;
    }
    store.insert_clip(v2, title, InsertMode::Strict).unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-text-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    // muestrear una banda central en t=2 (título activo) buscando píxeles claros
    let bright_at = |t: f64| -> usize {
        let mut bright = 0;
        for x in (600..1300).step_by(20) {
            let (r, g, b) = pixel_at(&out, t, x, 540);
            if r as u32 + g as u32 + b as u32 > 380 {
                bright += 1;
            }
        }
        bright
    };
    assert!(bright_at(2.0) >= 3, "el título es visible en t=2 ({} muestras claras)", bright_at(2.0));
    assert_eq!(bright_at(0.5), 0, "antes del título todo es negro");
}

/// Subtítulos automáticos: un TranscriptDoc con dos frases → clip Subtitles
/// sobre video negro → cada frase aparece en su rango (banda inferior) y no fuera.
#[test]
fn auto_subtitles_burn_per_segment() {
    let Some(dir) = media_dir() else { return };
    let src = dir.join("black_subs.mp4"); // archivo propio: evita carreras con otros tests
    let st = Command::new(ue_media::ffmpeg_bin())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:d=3:r=30"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());

    let mut project = Project::new("subs-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&src).unwrap();
    let aid = asset.id;
    project.assets.push(asset);
    // transcripción sintética: "primera frase" [0.2..1.2s), "segunda" [1.8..2.6s)
    let doc = TranscriptDoc {
        id: Id::new(),
        asset_id: aid,
        language: "es".into(),
        model: "test".into(),
        words: vec![],
        segments: vec![
            ue_core::model::Segment {
                text: "primera frase".into(),
                start_us: 200_000,
                end_us: 1_200_000,
                word_range: (0, 0),
                emotion: None,
                volume_rms: 0.0,
            },
            ue_core::model::Segment {
                text: "segunda".into(),
                start_us: 1_800_000,
                end_us: 2_600_000,
                word_range: (0, 0),
                emotion: None,
                volume_rms: 0.0,
            },
        ],
        global_avg_volume: 0.0,
    };
    let doc_id = doc.id;
    project.transcripts.push(doc);
    let seq = project.sequence_mut(seq_id).unwrap();
    seq.tracks.push(Track::new(TrackKind::Video, "V2"));
    let v2 = seq.tracks.last().unwrap().id;
    let v1 = seq
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video && t.name == "V1")
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    store.insert_clip(v1, Clip::new_media(aid, 0, 3 * SEC, 0), InsertMode::Strict).unwrap();
    let mut style = TextStyle::default();
    style.size = 90.0;
    style.y_offset = 380.0;
    let subs = Clip {
        id: Id::new(),
        payload: ClipPayload::Subtitles {
            transcript_id: doc_id,
            style,
            mode: SubtitleMode::Phrase,
        },
        start: 0,
        duration: 3 * SEC,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
    };
    store.insert_clip(v2, subs, InsertMode::Strict).unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-subs-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    // banda de subtítulos: y = 540 + 380 = 920 (centro del texto)
    let bright_at = |t: f64| -> usize {
        let mut n = 0;
        for x in (500..1400).step_by(25) {
            let (r, g, b) = pixel_at(&out, t, x, 920);
            if r as u32 + g as u32 + b as u32 > 380 {
                n += 1;
            }
        }
        n
    };
    assert!(bright_at(0.7) >= 3, "primera frase visible en t=0.7 ({})", bright_at(0.7));
    assert_eq!(bright_at(1.5), 0, "hueco entre frases sin texto");
    assert!(bright_at(2.2) >= 2, "segunda frase visible en t=2.2 ({})", bright_at(2.2));
}

// ---------------------------------------------------------------------------
// Modo vertical (core.vertical_fill)
// ---------------------------------------------------------------------------

/// La secuencia vertical exporta a 1080x1920 y el fondo desenfocado llena la
/// parte superior (no hay letterbox negro), mientras el centro es el video.
#[test]
fn vertical_fill_export_has_blurred_background() {
    let Some(dir) = media_dir() else { return };
    let mut project = Project::new("vertical-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&dir.join("counter.mp4")).unwrap();
    let aid = asset.id;
    project.assets.push(asset);
    // secuencia vertical con el efecto en el clip (lo que produce el wizard)
    let seq = project.sequence_mut(seq_id).unwrap();
    seq.resolution = (1080, 1920);
    let v1 = seq
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    let mut clip = Clip::new_media(aid, 0, 3 * SEC, 0);
    clip.effects.push(EffectInstance {
        effect_id: "core.vertical_fill".into(),
        enabled: true,
        params: Default::default(),
        color_params: Default::default(),
    });
    store.insert_clip(v1, clip, InsertMode::Strict).unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-vertical-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    let meta = ffprobe_json(&out);
    let v = meta["streams"].as_array().unwrap().iter().find(|s| s["codec_type"] == "video").unwrap();
    assert_eq!((v["width"].as_i64(), v["height"].as_i64()), (Some(1080), Some(1920)));

    // banda superior (y=200): fondo desenfocado ⇒ NO negro
    let mut top_bright = 0;
    for x in (100..1000).step_by(80) {
        let (r, g, b) = pixel_at_w(&out, 1.0, x, 200, 1080);
        if r as u32 + g as u32 + b as u32 > 90 {
            top_bright += 1;
        }
    }
    assert!(top_bright >= 6, "el fondo desenfocado llena arriba ({top_bright}/12)");
    // centro (y=960): el video real (testsrc tiene colores saturados)
    let (r, g, b) = pixel_at_w(&out, 1.0, 540, 960, 1080);
    assert!(r as u32 + g as u32 + b as u32 > 120, "centro con contenido: rgb({r},{g},{b})");
}

// ---------------------------------------------------------------------------
// Avatar reactivo (movie+overlay por segmento)
// ---------------------------------------------------------------------------

/// Avatar con dos emociones (calm=azul, angry=rojo) sobre el video: la esquina
/// inferior derecha muestra azul durante el segmento calm y rojo durante angry;
/// el resto del frame sigue siendo el video base.
#[test]
fn avatar_overlay_switches_emotion_per_segment() {
    let Some(dir) = media_dir() else { return };
    for (name, color) in [("avatar_calm.mp4", "blue"), ("avatar_angry.mp4", "red")] {
        let st = Command::new(ue_media::ffmpeg_bin())
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("color=c={color}:s=160x120:d=2:r=24"))
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
            .arg(dir.join(name))
            .status()
            .unwrap();
        assert!(st.success());
    }

    let mut project = Project::new("avatar-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&dir.join("counter.mp4")).unwrap();
    let aid = asset.id;
    project.assets.push(asset);
    // transcript con emociones ya clasificadas
    project.transcripts.push(TranscriptDoc {
        id: Id::new(),
        asset_id: aid,
        language: "es".into(),
        model: "t".into(),
        words: vec![],
        segments: vec![
            ue_core::model::Segment {
                text: "tranquilo".into(),
                start_us: 200_000,
                end_us: 1_800_000,
                word_range: (0, 0),
                emotion: Some("calm".into()),
                volume_rms: 1.0,
            },
            ue_core::model::Segment {
                text: "enfadado".into(),
                start_us: 2_200_000,
                end_us: 3_800_000,
                word_range: (0, 0),
                emotion: Some("angry".into()),
                volume_rms: 1.5,
            },
        ],
        global_avg_volume: 1.2,
    });
    let seq = project.sequence_mut(seq_id).unwrap();
    seq.tracks.push(Track::new(TrackKind::Video, "V2"));
    let v2 = seq.tracks.last().unwrap().id;
    let v1 = seq
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video && t.name == "V1")
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    store.insert_clip(v1, Clip::new_media(aid, 0, 4 * SEC, 0), InsertMode::Strict).unwrap();
    let mut avatars = std::collections::BTreeMap::new();
    avatars.insert("calm".to_string(), dir.join("avatar_calm.mp4").to_string_lossy().into_owned());
    avatars.insert("angry".to_string(), dir.join("avatar_angry.mp4").to_string_lossy().into_owned());
    let avatar = Clip {
        id: Id::new(),
        payload: ClipPayload::Avatar { driver_asset: aid, avatars, shake_factor: 1.0, scale: 0.3 },
        start: 0,
        duration: 4 * SEC,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
    };
    store.insert_clip(v2, avatar, InsertMode::Strict).unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-avatar-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    // avatar: aw=576, x∈[1320,1896], alto=432 → y∈[624,1056]; muestrear el centro
    let sample = |t: f64| pixel_at(&out, t, 1600, 850);
    let (r, g, b) = sample(1.0); // calm → azul
    assert!(b > 120 && r < 90, "calm=azul en t=1: rgb({r},{g},{b})");
    let (r, g, b) = sample(3.0); // angry → rojo
    assert!(r > 120 && b < 90, "angry=rojo en t=3: rgb({r},{g},{b})");
    // fuera del avatar el video base sigue visible (testsrc: no negro)
    let (r, g, b) = pixel_at(&out, 1.0, 400, 300);
    assert!(r as u32 + g as u32 + b as u32 > 100, "el video base sigue debajo");
}

/// Velocidad 2× real: el contador de testsrc avanza el doble por segundo de
/// salida, la duración se reduce a la mitad y el audio lleva atempo (pitch OK).
#[test]
fn speed_2x_export_halves_duration_and_doubles_counter() {
    let Some(dir) = media_dir() else { return };
    let mut project = Project::new("speed-test");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&dir.join("counter.mp4")).unwrap();
    let aid = asset.id;
    project.assets.push(asset);
    let v1 = project
        .sequence(seq_id)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ProjectStore::new(project);
    // fuente [0..6s) a 2× → 3 s de salida
    let clip_id = store
        .insert_clip(v1, Clip::new_media(aid, 0, 6 * SEC, 0), InsertMode::Strict)
        .unwrap();
    store.set_clip_speed(clip_id, 2.0).unwrap();
    assert_eq!(store.project.clip(clip_id).unwrap().duration, 3 * SEC);

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-speed-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    let meta = ffprobe_json(&out);
    let dur: f64 = meta["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((2.9..=3.2).contains(&dur), "≈3 s de salida, fue {dur}");
    assert!(
        meta["streams"].as_array().unwrap().iter().any(|s| s["codec_type"] == "audio"),
        "el audio sobrevive con atempo"
    );

    // el contador (dígito grande del testsrc) en t=1 de salida debe ser "2"
    // (fuente 2s), y en t=2.5 debe ser "5". Verificación visual: extraer frames.
    let frames_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-speed-frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    for (name, t) in [("salida2x_1s", 1.0f64), ("salida2x_2.5s", 2.5)] {
        let st = Command::new(ue_media::ffmpeg_bin())
            .args(["-y", "-v", "error", "-ss", &t.to_string(), "-i"])
            .arg(&out)
            .args(["-frames:v", "1"])
            .arg(frames_dir.join(format!("{name}.jpg")))
            .status()
            .unwrap();
        assert!(st.success());
    }
    eprintln!("frames de velocidad en {}", frames_dir.display());
}

/// Modo palabra a palabra: cada palabra aparece sola en su instante exacto.
#[test]
fn word_mode_subtitles_burn_per_word() {
    let Some(dir) = media_dir() else { return };
    let src = dir.join("black_words.mp4");
    let st = Command::new(ue_media::ffmpeg_bin())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:d=3:r=30"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());

    let mut project = Project::new("word-subs");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&src).unwrap();
    let aid = asset.id;
    project.assets.push(asset);
    let doc_id = Id::new();
    project.transcripts.push(TranscriptDoc {
        id: doc_id,
        asset_id: aid,
        language: "es".into(),
        model: "t".into(),
        words: vec![
            Word { text: "UNO".into(), start_us: 300_000, end_us: 800_000, confidence: 1.0, rejected: false },
            Word { text: "DOS".into(), start_us: 1_500_000, end_us: 2_000_000, confidence: 1.0, rejected: false },
            Word { text: "IGNORADA".into(), start_us: 2_300_000, end_us: 2_600_000, confidence: 1.0, rejected: true },
        ],
        segments: vec![],
        global_avg_volume: 0.0,
    });
    let seq = project.sequence_mut(seq_id).unwrap();
    seq.tracks.push(Track::new(TrackKind::Video, "V2"));
    let v2 = seq.tracks.last().unwrap().id;
    let v1 = seq.tracks.iter().find(|t| t.kind == TrackKind::Video && t.name == "V1").unwrap().id;
    let mut store = ProjectStore::new(project);
    store.insert_clip(v1, Clip::new_media(aid, 0, 3 * SEC, 0), InsertMode::Strict).unwrap();
    let subs = Clip {
        id: Id::new(),
        payload: ClipPayload::Subtitles {
            transcript_id: doc_id,
            style: TextStyle { size: 80.0, y_offset: 380.0, ..Default::default() },
            mode: SubtitleMode::Word,
        },
        start: 0,
        duration: 3 * SEC,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
    };
    store.insert_clip(v2, subs, InsertMode::Strict).unwrap();

    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-wordsubs-out.mp4");
    export_sequence(&store.project, seq_id, dir, &out, &ExportSettings::default()).unwrap();

    let bright_at = |t: f64| -> usize {
        let mut n = 0;
        for y in [890u32, 920, 950] {
            for x in (700..1300).step_by(20) {
                let (r, g, b) = pixel_at(&out, t, x, y);
                if r as u32 + g as u32 + b as u32 > 380 { n += 1; }
            }
        }
        n
    };
    assert!(bright_at(0.5) >= 2, "UNO visible en t=0.5 ({})", bright_at(0.5));
    assert_eq!(bright_at(1.1), 0, "hueco entre palabras limpio");
    assert!(bright_at(1.7) >= 2, "DOS visible en t=1.7 ({})", bright_at(1.7));
    assert_eq!(bright_at(2.4), 0, "las palabras rechazadas no se queman");
}
