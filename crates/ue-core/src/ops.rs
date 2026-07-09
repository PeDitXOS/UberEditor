//! Operaciones de alto nivel (split, delete con ripple, overwrite, move…).
//! Cada op PLANIFICA una lista de acciones primitivas simulándolas sobre un
//! clon del proyecto (Planner); el store luego las ejecuta como una transacción.

use crate::action::{apply, Action};
use crate::error::{UeError, UeResult};
use crate::model::*;
use crate::time::{frame_duration_us, quantize_to_frame, TimeUs};

/// Simula acciones sobre un clon para planificar transacciones consistentes.
pub struct Planner {
    pub proj: Project,
    pub actions: Vec<Action>,
}

impl Planner {
    pub fn new(project: &Project) -> Self {
        Planner { proj: project.clone(), actions: vec![] }
    }

    /// Aplica al clon y registra. Si falla, la transacción entera se aborta.
    pub fn do_(&mut self, action: Action) -> UeResult<()> {
        apply(&mut self.proj, action.clone())?;
        self.actions.push(action);
        Ok(())
    }

    pub fn finish(self) -> Vec<Action> {
        self.actions
    }
}

fn ensure_unlocked(track: &Track) -> UeResult<()> {
    if track.locked {
        return Err(UeError::Locked(track.name.clone()));
    }
    Ok(())
}

/// Divide un clip en el tiempo `t` del timeline (se cuantiza a frame).
/// Devuelve las acciones y los ids (izquierdo, derecho) resultantes.
pub fn split_clip(project: &Project, clip_id: Id, t: TimeUs) -> UeResult<(Vec<Action>, Id, Id)> {
    let (si, ti, ci) = project
        .locate_clip(clip_id)
        .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
    let seq = &project.sequences[si];
    let track = &seq.tracks[ti];
    ensure_unlocked(track)?;
    let clip = &track.clips[ci];

    let t = quantize_to_frame(t, seq.fps);
    if t <= clip.start || t >= clip.end() {
        return Err(UeError::Invalid(format!(
            "el punto de corte {t} está fuera del clip ({}..{})",
            clip.start,
            clip.end()
        )));
    }
    let offset = t - clip.start;

    let (left, right) = split_clip_data(clip, offset);
    let (left_id, right_id) = (left.id, right.id);

    let mut plan = Planner::new(project);
    plan.do_(Action::ReplaceClips {
        track_id: track.id,
        remove_ids: vec![clip_id],
        insert: vec![left, right],
    })?;
    Ok((plan.finish(), left_id, right_id))
}

/// Construye las dos mitades de un clip partido en `offset` (µs relativos al clip).
fn split_clip_data(clip: &Clip, offset: TimeUs) -> (Clip, Clip) {
    let mut left = clip.clone();
    let mut right = clip.clone();
    left.id = Id::new();
    right.id = Id::new();

    left.duration = offset;
    right.start = clip.start + offset;
    right.duration = clip.duration - offset;

    // Rango fuente para payload Media (mapeado por speed).
    if let (ClipPayload::Media { src_in, src_out, .. }, ClipPayload::Media { src_in: r_in, .. }) =
        (&mut left.payload, &mut right.payload)
    {
        let src_off = (offset as f64 * clip.speed).round() as TimeUs;
        let boundary = (*src_in + src_off).min(*src_out - 1);
        *src_out = boundary;
        *r_in = boundary;
    }

    // Curvas de keyframes: repartir preservando el valor en la frontera.
    let (tl, tr) = split_transform(&clip.transform, offset);
    left.transform = tl;
    right.transform = tr;
    let (al, ar) = split_audio(&clip.audio, offset, clip.duration);
    left.audio = al;
    right.audio = ar;
    for (le, re) in left.effects.iter_mut().zip(right.effects.iter_mut()) {
        for (key, param) in le.params.clone() {
            let (pl, pr) = param.split(offset);
            le.params.insert(key.clone(), pl);
            re.params.insert(key, pr);
        }
    }

    // La transición de entrada queda en la izquierda; la derecha nace limpia.
    right.transition_in = None;

    (left, right)
}

