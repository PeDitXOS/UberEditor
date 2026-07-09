//! ProjectStore: la única puerta de mutación del proyecto.
//! dispatch() aplica una transacción de acciones con atomicidad (rollback si
//! una falla) y la registra en el historial.

use crate::action::{apply, Action};
use crate::error::{UeError, UeResult};
use crate::history::{History, HistoryEntry};
use crate::model::{Clip, Id, Project};
use crate::ops::{self, InsertMode};
use crate::time::TimeUs;
use crate::validate::validate;

pub struct ProjectStore {
    pub project: Project,
    history: History,
    /// Aumenta con cada mutación efectiva (para sincronizar mirrors).
    pub version: u64,
    pub dirty: bool,
}

impl ProjectStore {
    pub fn new(project: Project) -> Self {
        ProjectStore { project, history: History::new(1000), version: 0, dirty: false }
    }

    /// Aplica una transacción. Atómica: si una acción falla, se revierten las
    /// anteriores y el proyecto queda intacto.
    pub fn dispatch(&mut self, label: &str, actions: Vec<Action>) -> UeResult<()> {
        if actions.is_empty() {
            return Ok(());
        }
        let mut inverses: Vec<Action> = Vec::with_capacity(actions.len());
        for action in &actions {
            match apply(&mut self.project, action.clone()) {
                Ok(inv) => inverses.push(inv),
                Err(e) => {
                    for inv in inverses.into_iter().rev() {
                        apply(&mut self.project, inv).expect("rollback debe ser infalible");
                    }
                    return Err(e);
                }
            }
        }
        debug_assert_eq!(
            validate(&self.project),
            Vec::<String>::new(),
            "invariantes rotos tras '{label}'"
        );
        self.history.push(HistoryEntry { label: label.to_string(), actions, inverses });
        self.version += 1;
        self.dirty = true;
        Ok(())
    }

    pub fn undo(&mut self) -> UeResult<Option<String>> {
        let Some(entry) = self.history.pop_undo() else { return Ok(None) };
        for inv in entry.inverses.iter().rev() {
            apply(&mut self.project, inv.clone())
                .map_err(|e| UeError::Invalid(format!("undo inconsistente: {e}")))?;
        }
        let label = entry.label.clone();
        self.history.push_redo(entry);
        self.version += 1;
        self.dirty = true;
        Ok(Some(label))
    }

    pub fn redo(&mut self) -> UeResult<Option<String>> {
        let Some(mut entry) = self.history.pop_redo() else { return Ok(None) };
        let mut new_inverses = Vec::with_capacity(entry.actions.len());
        for action in &entry.actions {
            let inv = apply(&mut self.project, action.clone())
                .map_err(|e| UeError::Invalid(format!("redo inconsistente: {e}")))?;
            new_inverses.push(inv);
        }
        entry.inverses = new_inverses;
        let label = entry.label.clone();
        self.history.push_undo_from_redo(entry);
        self.version += 1;
        self.dirty = true;
        Ok(Some(label))
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }
    pub fn undo_labels(&self) -> Vec<&str> {
        self.history.undo_labels()
    }

    // ---- Operaciones de alto nivel (planifican + despachan) ----

    pub fn split_clip(&mut self, clip_id: Id, t: TimeUs) -> UeResult<(Id, Id)> {
        let (actions, l, r) = ops::split_clip(&self.project, clip_id, t)?;
        self.dispatch("Dividir clip", actions)?;
        Ok((l, r))
    }

    pub fn delete_clips(&mut self, ids: &[Id], ripple: bool) -> UeResult<()> {
        let actions = ops::delete_clips(&self.project, ids, ripple)?;
        let label = if ripple { "Eliminar (ripple)" } else { "Eliminar" };
        self.dispatch(label, actions)
    }

    pub fn insert_clip(&mut self, track_id: Id, clip: Clip, mode: InsertMode) -> UeResult<Id> {
        let clip_id = clip.id;
        let actions = ops::insert_clip(&self.project, track_id, clip, mode)?;
        self.dispatch("Añadir clip", actions)?;
        Ok(clip_id)
    }

    pub fn move_clip(
        &mut self,
        clip_id: Id,
        to_track: Id,
        to_start: TimeUs,
        mode: InsertMode,
    ) -> UeResult<()> {
        let actions = ops::move_clip(&self.project, clip_id, to_track, to_start, mode)?;
        self.dispatch("Mover clip", actions)
    }

    pub fn trim_clip(&mut self, clip_id: Id, left: bool, new_edge: TimeUs) -> UeResult<()> {
        let actions = ops::trim_clip(&self.project, clip_id, left, new_edge)?;
        self.dispatch("Recortar clip", actions)
    }

    pub fn move_range(
        &mut self,
        sequence_id: Id,
        from: TimeUs,
        to: TimeUs,
        dest: TimeUs,
    ) -> UeResult<()> {
        let actions = ops::move_range(&self.project, sequence_id, from, to, dest)?;
        self.dispatch("Mover rango", actions)
    }

    pub fn cut_ranges(
        &mut self,
        sequence_id: Id,
        ranges: &[(TimeUs, TimeUs)],
        ripple: bool,
    ) -> UeResult<()> {
        let actions = ops::cut_ranges(&self.project, sequence_id, ranges, ripple)?;
        let label = format!("Cortar {} rango(s)", ranges.len());
        self.dispatch(&label, actions)
    }
}
