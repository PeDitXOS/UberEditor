//! Construcción de los MixItem a partir del proyecto: qué clips suenan, con
//! qué ganancia y desde qué WAV conformado. (Espejo de la lógica de export.)

use std::path::{Path, PathBuf};

use ue_core::model::{ClipPayload, Id, MediaAsset, Project};

use crate::mixer::{db_to_linear, MixItem};
use crate::us_to_frames;
use crate::wav::WavMap;

/// Especificación previa a abrir archivos (pura, testeable sin IO).
#[derive(Debug, PartialEq)]
pub struct ItemSpec {
    pub asset_id: Id,
    pub timeline_start_us: i64,
    pub src_in_us: i64,
    /// Duración en TIEMPO DE TIMELINE (la fuente ya dividida por speed).
    pub len_us: i64,
    pub speed: f64,
    pub gain_db: f64,
    pub fade_in_us: i64,
    pub fade_out_us: i64,
}

/// Colecta los clips audibles (pistas de audio y video; respeta mute/solo).
pub fn collect_specs(project: &Project, sequence_id: Id) -> Vec<ItemSpec> {
    let Some(seq) = project.sequence(sequence_id) else { return vec![] };
    let any_solo = seq.tracks.iter().any(|t| t.solo);
    let mut specs = vec![];
    for track in &seq.tracks {
        if track.muted || (any_solo && !track.solo) {
            continue;
        }
        for clip in &track.clips {
            if clip.audio.muted {
                continue;
            }
            let ClipPayload::Media { asset_id, src_in, src_out } = &clip.payload else {
                continue;
            };
            let Some(asset) = project.asset(*asset_id) else { continue };
            if asset.probe.audio_channels == 0 {
                continue;
            }
            let src_len_tl = (((*src_out - *src_in) as f64) / clip.speed).round() as i64;
            specs.push(ItemSpec {
                asset_id: *asset_id,
                timeline_start_us: clip.start,
                src_in_us: *src_in,
                len_us: src_len_tl.min(clip.duration),
                speed: clip.speed,
                gain_db: clip.audio.gain_db.eval(0) + track.volume_db as f64,
                fade_in_us: clip.audio.fade_in_us,
                fade_out_us: clip.audio.fade_out_us,
            });
        }
    }
    specs
}

/// Abre los WAV conformados y produce los MixItem listos para el mezclador.
/// Los assets sin conformado disponible se omiten (y se reportan).
pub fn load_items(
    project: &Project,
    specs: &[ItemSpec],
    conform_path: impl Fn(&MediaAsset) -> Option<PathBuf>,
) -> (Vec<MixItem>, Vec<Id>) {
    let mut items = vec![];
    let mut skipped = vec![];
    for spec in specs {
        let Some(asset) = project.asset(spec.asset_id) else {
            skipped.push(spec.asset_id);
            continue;
        };
        let Some(path) = conform_path(asset) else {
            skipped.push(spec.asset_id);
            continue;
        };
        match WavMap::open(Path::new(&path)) {
            Ok(wav) => items.push(MixItem {
                wav,
                timeline_start: us_to_frames(spec.timeline_start_us),
                src_in: us_to_frames(spec.src_in_us),
                len: us_to_frames(spec.len_us),
                speed: spec.speed,
                gain: db_to_linear(spec.gain_db),
                fade_in: us_to_frames(spec.fade_in_us),
                fade_out: us_to_frames(spec.fade_out_us),
            }),
            Err(_) => skipped.push(spec.asset_id),
        }
    }
    (items, skipped)
}