fn split_transform(t: &Transform2D, offset: TimeUs) -> (Transform2D, Transform2D) {
    let mut l = t.clone();
    let mut r = t.clone();
    macro_rules! sp {
        ($field:expr, $lf:expr, $rf:expr) => {{
            let (a, b) = $field.split(offset);
            *$lf = a;
            *$rf = b;
        }};
    }
    sp!(t.position.0, &mut l.position.0, &mut r.position.0);
    sp!(t.position.1, &mut l.position.1, &mut r.position.1);
    sp!(t.scale.0, &mut l.scale.0, &mut r.scale.0);
    sp!(t.scale.1, &mut l.scale.1, &mut r.scale.1);
    sp!(t.rotation, &mut l.rotation, &mut r.rotation);
    sp!(t.crop.0, &mut l.crop.0, &mut r.crop.0);
    sp!(t.crop.1, &mut l.crop.1, &mut r.crop.1);
    sp!(t.crop.2, &mut l.crop.2, &mut r.crop.2);
    sp!(t.crop.3, &mut l.crop.3, &mut r.crop.3);
    sp!(t.opacity, &mut l.opacity, &mut r.opacity);
    (l, r)
}

fn split_audio(a: &AudioProps, offset: TimeUs, total: TimeUs) -> (AudioProps, AudioProps) {
    let mut l = a.clone();
    let mut r = a.clone();
    let (gl, gr) = a.gain_db.split(offset);
    let (pl, pr) = a.pan.split(offset);
    l.gain_db = gl;
    r.gain_db = gr;
    l.pan = pl;
    r.pan = pr;
    // Fades: el fade-in pertenece a la izquierda, el fade-out a la derecha.
    l.fade_in_us = a.fade_in_us.min(offset);
    l.fade_out_us = 0;
    r.fade_in_us = 0;
    r.fade_out_us = a.fade_out_us.min(total - offset);
    (l, r)
}

/// Borra clips. `ripple=true` cierra los huecos desplazando lo posterior
/// (v1: los clips con ripple deben estar en una sola pista).
pub fn delete_clips(project: &Project, ids: &[Id], ripple: bool) -> UeResult<Vec<Action>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    // Agrupar por pista y validar locks.
    let mut by_track: std::collections::BTreeMap<Id, Vec<Id>> = Default::default();
    for id in ids {
        let (si, ti, _) = project
            .locate_clip(*id)
            .ok_or_else(|| UeError::NotFound(format!("clip {id}")))?;
        let track = &project.sequences[si].tracks[ti];
        ensure_unlocked(track)?;
        by_track.entry(track.id).or_default().push(*id);
    }
    if ripple && by_track.len() > 1 {
        return Err(UeError::Invalid(
            "ripple delete multi-pista no soportado en v1 (selecciona clips de una sola pista)"
                .into(),
        ));
    }

    let mut plan = Planner::new(project);
    for (track_id, clip_ids) in by_track {
        // Intervalos eliminados, ordenados.
        let track = project.track(track_id).unwrap();
        let mut removed: Vec<(TimeUs, TimeUs)> = clip_ids
            .iter()
            .map(|id| {
                let c = &track.clips[track.clip_index(*id).unwrap()];
                (c.start, c.end())
            })
            .collect();
        removed.sort();

        for id in &clip_ids {
            plan.do_(Action::RemoveClip { clip_id: *id })?;
        }

        if ripple {
            // Desplazar a la izquierda cada clip restante por la suma de lo
            // eliminado antes de su start (de izquierda a derecha: sin colisiones).
            let survivors: Vec<(Id, TimeUs, TimeUs)> = plan
                .proj
                .track(track_id)
                .unwrap()
                .clips
                .iter()
                .map(|c| (c.id, c.start, c.duration))
                .collect();
            for (cid, start, duration) in survivors {
                let shift: TimeUs = removed
                    .iter()
                    .filter(|(_, e)| *e <= start)
                    .map(|(s, e)| e - s)
                    .sum();
                if shift > 0 {
                    plan.do_(Action::SetClipBounds {
                        clip_id: cid,
                        start: start - shift,
                        duration,
                        src_in: None,
                        src_out: None,
                    })?;
                }
            }
        }
    }
    Ok(plan.finish())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertMode {
    /// Falla si hay colisión.
    Strict,
    /// Recorta/borra lo que haya debajo (comportamiento NLE clásico).
    Overwrite,
}

