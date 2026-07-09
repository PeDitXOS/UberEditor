//! Tests de integración del ProjectStore: split/trim/move/delete/cut_ranges,
//! atomicidad, undo/redo y propiedad "undo total ≡ estado inicial".

use ue_core::action::{Action, TrackProp};
use ue_core::keyframe::{Interp, Keyframe, KeyframeCurve, Param};
use ue_core::model::*;
use ue_core::ops::InsertMode;
use ue_core::time::US_PER_SEC;
use ue_core::validate::validate;
use ue_core::ProjectStore;

const SEC: i64 = US_PER_SEC;

/// Proyecto fixture: 1 asset de video de 60 s, 1 de audio de 120 s,
/// secuencia 30 fps con V1/A1 (y el asset ya en el pool).
/// Devuelve (store, seq_id, video_track, audio_track, video_asset, audio_asset).
fn fixture() -> (ProjectStore, Id, Id, Id, Id, Id) {
    let mut p = Project::new("Test");
    let seq_id = p.active_sequence;
    let video_asset = MediaAsset {
        id: Id::new(),
        kind: MediaKind::Video,
        path: "media/video.mp4".into(),
        content_hash: "xxh3:v".into(),
        probe: ProbeInfo {
            duration_us: 60 * SEC,
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
    let audio_asset = MediaAsset {
        id: Id::new(),
        kind: MediaKind::Audio,
        path: "media/music.mp3".into(),
        content_hash: "xxh3:a".into(),
        probe: ProbeInfo {
            duration_us: 120 * SEC,
            fps: None,
            width: 0,
            height: 0,
            rotation: 0,
            vcodec: None,
            acodec: Some("mp3".into()),
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
    let va = video_asset.id;
    let aa = audio_asset.id;
    p.assets.push(video_asset);
    p.assets.push(audio_asset);
    let seq = p.sequence(seq_id).unwrap();
    let audio_track = seq.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap().id;
    let video_track = seq.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
    (ProjectStore::new(p), seq_id, video_track, audio_track, va, aa)
}

fn media_src(clip: &Clip) -> (i64, i64) {
    match &clip.payload {
        ClipPayload::Media { src_in, src_out, .. } => (*src_in, *src_out),
        _ => panic!("no es media"),
    }
}

#[test]
fn insert_split_undo_roundtrip() {
    let (mut store, _seq, vtrack, _atrack, va, _aa) = fixture();
    let clip = Clip::new_media(va, 0, 10 * SEC, 0);
    store.insert_clip(vtrack, clip, InsertMode::Strict).unwrap();
    let snapshot = store.project.clone();

    let clip_id = store.project.track(vtrack).unwrap().clips[0].id;
    let (l, r) = store.split_clip(clip_id, 4 * SEC).unwrap();

    {
        let track = store.project.track(vtrack).unwrap();
        assert_eq!(track.clips.len(), 2);
        let (lc, rc) = (&track.clips[0], &track.clips[1]);
        assert_eq!((lc.id, rc.id), (l, r));
        assert_eq!((lc.start, lc.duration), (0, 4 * SEC));
        assert_eq!((rc.start, rc.duration), (4 * SEC, 6 * SEC));
        assert_eq!(media_src(lc), (0, 4 * SEC));
        assert_eq!(media_src(rc), (4 * SEC, 10 * SEC));
    }

    store.undo().unwrap();
    assert_eq!(store.project, snapshot, "undo debe restaurar byte a byte");

    store.redo().unwrap();
    let track = store.project.track(vtrack).unwrap();
    assert_eq!(track.clips.len(), 2);
    assert_eq!(track.clips[0].id, l, "redo reusa los mismos ids");
}

#[test]
fn split_quantizes_to_frame() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    let clip = Clip::new_media(va, 0, 10 * SEC, 0);
    let clip_id = store.insert_clip(vtrack, clip, InsertMode::Strict).unwrap();
    // 1.017 s no es frontera de frame a 30 fps → cuantiza al frame más cercano (31)
    store.split_clip(clip_id, 1_017_000).unwrap();
    let track = store.project.track(vtrack).unwrap();
    let boundary = track.clips[1].start;
    assert_eq!(boundary, ue_core::time::frame_to_time(31, (30, 1)), "corte en el frame 31");
    assert_eq!(
        ue_core::time::quantize_to_frame(boundary, (30, 1)),
        boundary,
        "la frontera es idempotente bajo cuantización"
    );
}

#[test]
fn split_keyframes_preserve_boundary_value() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    let mut clip = Clip::new_media(va, 0, 10 * SEC, 0);
    clip.transform.opacity = Param::Curve(KeyframeCurve::new(vec![
        Keyframe { t: 0, value: 0.0, interp: Interp::Linear },
        Keyframe { t: 10 * SEC, value: 1.0, interp: Interp::Linear },
    ]));
    let clip_id = store.insert_clip(vtrack, clip, InsertMode::Strict).unwrap();
    let (l, r) = store.split_clip(clip_id, 4 * SEC).unwrap();
    let track = store.project.track(vtrack).unwrap();
    let lc = track.clips.iter().find(|c| c.id == l).unwrap();
    let rc = track.clips.iter().find(|c| c.id == r).unwrap();
    // en la frontera ambas mitades valen 0.4
    assert!((lc.transform.opacity.eval(4 * SEC) - 0.4).abs() < 1e-9);
    assert!((rc.transform.opacity.eval(0) - 0.4).abs() < 1e-9);
    // y la derecha sigue llegando a 1.0 en su final
    assert!((rc.transform.opacity.eval(6 * SEC) - 1.0).abs() < 1e-9);
}

#[test]
fn ripple_delete_closes_gap() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    let a = store.insert_clip(vtrack, Clip::new_media(va, 0, 2 * SEC, 0), InsertMode::Strict).unwrap();
    let _b = store.insert_clip(vtrack, Clip::new_media(va, 0, 3 * SEC, 2 * SEC), InsertMode::Strict).unwrap();
    let c = store.insert_clip(vtrack, Clip::new_media(va, 0, 1 * SEC, 5 * SEC), InsertMode::Strict).unwrap();

    // borrar el clip del medio con ripple
    let b_id = store.project.track(vtrack).unwrap().clips[1].id;
    store.delete_clips(&[b_id], true).unwrap();

    let track = store.project.track(vtrack).unwrap();
    assert_eq!(track.clips.len(), 2);
    assert_eq!(track.clips[0].id, a);
    assert_eq!(track.clips[1].id, c);
    assert_eq!(track.clips[1].start, 2 * SEC, "c se desplaza 3 s a la izquierda");
    assert!(validate(&store.project).is_empty());
}

#[test]
fn delete_without_ripple_leaves_gap() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    store.insert_clip(vtrack, Clip::new_media(va, 0, 2 * SEC, 0), InsertMode::Strict).unwrap();
    let b = store.insert_clip(vtrack, Clip::new_media(va, 0, 3 * SEC, 2 * SEC), InsertMode::Strict).unwrap();
    let c = store.insert_clip(vtrack, Clip::new_media(va, 0, 1 * SEC, 5 * SEC), InsertMode::Strict).unwrap();
    store.delete_clips(&[b], false).unwrap();
    let track = store.project.track(vtrack).unwrap();
    assert_eq!(track.clips.len(), 2);
    let cc = track.clips.iter().find(|cl| cl.id == c).unwrap();
    assert_eq!(cc.start, 5 * SEC, "sin ripple, c no se mueve");
}

