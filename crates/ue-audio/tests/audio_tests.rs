//! Tests de ue-audio: parser WAV, mezclador con señales sintéticas exactas,
//! colección de clips audibles, conformado real (ffmpeg) y salida cpal
//! (con salto elegante si no hay dispositivo).

use std::path::{Path, PathBuf};

use ue_audio::items::{collect_specs, load_items};
use ue_audio::mixer::{db_to_linear, fill, mix_frame, MixItem};
use ue_audio::wav::WavMap;
use ue_audio::{us_to_frames, RATE};
use ue_core::model::*;
use ue_core::ops::InsertMode;
use ue_core::ProjectStore;

const SEC: i64 = 1_000_000;

fn tmp(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ue-audio-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// WAV estéreo 48k con generador por frame.
fn write_wav(name: &str, frames: i64, gen: impl Fn(i64) -> (i16, i16)) -> PathBuf {
    let path = tmp(name);
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for i in 0..frames {
        let (l, r) = gen(i);
        w.write_sample(l).unwrap();
        w.write_sample(r).unwrap();
    }
    w.finalize().unwrap();
    path
}

fn dc_item(path: &PathBuf, timeline_start: i64, len: i64) -> MixItem {
    MixItem {
        wav: WavMap::open(path).unwrap(),
        timeline_start,
        src_in: 0,
        len,
        speed: 1.0,
        gain: 1.0,
        fade_in: 0,
        fade_out: 0,
    }
}

// ---------------------------------------------------------------------------
// WAV parser
// ---------------------------------------------------------------------------

#[test]
fn wav_roundtrip_and_bounds() {
    let path = write_wav("ramp.wav", 1000, |i| ((i * 16) as i16, (-i * 16) as i16));
    let wav = WavMap::open(&path).unwrap();
    assert_eq!(wav.frames(), 1000);
    assert_eq!(wav.sample_rate, RATE);
    let (l, r) = wav.frame(10);
    assert!((l - 160.0 / 32768.0).abs() < 1e-6);
    assert!((r + 160.0 / 32768.0).abs() < 1e-6);
    assert_eq!(wav.frame(-1), (0.0, 0.0));
    assert_eq!(wav.frame(1000), (0.0, 0.0), "fuera de rango → silencio");
}

// ---------------------------------------------------------------------------
// Mezclador
// ---------------------------------------------------------------------------

#[test]
fn gain_in_db_is_applied() {
    let path = write_wav("dc_half.wav", 100, |_| (16384, 16384)); // 0.5
    let mut item = dc_item(&path, 0, 100);
    item.gain = db_to_linear(-6.0206); // ≈ 0.5×
    let (l, _) = mix_frame(&[item], 50);
    assert!((l - 0.25).abs() < 1e-3, "0.5 × -6dB ≈ 0.25, fue {l}");
}

#[test]
fn overlapping_items_sum_and_clamp() {
    let path = write_wav("dc_04.wav", 100, |_| (13107, 13107)); // 0.4
    let a = dc_item(&path, 0, 100);
    let b = dc_item(&path, 0, 100);
    let (l, _) = mix_frame(&[a, b], 10);
    assert!((l - 0.8).abs() < 1e-3, "suma 0.4+0.4");

    let path_hot = write_wav("dc_09.wav", 100, |_| (29491, 29491)); // 0.9
    let a = dc_item(&path_hot, 0, 100);
    let b = dc_item(&path_hot, 0, 100);
    let (l, _) = mix_frame(&[a, b], 10);
    assert_eq!(l, 1.0, "clamp a 1.0");
}

#[test]
fn timeline_offset_and_src_in_mapping() {
    // señal rampa exacta: frame i vale i*16
    let path = write_wav("ramp2.wav", 2000, |i| ((i * 16) as i16, (i * 16) as i16));
    let item = MixItem {
        wav: WavMap::open(&path).unwrap(),
        timeline_start: 500,
        src_in: 100,
        len: 300,
        speed: 1.0,
        gain: 1.0,
        fade_in: 0,
        fade_out: 0,
    };
    // antes del clip → silencio
    assert_eq!(mix_frame(&[item], 499).0, 0.0);
    // el clip re-abre el wav para más asserts
    let item = MixItem {
        wav: WavMap::open(&path).unwrap(),
        timeline_start: 500,
        src_in: 100,
        len: 300,
        speed: 1.0,
        gain: 1.0,
        fade_in: 0,
        fade_out: 0,
    };
    // frame 500 del timeline = frame 100 de la fuente = 1600/32768
    let expect = |src: i64| (src * 16) as f32 / 32768.0;
    assert!((mix_frame(&[item], 500).0 - expect(100)).abs() < 1e-6);
    let item = MixItem {
        wav: WavMap::open(&path).unwrap(),
        timeline_start: 500,
        src_in: 100,
        len: 300,
        speed: 1.0,
        gain: 1.0,
        fade_in: 0,
        fade_out: 0,
    };
    // último frame del clip: timeline 799 → fuente 399; y 800 ya es silencio
    assert!((mix_frame(&[item], 799).0 - expect(399)).abs() < 1e-6);
}

#[test]
fn fades_ramp_linearly() {
    let path = write_wav("dc_full.wav", 1000, |_| (32767, 32767)); // ≈1.0
    let item = MixItem {
        wav: WavMap::open(&path).unwrap(),
        timeline_start: 0,
        src_in: 0,
        len: 1000,
        speed: 1.0,
        gain: 1.0,
        fade_in: 200,
        fade_out: 200,
    };
    let items = [item];
    assert_eq!(mix_frame(&items, 0).0, 0.0, "inicio del fade-in");
    let mid_in = mix_frame(&items, 100).0;
    assert!((mid_in - 0.5).abs() < 0.01, "mitad del fade-in ≈ 0.5, fue {mid_in}");
    let plateau = mix_frame(&items, 500).0;
    assert!(plateau > 0.99, "meseta a ganancia completa");
    let mid_out = mix_frame(&items, 900).0;
    assert!((mid_out - 0.5).abs() < 0.01, "mitad del fade-out ≈ 0.5, fue {mid_out}");
}

#[test]
fn fill_is_contiguous() {
    let path = write_wav("ramp3.wav", 1000, |i| ((i * 16) as i16, (i * 16) as i16));
    let items = [dc_item(&path, 0, 1000)];
    let mut buf = vec![0f32; 20]; // 10 frames estéreo
    fill(&items, 100, &mut buf);
    for k in 0..10i64 {
        let expect = ((100 + k) * 16) as f32 / 32768.0;
        assert!((buf[(k * 2) as usize] - expect).abs() < 1e-6);
    }
}

// ---------------------------------------------------------------------------
// Colección desde el proyecto
// ---------------------------------------------------------------------------

fn asset(kind: MediaKind, channels: u32, dur_s: i64) -> MediaAsset {
    MediaAsset {
        id: Id::new(),
        kind,
        path: format!("{kind:?}.dat"),
        content_hash: format!("h{channels}{dur_s}"),
        probe: ProbeInfo {
            duration_us: dur_s * SEC,
            fps: None,
            width: 0,
            height: 0,
            rotation: 0,
            vcodec: None,
            acodec: Some("aac".into()),
            audio_channels: channels,
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

#[test]
fn collect_respects_mute_solo_and_video_audio() {
    let mut p = Project::new("t");
    let seq_id = p.active_sequence;
    let music = asset(MediaKind::Audio, 2, 60);
    let video_with_audio = asset(MediaKind::Video, 2, 60);
    let video_silent = asset(MediaKind::Video, 0, 60);
    let (m, va, vs) = (music.id, video_with_audio.id, video_silent.id);
    p.assets.extend([music, video_with_audio, video_silent]);
    let atrack = p.sequence(seq_id).unwrap().tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap().id;
    let vtrack = p.sequence(seq_id).unwrap().tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
    let mut store = ProjectStore::new(p);
    store.insert_clip(atrack, Clip::new_media(m, 0, 5 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(vtrack, Clip::new_media(va, 0, 5 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(vtrack, Clip::new_media(vs, 0, 5 * SEC, 6 * SEC), InsertMode::Strict).unwrap();

    // ambos con audio entran; el video sin audio no
    let specs = collect_specs(&store.project, seq_id);
    assert_eq!(specs.len(), 2);

    // mute de la pista de audio → solo queda el del video
    store
        .dispatch("mute", vec![ue_core::Action::SetTrackProp {
            track_id: atrack,
            prop: ue_core::action::TrackProp::Muted(true),
        }])
        .unwrap();
    let specs = collect_specs(&store.project, seq_id);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].asset_id, va);

    // solo en la pista de audio (desmuteada) → solo la música
    store
        .dispatch("unmute+solo", vec![
            ue_core::Action::SetTrackProp { track_id: atrack, prop: ue_core::action::TrackProp::Muted(false) },
            ue_core::Action::SetTrackProp { track_id: atrack, prop: ue_core::action::TrackProp::Solo(true) },
        ])
        .unwrap();
    let specs = collect_specs(&store.project, seq_id);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].asset_id, m);
}

#[test]
fn load_items_skips_missing_conform() {
    let mut p = Project::new("t");
    let seq_id = p.active_sequence;
    let music = asset(MediaKind::Audio, 2, 60);
    let mid = music.id;
    p.assets.push(music);
    let atrack = p.sequence(seq_id).unwrap().tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap().id;
    let mut store = ProjectStore::new(p);
    store.insert_clip(atrack, Clip::new_media(mid, 0, 5 * SEC, 0), InsertMode::Strict).unwrap();
    let specs = collect_specs(&store.project, seq_id);
    let (items, skipped) = load_items(&store.project, &specs, |_| None);
    assert!(items.is_empty());
    assert_eq!(skipped, vec![mid]);
}

// ---------------------------------------------------------------------------
// Conformado real (ffmpeg) y reproducción (cpal) — con salto elegante
// ---------------------------------------------------------------------------

#[test]
fn conform_produces_valid_48k_stereo_wav() {
    let ff_ok = std::process::Command::new(ue_media::ffmpeg_bin())
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ff_ok {
        eprintln!("AVISO: sin ffmpeg; test de conformado saltado");
        return;
    }
    // fuente deliberadamente distinta: 22.05 kHz mono
    let src = tmp("tone_22k.wav");
    let st = std::process::Command::new(ue_media::ffmpeg_bin())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=2:sample_rate=22050"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(st.success());

    let out = tmp("conformed/audio.wav");
    ue_media::conform_audio(&src, &out).unwrap();
    let wav = WavMap::open(&out).unwrap();
    assert_eq!(wav.sample_rate, RATE);
    let dur_frames = wav.frames();
    assert!((dur_frames - 2 * RATE as i64).abs() < RATE as i64 / 10, "≈2 s, fue {dur_frames}");
    // idempotente: no re-conforma si existe
    ue_media::conform_audio(&src, &out).unwrap();
    // hay señal de verdad (la fuente `sine` de ffmpeg es de amplitud baja,
    // ~0.05: el umbral distingue señal de silencio, no niveles)
    let mean_sq: f32 = (0..dur_frames.min(48000))
        .map(|i| wav.frame(i).0.powi(2))
        .sum::<f32>()
        / 48000.0;
    assert!(mean_sq > 1e-4, "el tono tiene energía (mean²={mean_sq})");
}

#[test]
fn player_clock_advances_if_device_available() {
    match ue_audio::player::Player::new() {
        Err(e) => eprintln!("AVISO: sin dispositivo de audio ({e}); test de player saltado"),
        Ok(player) => {
            player.set_items(vec![], 1);
            player.play(1 * SEC);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let pos = player.pause();
            let advanced = pos - 1 * SEC;
            assert!(
                (100_000..=900_000).contains(&advanced),
                "el reloj de audio avanzó ~300 ms, fue {advanced} µs"
            );
            // seek re-posiciona
            player.seek(10 * SEC);
            assert_eq!(player.position_us(), 10 * SEC);
            let _ = us_to_frames(0); // silencia unused en builds sin asserts
        }
    }
}