/// Inserta un clip en una pista. En modo Overwrite recorta lo que pise.
pub fn insert_clip(
    project: &Project,
    track_id: Id,
    clip: Clip,
    mode: InsertMode,
) -> UeResult<Vec<Action>> {
    let track = project
        .track(track_id)
        .ok_or_else(|| UeError::NotFound(format!("pista {track_id}")))?;
    ensure_unlocked(track)?;

    let mut plan = Planner::new(project);
    if mode == InsertMode::Overwrite {
        carve_range(&mut plan, track_id, clip.start, clip.end(), None)?;
    }
    plan.do_(Action::InsertClip { track_id, clip })?;
    Ok(plan.finish())
}

/// Mueve un clip (posiblemente a otra pista). Overwrite recorta el destino.
pub fn move_clip(
    project: &Project,
    clip_id: Id,
    to_track: Id,
    to_start: TimeUs,
    mode: InsertMode,
) -> UeResult<Vec<Action>> {
    let (si, ti, ci) = project
        .locate_clip(clip_id)
        .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
    let seq = &project.sequences[si];
    let from_track = &seq.tracks[ti];
    ensure_unlocked(from_track)?;
    let target = project
        .track(to_track)
        .ok_or_else(|| UeError::NotFound(format!("pista {to_track}")))?;
    ensure_unlocked(target)?;

    let to_start = quantize_to_frame(to_start.max(0), seq.fps);
    let duration = seq.tracks[ti].clips[ci].duration;

    let mut plan = Planner::new(project);
    if mode == InsertMode::Overwrite {
        carve_range(&mut plan, to_track, to_start, to_start + duration, Some(clip_id))?;
    }
    plan.do_(Action::MoveClipToTrack { clip_id, to_track, to_start })?;
    Ok(plan.finish())
}

/// Recorta un borde de un clip (trim). `left=true` mueve el borde izquierdo.
/// `new_edge` es el nuevo tiempo del borde en el timeline (se cuantiza).
pub fn trim_clip(project: &Project, clip_id: Id, left: bool, new_edge: TimeUs) -> UeResult<Vec<Action>> {
    let (si, ti, ci) = project
        .locate_clip(clip_id)
        .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
    let seq = &project.sequences[si];
    let track = &seq.tracks[ti];
    ensure_unlocked(track)?;
    let clip = &track.clips[ci];
    let min_dur = frame_duration_us(seq.fps);
    let new_edge = quantize_to_frame(new_edge, seq.fps);

    let mut start = clip.start;
    let duration; // se fija en ambas ramas
    let (mut src_in, mut src_out) = match &clip.payload {
        ClipPayload::Media { src_in, src_out, .. } => (Some(*src_in), Some(*src_out)),
        _ => (None, None),
    };

    if left {
        let max_edge = clip.end() - min_dur;
        // límite por material fuente disponible (no antes del inicio del archivo)
        let min_edge = match src_in {
            Some(si_v) => clip.start - (si_v as f64 / clip.speed).round() as TimeUs,
            None => TimeUs::MIN / 4,
        };
        let edge = new_edge.clamp(min_edge.max(0), max_edge);
        let delta = edge - clip.start;
        start = edge;
        duration = clip.duration - delta;
        if let Some(v) = src_in.as_mut() {
            *v += (delta as f64 * clip.speed).round() as TimeUs;
        }
    } else {
        let min_edge = clip.start + min_dur;
        // límite por material fuente (duración del asset)
        let max_by_src = match (src_in, src_out) {
            (Some(si_v), Some(_)) => {
                let asset_dur = asset_duration(project, clip);
                match asset_dur {
                    Some(d) => clip.start + ((d - si_v) as f64 / clip.speed).round() as TimeUs,
                    None => TimeUs::MAX / 4,
                }
            }
            _ => TimeUs::MAX / 4,
        };
        let edge = new_edge.clamp(min_edge, max_by_src);
        duration = edge - clip.start;
        if let Some(v) = src_out.as_mut() {
            *v = src_in.unwrap() + (duration as f64 * clip.speed).round() as TimeUs;
        }
    }

    let mut plan = Planner::new(project);
    plan.do_(Action::SetClipBounds { clip_id, start, duration, src_in, src_out })?;
    Ok(plan.finish())
}