#[test]
fn overwrite_insert_carves_middle() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    // clip grande [0, 10s)
    store.insert_clip(vtrack, Clip::new_media(va, 0, 10 * SEC, 0), InsertMode::Strict).unwrap();
    // overwrite en el medio [4s, 6s)
    let new_clip = Clip::new_media(va, 20 * SEC, 22 * SEC, 4 * SEC);
    let new_id = store.insert_clip(vtrack, new_clip, InsertMode::Overwrite).unwrap();

    let track = store.project.track(vtrack).unwrap();
    assert_eq!(track.clips.len(), 3, "izquierda + nuevo + derecha");
    let (l, m, r) = (&track.clips[0], &track.clips[1], &track.clips[2]);
    assert_eq!((l.start, l.end()), (0, 4 * SEC));
    assert_eq!(m.id, new_id);
    assert_eq!((m.start, m.end()), (4 * SEC, 6 * SEC));
    assert_eq!((r.start, r.end()), (6 * SEC, 10 * SEC));
    // el material fuente de la derecha avanzó: [6s, 10s) del archivo
    assert_eq!(media_src(r), (6 * SEC, 10 * SEC));
    assert!(validate(&store.project).is_empty());

    // undo devuelve el clip único
    store.undo().unwrap();
    assert_eq!(store.project.track(vtrack).unwrap().clips.len(), 1);
}

