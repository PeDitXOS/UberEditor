//! ue-export v0: exportación real del timeline a MP4 vía un único proceso
//! ffmpeg con filter_complex.
//!
//! Alcance v0 (documentado en PLAN §5.6; el grafo wgpu lo sustituirá):
//! - Video: EDL plana "gana el clip de la pista superior" por tramo; huecos → negro.
//!   Texto/efectos/transform aún no se queman (Fase 2).
//! - Audio: todos los clips con audio (pistas de audio y video) con ganancia,
//!   fades y volumen de pista; mezcla con amix.
//! - speed != 1.0 no soportado todavía (error explícito).

pub mod edl;
pub mod graph;

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;
use ue_core::model::{Id, Project};

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("secuencia {0} no existe")]
    NoSequence(Id),
    #[error("no hay nada que exportar (timeline vacío)")]
    EmptyTimeline,
    #[error("asset {0} no existe en el pool")]
    MissingAsset(Id),
    #[error("no se pudo ejecutar ffmpeg: {0}")]
    Spawn(String),
    #[error("ffmpeg falló:\n{0}")]
    Ffmpeg(String),
    #[error("exportación cancelada")]
    Cancelled,
}

pub type ExportResult<T> = Result<T, ExportError>;

#[derive(Debug, Clone)]
pub struct ExportSettings {
    /// Altura máxima de salida (None = resolución de la secuencia).
    pub max_height: Option<u32>,
    pub crf: u8,
    pub preset: String,
    pub audio_bitrate_k: u32,
    /// Packs de efectos de usuario (se fusionan sobre los core).
    pub extra_packs: Vec<ue_render::EffectDef>,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            max_height: None,
            crf: 18,
            preset: "veryfast".into(),
            audio_bitrate_k: 256,
            extra_packs: vec![],
        }
    }
}

/// Exporta la secuencia activa a `output` (mp4). Bloqueante.
pub fn export_sequence(
    project: &Project,
    sequence_id: Id,
    base_dir: &Path,
    output: &Path,
    settings: &ExportSettings,
) -> ExportResult<()> {
    let never = AtomicBool::new(false);
    export_sequence_with_progress(project, sequence_id, base_dir, output, settings, |_| {}, &never)
}

/// Exportación con progreso (0..1) y cancelación cooperativa.
/// Al cancelar se mata ffmpeg y se borra el archivo parcial.
pub fn export_sequence_with_progress(
    project: &Project,
    sequence_id: Id,
    base_dir: &Path,
    output: &Path,
    settings: &ExportSettings,
    mut on_progress: impl FnMut(f32),
    cancel: &AtomicBool,
) -> ExportResult<()> {
    let plan = graph::build_ffmpeg_args(project, sequence_id, base_dir, output, settings)?;
    let total_us = plan.duration_us.max(1);

    // -progress pipe:1 emite líneas clave=valor por stdout
    let mut args: Vec<String> =
        vec!["-progress".into(), "pipe:1".into(), "-nostats".into()];
    args.extend(plan.args.iter().cloned());

    let mut child = Command::new(ue_media::ffmpeg_bin())
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| ExportError::Spawn(e.to_string()))?;

    // drenar stderr en paralelo (evita bloqueo del pipe y conserva el error)
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let stdout = child.stdout.take().expect("stdout piped");
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(output);
            return Err(ExportError::Cancelled);
        }
        let Ok(line) = line else { break };
        if let Some(v) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<i64>() {
                on_progress((us as f32 / total_us as f32).clamp(0.0, 1.0));
            }
        }
    }

    let status = child.wait().map_err(|e| ExportError::Spawn(e.to_string()))?;
    let err_text = stderr_thread.join().unwrap_or_default();
    if cancel.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(output);
        return Err(ExportError::Cancelled);
    }
    if !status.success() {
        return Err(ExportError::Ffmpeg(err_text.trim().to_string()));
    }
    on_progress(1.0);
    Ok(())
}