fn asset_duration(project: &Project, clip: &Clip) -> Option<TimeUs> {
    if let ClipPayload::Media { asset_id, .. } = &clip.payload {
        project.asset(*asset_id).map(|a| a.probe.duration_us)
    } else {
        None
    }
}

/// Libera el rango [from, to) de una pista recortando/eliminando/partiendo los
/// clips que lo pisan (excluyendo opcionalmente un clip).
fn carve_range(
    plan: &mut Planner,
    track_id: Id,
    from: TimeUs,
    to: TimeUs,
    exclude: Option<Id>,
) -> UeResult<()> {
    let victims: Vec<Clip> = plan
        .proj
        .track(track_id)
        .ok_or_else(|| UeError::NotFound(format!("pista {track_id}")))?
        .clips
        .iter()
        .filter(|c| Some(c.id) != exclude && c.start < to && from < c.end())
        .cloned()
        .collect();

    for c in victims {
        let covered_left = c.start >= from;
        let covered_right = c.end() <= to;
        match (covered_left, covered_right) {
            // totalmente cubierto → fuera
            (true, true) => plan.do_(Action::RemoveClip { clip_id: c.id })?,
            // pisado por la izquierda → recortar su inicio hasta `to`
            (true, false) => {
                let delta = to - c.start;
                let (si, so) = media_src(&c);
                plan.do_(Action::SetClipBounds {
                    clip_id: c.id,
                    start: to,
                    duration: c.duration - delta,
                    src_in: si.map(|v| v + (delta as f64 * c.speed).round() as TimeUs),
                    src_out: so,
                })?;
            }
            // pisado por la derecha → recortar su final hasta `from`
            (false, true) => {
                let new_dur = from - c.start;
                let (si, so) = media_src(&c);
                plan.do_(Action::SetClipBounds {
                    clip_id: c.id,
                    start: c.start,
                    duration: new_dur,
                    src_in: si,
                    src_out: so.map(|_| {
                        media_src(&c).0.unwrap() + (new_dur as f64 * c.speed).round() as TimeUs
                    }),
                })?;
            }
            // el clip envuelve el rango entero → partir en dos y recortar el medio
            (false, false) => {
                let (left, right) = split_clip_data(&c, from - c.start);
                // right empieza en `from`; recortar su inicio hasta `to`
                let delta = to - from;
                let mut right2 = right.clone();
                right2.start = to;
                right2.duration = right.duration - delta;
                if let ClipPayload::Media { src_in, .. } = &mut right2.payload {
                    *src_in += (delta as f64 * c.speed).round() as TimeUs;
                }
                plan.do_(Action::ReplaceClips {
                    track_id,
                    remove_ids: vec![c.id],
                    insert: vec![left, right2],
                })?;
            }
        }
    }
    Ok(())
}

fn media_src(c: &Clip) -> (Option<TimeUs>, Option<TimeUs>) {
    match &c.payload {
        ClipPayload::Media { src_in, src_out, .. } => (Some(*src_in), Some(*src_out)),
        _ => (None, None),
    }
}

