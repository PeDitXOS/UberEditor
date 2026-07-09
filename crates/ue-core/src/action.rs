//! Acciones primitivas con inversa mecánica. Toda mutación del proyecto pasa
//! por `apply()`, que devuelve la acción inversa (base del undo/redo).
//!
//! Las primitivas NO comprueban `locked` (eso lo hace la capa de ops/UI):
//! así el undo funciona siempre, incluso si el usuario bloquea una pista después.

use serde::{Deserialize, Serialize};

use crate::error::{UeError, UeResult};
use crate::model::*;
use crate::time::TimeUs;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
#[allow(clippy::large_enum_variant)] // las acciones viven poco y viajan por historial; Box-ear Clip complicaría las inversas
pub enum Action {
    SetProjectName { name: String },
    AddTrack { sequence_id: Id, index: usize, track: Track },
    RemoveTrack { track_id: Id },
    InsertClip { track_id: Id, clip: Clip },
    RemoveClip { clip_id: Id },
    /// Quita `remove_ids` de la pista e inserta `insert` (ordenados). Primitiva
    /// central para split/join y overwrite. Su inversa es otra ReplaceClips.
    ReplaceClips { track_id: Id, remove_ids: Vec<Id>, insert: Vec<Clip> },
    /// Reposiciona/recorta un clip. `src_in/src_out` solo aplican a payload Media.
    SetClipBounds {
        clip_id: Id,
        start: TimeUs,
        duration: TimeUs,
        src_in: Option<TimeUs>,
        src_out: Option<TimeUs>,
    },
    MoveClipToTrack { clip_id: Id, to_track: Id, to_start: TimeUs },
    SetTrackProp { track_id: Id, prop: TrackProp },
    SetClipTransform { clip_id: Id, transform: Transform2D },
    SetClipAudio { clip_id: Id, audio: AudioProps },
    SetClipEffects { clip_id: Id, effects: Vec<EffectInstance> },
    SetClipTransition { clip_id: Id, transition: Option<TransitionRef> },
    /// Cambia contenido y estilo de un clip de texto (payload Text).
    SetClipText { clip_id: Id, content: String, style: TextStyle },
    /// Cambia estilo y modo de un clip de subtítulos (payload Subtitles).
    SetClipSubtitles { clip_id: Id, style: TextStyle, mode: SubtitleMode },
    /// Cambia la velocidad de un clip media (rate stretch: mismo material
    /// fuente, duración = fuente/velocidad). `duration` viene precalculada.
    SetClipSpeed { clip_id: Id, speed: f64, duration: TimeUs },
    AddSequence { sequence: Sequence },
    RemoveSequence { sequence_id: Id },
    SetActiveSequence { sequence_id: Id },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "prop", content = "value")]
pub enum TrackProp {
    Name(String),
    Muted(bool),
    Solo(bool),
    Locked(bool),
    VolumeDb(f32),
}

/// Tipo de pista que le corresponde a un clip por su payload.
pub fn natural_track_kind(project: &Project, clip: &Clip) -> TrackKind {
    match &clip.payload {
        ClipPayload::Media { asset_id, .. } => match project.asset(*asset_id) {
            Some(a) if a.kind == MediaKind::Audio => TrackKind::Audio,
            _ => TrackKind::Video,
        },
        _ => TrackKind::Video,
    }
}

fn check_kind(project: &Project, track: &Track, clip: &Clip) -> UeResult<()> {
    if natural_track_kind(project, clip) != track.kind {
        return Err(UeError::Invalid(format!(
            "el clip {} no puede ir en una pista de tipo {:?}",
            clip.id, track.kind
        )));
    }
    Ok(())
}

fn insert_sorted(track: &mut Track, clip: Clip) -> UeResult<()> {
    if track.collides(clip.start, clip.duration, None) {
        return Err(UeError::Overlap(format!(
            "clip {} en [{}, {}) sobre pista {}",
            clip.id,
            clip.start,
            clip.end(),
            track.id
        )));
    }
    let idx = track.insertion_index(clip.start);
    track.clips.insert(idx, clip);
    Ok(())
}