#[test]
fn overwrite_insert_trims_edges() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    store.insert_clip(vtrack, Clip::new_media(va, 0, 4 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(vtrack, Clip::new_media(va, 0, 4 * SEC, 6 * SEC), InsertMode::Strict).unwrap();
    // overwrite [3s, 7s): recorta el final del primero y el inicio del segundo
    store
        .insert_clip(vtrack, Clip::new_media(va, 30 * SEC, 34 * SEC, 3 * SEC), InsertMode::Overwrite)
        .unwrap();
    let track = store.project.track(vtrack).unwrap();
    assert_eq!(track.clips.len(), 3);
    assert_eq!((track.clips[0].start, track.clips[0].end()), (0, 3 * SEC));
    assert_eq!((track.clips[1].start, track.clips[1].end()), (3 * SEC, 7 * SEC));
    assert_eq!((track.clips[2].start, track.clips[2].end()), (7 * SEC, 10 * SEC));
    // src_in del tercero avanzó 1 s
    assert_eq!(media_src(&track.clips[2]).0, 1 * SEC);
}

#[test]
fn trim_respects_source_material() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    // clip que usa [5s, 10s) del archivo, colocado en t=20s
    let clip_id = store
        .insert_clip(vtrack, Clip::new_media(va, 5 * SEC, 10 * SEC, 20 * SEC), InsertMode::Strict)
        .unwrap();
    // intentar extender el borde izquierdo hasta t=0: solo hay 5 s de handle → clampa a 15 s
    store.trim_clip(clip_id, true, 0).unwrap();
    let clip = store.project.clip(clip_id).unwrap();
    assert_eq!(clip.start, 15 * SEC, "el borde se detiene donde se acaba el material");
    assert_eq!(media_src(clip).0, 0, "src_in llegó al inicio del archivo");
    assert_eq!(clip.duration, 10 * SEC);

    // extender el borde derecho más allá del archivo (60 s de asset)
    store.trim_clip(clip_id, false, 500 * SEC).unwrap();
    let clip = store.project.clip(clip_id).unwrap();
    assert_eq!(media_src(clip).1, 60 * SEC, "src_out clampeado a la duración del asset");
}

#[test]
fn cut_ranges_multitrack_ripple() {
    let (mut store, seq, vtrack, atrack, va, aa) = fixture();
    store.insert_clip(vtrack, Clip::new_media(va, 0, 10 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(atrack, Clip::new_media(aa, 0, 10 * SEC, 0), InsertMode::Strict).unwrap();

    // cortar [2s,3s) y [5s,6s) — solapa/fusiona incluido
    store.cut_ranges(seq, &[(2 * SEC, 3 * SEC), (5 * SEC, 6 * SEC)], true).unwrap();

    for track_id in [vtrack, atrack] {
        let track = store.project.track(track_id).unwrap();
        let total: i64 = track.clips.iter().map(|c| c.duration).sum();
        assert_eq!(total, 8 * SEC, "quedan 8 s de material en pista");
        // contiguos sin huecos (ripple)
        let mut expected_start = 0;
        for c in &track.clips {
            assert_eq!(c.start, expected_start);
            expected_start = c.end();
        }
    }
    assert!(validate(&store.project).is_empty());
    assert_eq!(store.undo_labels().last().copied(), Some("Cortar 2 rango(s)"));

    // y es UNA entrada de undo
    store.undo().unwrap();
    let track = store.project.track(vtrack).unwrap();
    assert_eq!(track.clips.len(), 1);
    assert_eq!(track.clips[0].duration, 10 * SEC);
}

#[test]
fn transaction_atomicity_on_failure() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    let a = Clip::new_media(va, 0, 2 * SEC, 0);
    let b_colliding = Clip::new_media(va, 0, 2 * SEC, SEC); // colisiona con a
    let snapshot = store.project.clone();

    let result = store.dispatch(
        "transacción rota",
        vec![
            Action::InsertClip { track_id: vtrack, clip: a },
            Action::InsertClip { track_id: vtrack, clip: b_colliding },
        ],
    );
    assert!(result.is_err());
    assert_eq!(store.project, snapshot, "rollback total: el proyecto queda intacto");
    assert!(!store.can_undo(), "una transacción fallida no entra al historial");
}

#[test]
fn locked_track_rejects_ops() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    let clip_id = store
        .insert_clip(vtrack, Clip::new_media(va, 0, 2 * SEC, 0), InsertMode::Strict)
        .unwrap();
    store
        .dispatch(
            "Bloquear pista",
            vec![Action::SetTrackProp { track_id: vtrack, prop: TrackProp::Locked(true) }],
        )
        .unwrap();
    assert!(store.split_clip(clip_id, SEC).is_err());
    assert!(store.delete_clips(&[clip_id], false).is_err());
    // pero el undo del bloqueo funciona
    store.undo().unwrap();
    assert!(store.split_clip(clip_id, SEC).is_ok());
}