/// Cambia la velocidad de un clip media (rate stretch). La duración nueva se
/// cuantiza a frame; error si chocaría con el siguiente clip.
pub fn set_clip_speed(project: &Project, clip_id: Id, speed: f64) -> UeResult<Vec<Action>> {
    if !(0.05..=20.0).contains(&speed) {
        return Err(UeError::Invalid("velocidad fuera de rango (0.05–20)".into()));
    }
    let (si, ti, ci) = project
        .locate_clip(clip_id)
        .ok_or_else(|| UeError::NotFound(format!("clip {clip_id}")))?;
    let seq = &project.sequences[si];
    ensure_unlocked(&seq.tracks[ti])?;
    let clip = &seq.tracks[ti].clips[ci];
    let ClipPayload::Media { src_in, src_out, .. } = &clip.payload else {
        return Err(UeError::Invalid("solo los clips de media tienen velocidad".into()));
    };
    let src_len = src_out - src_in;
    let duration = quantize_to_frame(((src_len as f64) / speed).round() as TimeUs, seq.fps)
        .max(frame_duration_us(seq.fps));
    let mut plan = Planner::new(project);
    plan.do_(Action::SetClipSpeed { clip_id, speed, duration })?;
    Ok(plan.finish())
}

/// Acelera los rangos dados (p. ej. silencios): corta en fronteras, aplica
/// `factor` a los clips interiores (que encogen) y cierra los huecos con
/// ripple. Multi-pista, una transacción.
pub fn speedup_ranges(
    project: &Project,
    sequence_id: Id,
    ranges: &[(TimeUs, TimeUs)],
    factor: f64,
) -> UeResult<Vec<Action>> {
    if factor <= 1.0 {
        return Err(UeError::Invalid("el factor de aceleración debe ser > 1".into()));
    }
    let seq = project
        .sequence(sequence_id)
        .ok_or_else(|| UeError::NotFound(format!("secuencia {sequence_id}")))?;
    let fps = seq.fps;
    let track_ids: Vec<Id> = seq.tracks.iter().filter(|t| !t.locked).map(|t| t.id).collect();

    // normalizar y ordenar de DERECHA a IZQUIERDA: al encoger un rango se
    // desplazan los posteriores, así que procesando de atrás hacia delante
    // los rangos anteriores no se invalidan.
    let mut rs: Vec<(TimeUs, TimeUs)> = ranges
        .iter()
        .map(|(a, b)| (quantize_to_frame((*a).max(0), fps), quantize_to_frame(*b, fps)))
        .filter(|(a, b)| b > a)
        .collect();
    rs.sort();
    rs.reverse();

    let mut plan = Planner::new(project);
    for (from, to) in rs {
        split_all_at(&mut plan, &track_ids, from)?;
        split_all_at(&mut plan, &track_ids, to)?;
        let len = to - from;
        // encoger los clips interiores
        let mut shrunk_by: TimeUs = 0;
        for tid in &track_ids {
            let inside: Vec<Clip> = plan
                .proj
                .track(*tid)
                .unwrap()
                .clips
                .iter()
                .filter(|c| c.start >= from && c.end() <= to)
                .cloned()
                .collect();
            for c in inside {
                if let ClipPayload::Media { src_in, src_out, .. } = &c.payload {
                    let src_len = src_out - src_in;
                    let new_speed = c.speed * factor;
                    let new_dur =
                        quantize_to_frame(((src_len as f64) / new_speed).round() as TimeUs, fps)
                            .max(frame_duration_us(fps));
                    // reposicionar proporcionalmente dentro del rango encogido
                    let rel = c.start - from;
                    let new_start = from + quantize_to_frame(
                        ((rel as f64) / factor).round() as TimeUs,
                        fps,
                    );
                    plan.do_(Action::SetClipSpeed {
                        clip_id: c.id,
                        speed: new_speed,
                        duration: new_dur,
                    })?;
                    // mover a la izquierda no colisiona: procesamos rangos de
                    // derecha a izquierda y dentro del rango en orden natural
                    plan.do_(Action::SetClipBounds {
                        clip_id: c.id,
                        start: new_start,
                        duration: new_dur,
                        src_in: None,
                        src_out: None,
                    })?;
                    shrunk_by = shrunk_by.max(len - quantize_to_frame(
                        ((len as f64) / factor).round() as TimeUs,
                        fps,
                    ));
                }
            }
        }
        // cerrar el hueco: todo lo que empiece en >= to se desplaza a la izquierda
        let shift = shrunk_by;
        if shift > 0 {
            for tid in &track_ids {
                let after: Vec<(Id, TimeUs, TimeUs)> = plan
                    .proj
                    .track(*tid)
                    .unwrap()
                    .clips
                    .iter()
                    .filter(|c| c.start >= to)
                    .map(|c| (c.id, c.start, c.duration))
                    .collect();
                for (cid, start, duration) in after {
                    plan.do_(Action::SetClipBounds {
                        clip_id: cid,
                        start: start - shift,
                        duration,
                        src_in: None,
                        src_out: None,
                    })?;
                }
            }
        }
    }
    Ok(plan.finish())
}