/// Aplica la acción y devuelve su inversa.
pub fn apply(project: &mut Project, action: Action) -> UeResult<Action> {
    match action {
        Action::SetProjectName { name } => {
            let old = std::mem::replace(&mut project.name, name);
            Ok(Action::SetProjectName { name: old })
        }

        Action::AddTrack { sequence_id, index, track } => {
            let track_id = track.id;
            let seq = project
                .sequence_mut(sequence_id)
                .ok_or_else(|| UeError::NotFound(format!("secuencia {sequence_id}")))?;
            let index = index.min(seq.tracks.len());
            seq.tracks.insert(index, track);
            Ok(Action::RemoveTrack { track_id })
        }

        Action::RemoveTrack { track_id } => {
            for seq in project.sequences.iter_mut() {
                if let Some(idx) = seq.tracks.iter().position(|t| t.id == track_id) {
                    let track = seq.tracks.remove(idx);
                    return Ok(Action::AddTrack { sequence_id: seq.id, index: idx, track });
                }
            }
            Err(UeError::NotFound(format!("pista {track_id}")))
        }

        Action::InsertClip { track_id, clip } => {
            check_kind(project, project.track(track_id).ok_or_else(|| UeError::NotFound(format!("pista {track_id}")))?, &clip)?;
            let clip_id = clip.id;
            let track = project.track_mut(track_id).unwrap();
            insert_sorted(track, clip)?;
            Ok(Action::RemoveClip { clip_id })
        }

        Action::RemoveClip { clip_id } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let track_id = project.sequences[si].tracks[ti].id;
            let clip = project.sequences[si].tracks[ti].clips.remove(ci);
            Ok(Action::InsertClip { track_id, clip })
        }

        Action::ReplaceClips { track_id, remove_ids, insert } => {
            // Validar primero (atomicidad): todos los ids existen en la pista y
            // los nuevos clips no colisionan con lo que queda ni entre sí.
            let track = project
                .track(track_id)
                .ok_or_else(|| UeError::NotFound(format!("pista {track_id}")))?;
            for id in &remove_ids {
                if track.clip_index(*id).is_none() {
                    return Err(UeError::NotFound(format!("clip {id} en pista {track_id}")));
                }
            }
            for c in &insert {
                check_kind(project, track, c)?;
            }
            {
                let remaining: Vec<&Clip> = track
                    .clips
                    .iter()
                    .filter(|c| !remove_ids.contains(&c.id))
                    .collect();
                let mut all: Vec<(TimeUs, TimeUs)> =
                    remaining.iter().map(|c| (c.start, c.end())).collect();
                for c in &insert {
                    all.push((c.start, c.end()));
                }
                all.sort();
                for w in all.windows(2) {
                    if w[0].1 > w[1].0 {
                        return Err(UeError::Overlap(format!(
                            "replace_clips produciría solape en pista {track_id}"
                        )));
                    }
                }
            }
            // Mutar.
            let track = project.track_mut(track_id).unwrap();
            let mut removed: Vec<Clip> = Vec::with_capacity(remove_ids.len());
            for id in &remove_ids {
                let idx = track.clip_index(*id).unwrap();
                removed.push(track.clips.remove(idx));
            }
            let inserted_ids: Vec<Id> = insert.iter().map(|c| c.id).collect();
            for c in insert {
                let idx = track.insertion_index(c.start);
                track.clips.insert(idx, c);
            }
            Ok(Action::ReplaceClips { track_id, remove_ids: inserted_ids, insert: removed })
        }

        Action::SetClipBounds { clip_id, start, duration, src_in, src_out } => {
            if duration <= 0 {
                return Err(UeError::Invalid("duración <= 0".into()));
            }
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let track = &project.sequences[si].tracks[ti];
            if track.collides(start, duration, Some(clip_id)) {
                return Err(UeError::Overlap(format!("set_clip_bounds de {clip_id}")));
            }
            let track = &mut project.sequences[si].tracks[ti];
            let clip = &mut track.clips[ci];
            let old_start = clip.start;
            let old_duration = clip.duration;
            clip.start = start;
            clip.duration = duration;
            let (old_si, old_so) = match &mut clip.payload {
                ClipPayload::Media { src_in: s_in, src_out: s_out, .. } => {
                    let old = (Some(*s_in), Some(*s_out));
                    if let Some(v) = src_in {
                        *s_in = v;
                    }
                    if let Some(v) = src_out {
                        *s_out = v;
                    }
                    if *s_in >= *s_out {
                        return Err(UeError::Invalid("src_in >= src_out".into()));
                    }
                    old
                }
                _ => (None, None),
            };
            // Reordenar la pista por start (el clip pudo saltar de posición).
            track.clips.sort_by_key(|c| c.start);
            Ok(Action::SetClipBounds {
                clip_id,
                start: old_start,
                duration: old_duration,
                src_in: old_si,
                src_out: old_so,
            })
        }

        Action::MoveClipToTrack { clip_id, to_track, to_start } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let from_track = project.sequences[si].tracks[ti].id;
            let from_start = project.sequences[si].tracks[ti].clips[ci].start;
            let clip_ref = &project.sequences[si].tracks[ti].clips[ci];
            let duration = clip_ref.duration;

            let target = project
                .track(to_track)
                .ok_or_else(|| UeError::NotFound(format!("pista {to_track}")))?;
            let exclude = if to_track == from_track { Some(clip_id) } else { None };
            if target.collides(to_start, duration, exclude) {
                return Err(UeError::Overlap(format!("move de {clip_id} a {to_track}@{to_start}")));
            }
            check_kind(project, target, clip_ref)?;

            let mut clip = {
                let track = &mut project.sequences[si].tracks[ti];
                track.clips.remove(ci)
            };
            clip.start = to_start;
            let target = project.track_mut(to_track).unwrap();
            let idx = target.insertion_index(clip.start);
            target.clips.insert(idx, clip);
            Ok(Action::MoveClipToTrack { clip_id, to_track: from_track, to_start: from_start })
        }

        Action::SetTrackProp { track_id, prop } => {
            let track = project
                .track_mut(track_id)
                .ok_or_else(|| UeError::NotFound(format!("pista {track_id}")))?;
            let old = match prop {
                TrackProp::Name(v) => TrackProp::Name(std::mem::replace(&mut track.name, v)),
                TrackProp::Muted(v) => TrackProp::Muted(std::mem::replace(&mut track.muted, v)),
                TrackProp::Solo(v) => TrackProp::Solo(std::mem::replace(&mut track.solo, v)),
                TrackProp::Locked(v) => TrackProp::Locked(std::mem::replace(&mut track.locked, v)),
                TrackProp::VolumeDb(v) => {
                    TrackProp::VolumeDb(std::mem::replace(&mut track.volume_db, v))
                }
            };
            Ok(Action::SetTrackProp { track_id, prop: old })
        }

        Action::SetClipTransform { clip_id, transform } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            let old = std::mem::replace(&mut clip.transform, transform);
            Ok(Action::SetClipTransform { clip_id, transform: old })
        }

        Action::SetClipAudio { clip_id, audio } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            let old = std::mem::replace(&mut clip.audio, audio);
            Ok(Action::SetClipAudio { clip_id, audio: old })
        }

        Action::SetClipEffects { clip_id, effects } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            let old = std::mem::replace(&mut clip.effects, effects);
            Ok(Action::SetClipEffects { clip_id, effects: old })
        }

        Action::SetClipText { clip_id, content, style } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            match &mut clip.payload {
                ClipPayload::Text { content: c, style: st } => {
                    let old_content = std::mem::replace(c, content);
                    let old_style = std::mem::replace(st, style);
                    Ok(Action::SetClipText { clip_id, content: old_content, style: old_style })
                }
                _ => Err(UeError::Invalid("el clip no es de texto".into())),
            }
        }

        Action::SetClipSpeed { clip_id, speed, duration } => {
            if speed <= 0.0 || duration <= 0 {
                return Err(UeError::Invalid("velocidad o duración inválida".into()));
            }
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let track = &project.sequences[si].tracks[ti];
            let start = track.clips[ci].start;
            if track.collides(start, duration, Some(clip_id)) {
                return Err(UeError::Overlap(format!(
                    "cambiar la velocidad de {clip_id} chocaría con el siguiente clip"
                )));
            }
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            let old_speed = clip.speed;
            let old_duration = clip.duration;
            clip.speed = speed;
            clip.duration = duration;
            Ok(Action::SetClipSpeed { clip_id, speed: old_speed, duration: old_duration })
        }

        Action::SetClipSubtitles { clip_id, style, mode } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            match &mut clip.payload {
                ClipPayload::Subtitles { style: st, mode: md, .. } => {
                    let old_style = std::mem::replace(st, style);
                    let old_mode = std::mem::replace(md, mode);
                    Ok(Action::SetClipSubtitles { clip_id, style: old_style, mode: old_mode })
                }
                _ => Err(UeError::Invalid("el clip no es de subtítulos".into())),
            }
        }

        Action::AddSequence { sequence } => {
            let id = sequence.id;
            project.sequences.push(sequence);
            Ok(Action::RemoveSequence { sequence_id: id })
        }

        Action::RemoveSequence { sequence_id } => {
            if project.active_sequence == sequence_id {
                return Err(UeError::Invalid(
                    "no se puede eliminar la secuencia activa".into(),
                ));
            }
            let idx = project
                .sequences
                .iter()
                .position(|s| s.id == sequence_id)
                .ok_or_else(|| UeError::NotFound(format!("secuencia {sequence_id}")))?;
            let sequence = project.sequences.remove(idx);
            Ok(Action::AddSequence { sequence })
        }

        Action::SetActiveSequence { sequence_id } => {
            if project.sequence(sequence_id).is_none() {
                return Err(UeError::NotFound(format!("secuencia {sequence_id}")));
            }
            let old = std::mem::replace(&mut project.active_sequence, sequence_id);
            Ok(Action::SetActiveSequence { sequence_id: old })
        }

        Action::SetClipTransition { clip_id, transition } => {
            let (si, ti, ci) = project
                .locate_clip(clip_id)
                .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
            let clip = &mut project.sequences[si].tracks[ti].clips[ci];
            let old = std::mem::replace(&mut clip.transition_in, transition);
            Ok(Action::SetClipTransition { clip_id, transition: old })
        }
    }
}