#[test]
fn wrong_track_kind_rejected() {
    let (mut store, _seq, _vt, atrack, va, _aa) = fixture();
    let video_clip = Clip::new_media(va, 0, 2 * SEC, 0);
    let err = store.insert_clip(atrack, video_clip, InsertMode::Strict);
    assert!(err.is_err(), "un clip de video no entra en pista de audio");
}

#[test]
fn project_save_load_after_edits() {
    let (mut store, _seq, vtrack, _at, va, _aa) = fixture();
    let clip_id = store
        .insert_clip(vtrack, Clip::new_media(va, 0, 10 * SEC, 0), InsertMode::Strict)
        .unwrap();
    store.split_clip(clip_id, 3 * SEC).unwrap();
    let json = store.project.to_json().unwrap();
    let loaded = Project::from_json(&json).unwrap();
    assert_eq!(store.project, loaded);
    assert!(validate(&loaded).is_empty());
}

// ---------------------------------------------------------------------------
// Propiedad: secuencias aleatorias de operaciones + undo total ≡ estado inicial
// ---------------------------------------------------------------------------

mod property_tests {
    use super::*;
    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum OpSpec {
        Insert { start_s: i64, dur_s: i64 },
        Split { clip_sel: usize, frac: f64 },
        Delete { clip_sel: usize, ripple: bool },
        Move { clip_sel: usize, start_s: i64, overwrite: bool },
        Trim { clip_sel: usize, left: bool, edge_s: i64 },
        CutRange { from_s: i64, len_s: i64 },
    }