/// Divide en `t` cualquier clip que lo cruce, en todas las pistas dadas.
fn split_all_at(plan: &mut Planner, track_ids: &[Id], t: TimeUs) -> UeResult<()> {
    for tid in track_ids {
        let victim = plan
            .proj
            .track(*tid)
            .and_then(|tr| {
                tr.clips
                    .iter()
                    .find(|c| c.start < t && t < c.end())
                    .cloned()
            });
        if let Some(c) = victim {
            let (left, right) = split_clip_data(&c, t - c.start);
            plan.do_(Action::ReplaceClips {
                track_id: *tid,
                remove_ids: vec![c.id],
                insert: vec![left, right],
            })?;
        }
    }
    Ok(())
}

/// Mueve el rango [from, to) de la secuencia a `dest` (fuera del rango),
/// en todas las pistas no bloqueadas. Corta en las fronteras, cierra el hueco
/// de origen, abre hueco en destino y recoloca. Base de "mover frases" en la
/// edición por texto. Una transacción.
pub fn move_range(
    project: &Project,
    sequence_id: Id,
    from: TimeUs,
    to: TimeUs,
    dest: TimeUs,
) -> UeResult<Vec<Action>> {
    let seq = project
        .sequence(sequence_id)
        .ok_or_else(|| UeError::NotFound(format!("secuencia {sequence_id}")))?;
    let from = quantize_to_frame(from.max(0), seq.fps);
    let to = quantize_to_frame(to, seq.fps);
    let dest = quantize_to_frame(dest.max(0), seq.fps);
    if to <= from {
        return Err(UeError::Invalid("rango vacío".into()));
    }
    if dest > from && dest < to {
        return Err(UeError::Invalid("el destino no puede caer dentro del rango".into()));
    }
    let len = to - from;
    let track_ids: Vec<Id> = seq.tracks.iter().filter(|t| !t.locked).map(|t| t.id).collect();

    let mut plan = Planner::new(project);
    // 1. fronteras exactas
    split_all_at(&mut plan, &track_ids, from)?;
    split_all_at(&mut plan, &track_ids, to)?;
    split_all_at(&mut plan, &track_ids, dest)?;

    // 2. extraer los clips del rango (clones) y quitarlos
    let mut moved: Vec<(Id, Clip)> = vec![]; // (track, clip)
    for tid in &track_ids {
        let inside: Vec<Clip> = plan
            .proj
            .track(*tid)
            .unwrap()
            .clips
            .iter()
            .filter(|c| c.start >= from && c.end() <= to)
            .cloned()
            .collect();
        for c in inside {
            plan.do_(Action::RemoveClip { clip_id: c.id })?;
            moved.push((*tid, c));
        }
    }

    // 3. cerrar el hueco de origen (izquierda, ascendente)
    for tid in &track_ids {
        let after: Vec<(Id, TimeUs, TimeUs)> = plan
            .proj
            .track(*tid)
            .unwrap()
            .clips
            .iter()
            .filter(|c| c.start >= to)
            .map(|c| (c.id, c.start, c.duration))
            .collect();
        for (cid, start, duration) in after {
            plan.do_(Action::SetClipBounds {
                clip_id: cid,
                start: start - len,
                duration,
                src_in: None,
                src_out: None,
            })?;
        }
    }

    // 4. destino ajustado tras cerrar el hueco
    let dest_adj = if dest >= to { dest - len } else { dest };

    // 5. abrir hueco en destino (derecha, DESCENDENTE para no colisionar)
    for tid in &track_ids {
        let mut after: Vec<(Id, TimeUs, TimeUs)> = plan
            .proj
            .track(*tid)
            .unwrap()
            .clips
            .iter()
            .filter(|c| c.start >= dest_adj)
            .map(|c| (c.id, c.start, c.duration))
            .collect();
        after.sort_by_key(|(_, start, _)| std::cmp::Reverse(*start));
        for (cid, start, duration) in after {
            plan.do_(Action::SetClipBounds {
                clip_id: cid,
                start: start + len,
                duration,
                src_in: None,
                src_out: None,
            })?;
        }
    }

    // 6. recolocar los clips extraídos
    for (tid, mut clip) in moved {
        clip.start = dest_adj + (clip.start - from);
        plan.do_(Action::InsertClip { track_id: tid, clip })?;
    }
    Ok(plan.finish())
}

