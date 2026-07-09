//! Tests del dispatcher MCP (handle_rpc es puro sobre AppState: sin HTTP).

use serde_json::{json, Value};
use ue_core::model::*;
use ue_core::ops::InsertMode;
use ue_tauri_lib::mcp::handle_rpc;
use ue_tauri_lib::AppState;

const SEC: i64 = 1_000_000;

fn rpc(state: &AppState, method: &str, params: Value) -> Value {
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    handle_rpc(state, &req).expect("con id siempre hay respuesta")
}

/// Texto del primer content de un tool result, parseado como JSON.
fn tool_json(resp: &Value) -> Value {
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .expect("tool result con texto");
    serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
}

fn state_with_clip() -> (AppState, Id) {
    let state = AppState::new_default();
    let clip_id;
    {
        let mut store = state.store.lock().unwrap();
        let asset = MediaAsset {
            id: Id::new(),
            kind: MediaKind::Video,
            path: "media/x.mp4".into(),
            content_hash: "xxh3:x".into(),
            probe: ProbeInfo {
                duration_us: 30 * SEC,
                fps: Some((30, 1)),
                width: 1920,
                height: 1080,
                rotation: 0,
                vcodec: None,
                acodec: None,
                audio_channels: 0,
                vfr: false,
            },
            proxy: None,
            audio_conform: None,
            peaks: None,
            thumbnails: None,
            transcript: None,
            offline: false,
        };
        let aid = asset.id;
        store.project.assets.push(asset);
        let vtrack = store
            .project
            .sequence(store.project.active_sequence)
            .unwrap()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        let clip = Clip::new_media(aid, 0, 10 * SEC, 0);
        clip_id = clip.id;
        store.insert_clip(vtrack, clip, InsertMode::Strict).unwrap();
    }
    (state, clip_id)
}

#[test]
fn initialize_and_tools_list() {
    let state = AppState::new_default();
    let init = rpc(&state, "initialize", json!({ "protocolVersion": "2025-06-18" }));
    assert_eq!(init.pointer("/result/protocolVersion").unwrap(), "2025-06-18");
    assert_eq!(init.pointer("/result/serverInfo/name").unwrap(), "ubereditor");

    let tools = rpc(&state, "tools/list", json!({}));
    let list = tools.pointer("/result/tools").unwrap().as_array().unwrap();
    assert!(list.len() >= 10, "hay al menos 10 herramientas, fueron {}", list.len());
    assert!(list.iter().any(|t| t["name"] == "split_clip"));

    // notificación → sin respuesta
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert!(handle_rpc(&state, &note).is_none());

    // método desconocido → error JSON-RPC
    let err = rpc(&state, "no/existe", json!({}));
    assert_eq!(err.pointer("/error/code").unwrap(), -32601);
}