    fn op_strategy() -> impl Strategy<Value = OpSpec> {
        prop_oneof![
            (0i64..30, 1i64..8).prop_map(|(s, d)| OpSpec::Insert { start_s: s, dur_s: d }),
            (0usize..8, 0.05f64..0.95).prop_map(|(c, f)| OpSpec::Split { clip_sel: c, frac: f }),
            (0usize..8, any::<bool>()).prop_map(|(c, r)| OpSpec::Delete { clip_sel: c, ripple: r }),
            (0usize..8, 0i64..30, any::<bool>())
                .prop_map(|(c, s, o)| OpSpec::Move { clip_sel: c, start_s: s, overwrite: o }),
            (0usize..8, any::<bool>(), 0i64..30)
                .prop_map(|(c, l, e)| OpSpec::Trim { clip_sel: c, left: l, edge_s: e }),
            (0i64..25, 1i64..5).prop_map(|(f, l)| OpSpec::CutRange { from_s: f, len_s: l }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn random_ops_then_undo_all_restores_initial(ops in prop_vec(op_strategy(), 1..40)) {
            let (mut store, seq, vtrack, _at, va, _aa) = fixture();
            // estado inicial con un clip base
            store.insert_clip(vtrack, Clip::new_media(va, 0, 10 * SEC, 0), InsertMode::Strict).unwrap();
            let initial = store.project.clone();
            let initial_undo_depth = store.undo_labels().len();

            for op in &ops {
                let clip_ids: Vec<Id> = store
                    .project
                    .track(vtrack)
                    .unwrap()
                    .clips
                    .iter()
                    .map(|c| c.id)
                    .collect();
                let pick = |sel: usize| clip_ids.get(sel % clip_ids.len().max(1)).copied();
                let _ = match *op {
                    OpSpec::Insert { start_s, dur_s } => {
                        let src_len = dur_s.min(50) * SEC;
                        store
                            .insert_clip(
                                vtrack,
                                Clip::new_media(va, 0, src_len, start_s * SEC),
                                InsertMode::Overwrite,
                            )
                            .map(|_| ())
                    }
                    OpSpec::Split { clip_sel, frac } => match pick(clip_sel) {
                        Some(id) => {
                            let c = store.project.clip(id).unwrap();
                            let t = c.start + (c.duration as f64 * frac) as i64;
                            store.split_clip(id, t).map(|_| ())
                        }
                        None => Ok(()),
                    },
                    OpSpec::Delete { clip_sel, ripple } => match pick(clip_sel) {
                        Some(id) => store.delete_clips(&[id], ripple),
                        None => Ok(()),
                    },
                    OpSpec::Move { clip_sel, start_s, overwrite } => match pick(clip_sel) {
                        Some(id) => {
                            let mode = if overwrite { InsertMode::Overwrite } else { InsertMode::Strict };
                            store.move_clip(id, vtrack, start_s * SEC, mode)
                        }
                        None => Ok(()),
                    },
                    OpSpec::Trim { clip_sel, left, edge_s } => match pick(clip_sel) {
                        Some(id) => store.trim_clip(id, left, edge_s * SEC),
                        None => Ok(()),
                    },
                    OpSpec::CutRange { from_s, len_s } => {
                        store.cut_ranges(seq, &[(from_s * SEC, (from_s + len_s) * SEC)], true)
                    }
                };
                // gane o falle, los invariantes se mantienen SIEMPRE
                prop_assert_eq!(validate(&store.project), Vec::<String>::new());
            }

            // deshacer todo lo aplicado en el bucle
            while store.undo_labels().len() > initial_undo_depth {
                store.undo().unwrap();
            }
            prop_assert_eq!(&store.project, &initial);
        }
    }
}

#[test]
fn move_range_reorders_and_preserves_material() {
    let (mut store, seq, vtrack, atrack, va, aa) = fixture();
    // V1: un clip de 9 s [0..9); A1: audio paralelo [0..9)
    store.insert_clip(vtrack, Clip::new_media(va, 0, 9 * SEC, 0), InsertMode::Strict).unwrap();
    store.insert_clip(atrack, Clip::new_media(aa, 0, 9 * SEC, 0), InsertMode::Strict).unwrap();
    let snapshot = store.project.clone();

    // mover el tercio central [3..6) al principio (dest=0)
    store.move_range(seq, 3 * SEC, 6 * SEC, 0).unwrap();

    for track_id in [vtrack, atrack] {
        let track = store.project.track(track_id).unwrap();
        // material total conservado y contiguo
        let total: i64 = track.clips.iter().map(|c| c.duration).sum();
        assert_eq!(total, 9 * SEC);
        let mut expected = 0;
        for c in &track.clips {
            assert_eq!(c.start, expected, "sin huecos tras mover");
            expected = c.end();
        }
        // el primer clip ahora es el material fuente [3..6)
        let first = &track.clips[0];
        match &first.payload {
            ClipPayload::Media { src_in, src_out, .. } => {
                assert_eq!((*src_in, *src_out), (3 * SEC, 6 * SEC), "el tercio central va primero");
            }
            _ => panic!("payload inesperado"),
        }
    }
    assert!(validate(&store.project).is_empty());

    // una sola entrada de undo lo revierte todo
    store.undo().unwrap();
    assert_eq!(store.project, snapshot);
}

#[test]
fn move_range_forward_and_edge_cases() {
    let (mut store, seq, vtrack, _at, va, _aa) = fixture();
    store.insert_clip(vtrack, Clip::new_media(va, 0, 9 * SEC, 0), InsertMode::Strict).unwrap();

    // mover [0..3) al final (dest=9): queda [3..9)+[0..3)
    store.move_range(seq, 0, 3 * SEC, 9 * SEC).unwrap();
    let track = store.project.track(vtrack).unwrap();
    let last = track.clips.last().unwrap();
    match &last.payload {
        ClipPayload::Media { src_in, .. } => assert_eq!(*src_in, 0, "el inicio quedó al final"),
        _ => panic!(),
    }
    assert_eq!(track.clips.last().unwrap().end(), 9 * SEC, "duración total intacta");

    // destino dentro del rango → error y sin cambios
    let before = store.project.clone();
    assert!(store.move_range(seq, 0, 4 * SEC, 2 * SEC).is_err());
    assert_eq!(store.project, before);
}