/// Elimina rangos de tiempo de TODA la secuencia (todas las pistas no bloqueadas),
/// con ripple opcional. Base de "eliminar silencios" y edición por texto.
pub fn cut_ranges(
    project: &Project,
    sequence_id: Id,
    ranges: &[(TimeUs, TimeUs)],
    ripple: bool,
) -> UeResult<Vec<Action>> {
    let seq = project
        .sequence(sequence_id)
        .ok_or_else(|| UeError::NotFound(format!("secuencia {sequence_id}")))?;

    // Normalizar: ordenar y fusionar solapes.
    let mut rs: Vec<(TimeUs, TimeUs)> = ranges
        .iter()
        .copied()
        .filter(|(a, b)| b > a)
        .map(|(a, b)| (quantize_to_frame(a.max(0), seq.fps), quantize_to_frame(b, seq.fps)))
        .filter(|(a, b)| b > a)
        .collect();
    rs.sort();
    let mut merged: Vec<(TimeUs, TimeUs)> = vec![];
    for r in rs {
        match merged.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => merged.push(r),
        }
    }
    if merged.is_empty() {
        return Ok(vec![]);
    }

    let track_ids: Vec<Id> =
        seq.tracks.iter().filter(|t| !t.locked).map(|t| t.id).collect();

    let mut plan = Planner::new(project);
    for &(from, to) in &merged {
        for tid in &track_ids {
            carve_range(&mut plan, *tid, from, to, None)?;
        }
    }
    if ripple {
        for tid in &track_ids {
            let survivors: Vec<(Id, TimeUs, TimeUs)> = plan
                .proj
                .track(*tid)
                .unwrap()
                .clips
                .iter()
                .map(|c| (c.id, c.start, c.duration))
                .collect();
            for (cid, start, duration) in survivors {
                let shift: TimeUs = merged
                    .iter()
                    .filter(|(_, e)| *e <= start)
                    .map(|(s, e)| e - s)
                    .sum();
                if shift > 0 {
                    plan.do_(Action::SetClipBounds {
                        clip_id: cid,
                        start: start - shift,
                        duration,
                        src_in: None,
                        src_out: None,
                    })?;
                }
            }
        }
    }
    Ok(plan.finish())
}