#[test]
fn summary_and_timeline_reflect_project() {
    let (state, _clip) = state_with_clip();
    let resp = rpc(&state, "tools/call", json!({ "name": "get_project_summary", "arguments": {} }));
    let summary = tool_json(&resp);
    assert_eq!(summary["assets"], 1);
    assert_eq!(summary["sequence"]["duration_us"], 10 * SEC);

    let resp = rpc(&state, "tools/call", json!({ "name": "get_timeline", "arguments": {} }));
    let timeline = tool_json(&resp);
    let clips = timeline["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["clips"].as_array().unwrap().len())
        .sum::<usize>();
    assert_eq!(clips, 1);
}

#[test]
fn split_edit_and_undo_via_mcp() {
    let (state, clip_id) = state_with_clip();
    let resp = rpc(
        &state,
        "tools/call",
        json!({ "name": "split_clip", "arguments": { "clip_id": clip_id.to_string(), "t_us": 4 * SEC } }),
    );
    let result = tool_json(&resp);
    assert!(result["left"].is_string() && result["right"].is_string());
    {
        let store = state.store.lock().unwrap();
        let seq = store.project.sequence(store.project.active_sequence).unwrap();
        let total: usize = seq.tracks.iter().map(|t| t.clips.len()).sum();
        assert_eq!(total, 2, "el split via MCP partió el clip");
    }

    let resp = rpc(&state, "tools/call", json!({ "name": "undo", "arguments": {} }));
    assert!(tool_json(&resp)["undone"].is_string());
    {
        let store = state.store.lock().unwrap();
        let seq = store.project.sequence(store.project.active_sequence).unwrap();
        let total: usize = seq.tracks.iter().map(|t| t.clips.len()).sum();
        assert_eq!(total, 1, "undo via MCP restauró el clip");
    }

    // errores de herramienta van como isError, no como error JSON-RPC
    let resp = rpc(
        &state,
        "tools/call",
        json!({ "name": "split_clip", "arguments": { "clip_id": "no-es-un-id", "t_us": 1 } }),
    );
    assert_eq!(resp.pointer("/result/isError").unwrap(), true);
}

/// remove_silences de punta a punta: video con tono-silencio-tono real,
/// conformado incluido. Verifica cortes y duración final.
#[test]
fn remove_silences_via_mcp_cuts_the_gap() {
    let ff_ok = std::process::Command::new(ue_media::ffmpeg_bin())
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ff_ok {
        eprintln!("AVISO: sin ffmpeg; test saltado");
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-mcp-media");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("tono_silencio_tono.mp4");
    // 2s tono + 2s silencio + 2s tono, con video de color
    let st = std::process::Command::new(ue_media::ffmpeg_bin())
        .args([
            "-y", "-v", "error",
            "-f", "lavfi", "-i", "color=c=gray:s=320x180:d=6:r=30",
            "-f", "lavfi", "-i",
            "aevalsrc='if(lt(mod(t,6),2)*0.4+gte(mod(t,6),4)*0.4, 0.4*sin(880*2*PI*t), 0)':d=6",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-shortest",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());

    let state = AppState::new_default();
    let clip_id;
    {
        let mut store = state.store.lock().unwrap();
        let mut asset = ue_media::import_file(&src).unwrap();
        // conformar el audio a mano (en la app lo hace el job de import)
        let conform = dir.join("conform.wav");
        ue_media::conform_audio(&src, &conform).unwrap();
        asset.audio_conform = Some(conform.to_string_lossy().into_owned());
        let aid = asset.id;
        store.project.assets.push(asset);
        let vtrack = store
            .project
            .sequence(store.project.active_sequence)
            .unwrap()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        let clip = Clip::new_media(aid, 0, 6 * SEC, 0);
        clip_id = clip.id;
        store.insert_clip(vtrack, clip, InsertMode::Strict).unwrap();
    }

    let resp = rpc(
        &state,
        "tools/call",
        json!({ "name": "remove_silences", "arguments": { "clip_id": clip_id.to_string() } }),
    );
    let result = tool_json(&resp);
    assert_eq!(result["removed"], 1, "un silencio central: {result}");
    let removed_us = result["removed_us"].as_i64().unwrap();
    assert!((1_200_000..=2_200_000).contains(&removed_us), "≈2 s menos padding: {removed_us}");

    let store = state.store.lock().unwrap();
    let seq = store.project.sequence(store.project.active_sequence).unwrap();
    let dur = seq.duration_us();
    assert!((3_800_000..=4_900_000).contains(&dur), "duración final ≈ 6s - silencio: {dur}");
    let clips: usize = seq.tracks.iter().map(|t| t.clips.len()).sum();
    assert_eq!(clips, 2, "el clip quedó partido en dos alrededor del silencio");
}

/// El desenlace rompe el grupo entero y es deshacible.
#[test]
fn unlink_breaks_group_with_undo() {
    let state = AppState::new_default();
    let (v_id, group);
    {
        let mut store = state.store.lock().unwrap();
        let asset = MediaAsset {
            id: Id::new(),
            kind: MediaKind::Video,
            path: "x.mp4".into(),
            content_hash: "h".into(),
            probe: ProbeInfo {
                duration_us: 10 * SEC,
                fps: Some((30, 1)),
                width: 640,
                height: 360,
                rotation: 0,
                vcodec: None,
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
        let aid = asset.id;
        store.project.assets.push(asset);
        let seq = store.project.sequence(store.project.active_sequence).unwrap();
        let vt = seq.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
        let at = seq.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap().id;
        group = Id::new();
        let mut vc = Clip::new_media(aid, 0, 5 * SEC, 0);
        vc.group = Some(group);
        v_id = vc.id;
        let mut ac = Clip::new_media(aid, 0, 5 * SEC, 0);
        ac.group = Some(group);
        store.insert_clip(vt, vc, InsertMode::Strict).unwrap();
        store.insert_clip(at, ac, InsertMode::Strict).unwrap();
    }
    // desenlazar via las mismas acciones que usa el comando
    {
        let mut store = state.store.lock().unwrap();
        let members = ue_core::ops::linked_ids(&store.project, v_id);
        assert_eq!(members.len(), 2);
        let actions = members
            .into_iter()
            .map(|clip_id| ue_core::Action::SetClipGroup { clip_id, group: None })
            .collect();
        store.dispatch("Desenlazar clips", actions).unwrap();
        assert_eq!(ue_core::ops::linked_ids(&store.project, v_id).len(), 1, "grupo roto");
        store.undo().unwrap();
        assert_eq!(ue_core::ops::linked_ids(&store.project, v_id).len(), 2, "undo re-enlaza");
    }
}

/// Portabilidad: guardar relativiza las rutas y limpia cachés; abrir resuelve
/// contra la carpeta del proyecto y marca offline lo que no existe.
#[test]
fn portable_project_roundtrip() {
    use ue_tauri_lib::{make_portable, resolve_project_paths};
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-portable");
    std::fs::create_dir_all(dir.join("media")).unwrap();
    let real = dir.join("media/existe.mp4");
    std::fs::write(&real, b"fake").unwrap();

    let mut project = ue_core::model::Project::new("portable");
    let mk = |path: String| MediaAsset {
        id: Id::new(),
        kind: MediaKind::Video,
        path,
        content_hash: "h".into(),
        probe: ProbeInfo {
            duration_us: SEC,
            fps: Some((30, 1)),
            width: 1,
            height: 1,
            rotation: 0,
            vcodec: None,
            acodec: None,
            audio_channels: 0,
            vfr: false,
        },
        proxy: Some("/tmp/cache/proxy.mp4".into()),
        audio_conform: Some("/tmp/cache/a.wav".into()),
        peaks: None,
        thumbnails: None,
        transcript: None,
        offline: false,
    };
    project.assets.push(mk(real.to_string_lossy().into_owned()));
    project.assets.push(mk("/no/existe/fuera.mp4".into()));

    let portable = make_portable(&project, Some(&dir));
    assert_eq!(portable.assets[0].path, "media/existe.mp4", "bajo el proyecto → relativa");
    assert_eq!(portable.assets[1].path, "/no/existe/fuera.mp4", "fuera → absoluta");
    assert!(portable.assets[0].audio_conform.is_none(), "cachés no viajan");

    let mut reopened = portable.clone();
    resolve_project_paths(&mut reopened, Some(&dir));
    assert_eq!(reopened.assets[0].path, real.to_string_lossy(), "resuelta a absoluta");
    assert!(!reopened.assets[0].offline, "existe → online");
    assert!(reopened.assets[1].offline, "no existe → offline");
}

/// El sufijo de avatar del stream agrupa por emoción y traduce las ventanas
/// al dominio t de la sesión (timeline = tl0 + t/speed).
#[test]
fn avatar_stream_suffix_groups_by_emotion_and_maps_time() {
    use ue_core::model::*;
    use ue_core::ops::InsertMode;

    // dos "videos" de avatar en disco (basta con que existan)
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-avatar-stream");
    std::fs::create_dir_all(&dir).unwrap();
    let calm = dir.join("calm.mp4");
    let angry = dir.join("angry.mp4");
    std::fs::write(&calm, b"x").unwrap();
    std::fs::write(&angry, b"x").unwrap();

    let mut project = Project::new("avatar-stream");
    let seq_id = project.active_sequence;
    // asset conductor ficticio + clip media que lo pone en el timeline 0..10s
    let asset = MediaAsset {
        id: Id::new(),
        kind: MediaKind::Video,
        path: "/no/importa.mp4".into(),
        content_hash: "h".into(),
        probe: ProbeInfo {
            duration_us: 10_000_000,
            fps: Some((30, 1)),
            width: 1920,
            height: 1080,
            rotation: 0,
            vcodec: None,
            acodec: None,
            audio_channels: 0,
            vfr: false,
        },
        proxy: None,
        audio_conform: None,
        peaks: None,
        thumbnails: None,
        transcript: None,
        offline: false,
    };
    let aid = asset.id;
    project.assets.push(asset);
    let doc_id = Id::new();
    project.transcripts.push(TranscriptDoc {
        id: doc_id,
        asset_id: aid,
        language: "es".into(),
        model: "t".into(),
        words: vec![],
        segments: vec![
            Segment {
                text: "tranquilo".into(),
                start_us: 1_000_000,
                end_us: 3_000_000,
                word_range: (0, 0),
                emotion: Some("calm".into()),
                volume_rms: 0.0,
            },
            Segment {
                text: "furioso".into(),
                start_us: 3_000_000,
                end_us: 5_000_000,
                word_range: (0, 0),
                emotion: Some("angry".into()),
                volume_rms: 0.0,
            },
        ],
        global_avg_volume: 0.0,
    });
    let v1 = project
        .sequence(seq_id)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .unwrap()
        .id;
    let mut store = ue_core::ProjectStore::new(project);
    store
        .insert_clip(v1, Clip::new_media(aid, 0, 10_000_000, 0), InsertMode::Strict)
        .unwrap();
    let mut avatars = std::collections::BTreeMap::new();
    avatars.insert("calm".to_string(), calm.to_string_lossy().into_owned());
    avatars.insert("angry".to_string(), angry.to_string_lossy().into_owned());
    let av_clip = Clip {
        id: Id::new(),
        payload: ClipPayload::Avatar {
            driver_asset: aid,
            avatars,
            shake_factor: 0.0,
            scale: 0.25,
        },
        start: 0,
        duration: 10_000_000,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: AudioProps { muted: true, ..Default::default() },
        transition_in: None,
        label_color: None,
        group: None,
    };
    let track2 = Track::new(TrackKind::Video, "V2");
    let v2 = track2.id;
    let seq_len = store.project.sequence(seq_id).unwrap().tracks.len();
    store
        .dispatch(
            "V2",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index: seq_len, track: track2 }],
        )
        .unwrap();
    store.insert_clip(v2, av_clip, InsertMode::Strict).unwrap();

    // forma canónica (tl0=0, speed=1): ventanas absolutas de timeline
    let canonical =
        ue_tauri_lib::avatar_vf_stream_suffix(&store.project, seq_id, 0, 1.0, 960).unwrap();
    // una sola instancia de movie por emoción (2), no por segmento
    assert_eq!(canonical.matches("movie=").count(), 2, "{canonical}");
    assert!(canonical.contains("between(t,1.0000,3.0000)"), "{canonical}");
    assert!(canonical.contains("between(t,3.0000,5.0000)"), "{canonical}");
    // la emoción default (calm, primera) rellena los huecos 0-1 y 5-10
    assert!(canonical.contains("between(t,0.0000,1.0000)"), "{canonical}");
    assert!(canonical.contains("between(t,5.0000,10.0000)"), "{canonical}");

    // sesión abierta en tl0=2s a velocidad 2×: ventana [3,5] → [2,6] en t
    let open =
        ue_tauri_lib::avatar_vf_stream_suffix(&store.project, seq_id, 2_000_000, 2.0, 960).unwrap();
    assert!(open.contains("between(t,2.0000,6.0000)"), "{open}");
    // la ventana [1,3] queda [0,2] (el pasado se clampa a 0)
    assert!(open.contains("between(t,0.0000,2.0000)"), "{open}");
}

/// El grafo del avatar de reproducción corre en ffmpeg real: el avatar rojo
/// aparece en la esquina inferior derecha del stream.
#[test]
fn avatar_stream_graph_runs_in_ffmpeg() {
    use std::process::Command;
    use ue_core::model::*;
    use ue_core::ops::InsertMode;

    let ffmpeg = ue_media::ffmpeg_bin();
    if Command::new(&ffmpeg).arg("-version").output().map(|o| !o.status.success()).unwrap_or(true)
    {
        eprintln!("AVISO: sin ffmpeg; test saltado");
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-avatar-e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("base_black.mp4");
    let avatar = dir.join("avatar_red.mp4");
    for (path, src) in [
        (&base, "color=black:s=640x360:r=30:d=4"),
        (&avatar, "color=red:s=160x90:r=30:d=1"),
    ] {
        if !path.exists() {
            let st = Command::new(&ffmpeg)
                .args(["-y", "-v", "error", "-f", "lavfi", "-i", src])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
                .arg(path)
                .status()
                .unwrap();
            assert!(st.success());
        }
    }

    let mut project = Project::new("avatar-e2e");
    let seq_id = project.active_sequence;
    let asset = ue_media::import_file(&base).unwrap();
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
    let mut store = ue_core::ProjectStore::new(project);
    store.insert_clip(v1, Clip::new_media(aid, 0, 4 * SEC, 0), InsertMode::Strict).unwrap();
    let mut avatars = std::collections::BTreeMap::new();
    avatars.insert("calm".to_string(), avatar.to_string_lossy().into_owned());
    let track2 = Track::new(TrackKind::Video, "V2");
    let v2 = track2.id;
    let n = store.project.sequence(seq_id).unwrap().tracks.len();
    store
        .dispatch(
            "V2",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index: n, track: track2 }],
        )
        .unwrap();
    store
        .insert_clip(
            v2,
            Clip {
                id: Id::new(),
                payload: ClipPayload::Avatar {
                    driver_asset: aid,
                    avatars,
                    shake_factor: 0.0,
                    scale: 0.25,
                },
                start: 0,
                duration: 4 * SEC,
                speed: 1.0,
                effects: vec![],
                transform: Default::default(),
                audio: AudioProps { muted: true, ..Default::default() },
                transition_in: None,
                label_color: None,
                group: None,
            },
            InsertMode::Strict,
        )
        .unwrap();

    let suffix =
        ue_tauri_lib::avatar_vf_stream_suffix(&store.project, seq_id, 0, 1.0, 640).unwrap();
    let vf = format!("null{suffix}");
    // mismo -vf que usa la MjpegSession: un frame a t=1s y verificar la esquina
    let png = dir.join("frame.png");
    let out = Command::new(&ffmpeg)
        .args(["-y", "-v", "error", "-ss", "1", "-i"])
        .arg(&base)
        .args(["-vf", &vf, "-frames:v", "1"])
        .arg(&png)
        .output()
        .unwrap();
    assert!(out.status.success(), "grafo inválido: {}", String::from_utf8_lossy(&out.stderr));
    // píxel en la esquina inferior derecha (donde va el avatar 160x90 @16px)
    let probe = Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&png)
        .args(["-vf", "crop=1:1:600:320", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    let px = &probe.stdout;
    assert!(px.len() >= 3 && px[0] > 180 && px[1] < 80, "avatar rojo en la esquina: {px:?}");
}
