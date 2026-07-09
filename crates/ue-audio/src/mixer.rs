//! Mezclador puro: dado un conjunto de items y un frame del timeline, produce
//! la muestra estéreo mezclada. Sin IO, sin dispositivos: 100% testeable.

use crate::wav::WavMap;

pub struct MixItem {
    pub wav: WavMap,
    /// Posición del clip en el timeline, en frames de 48 kHz.
    pub timeline_start: i64,
    /// Offset dentro del WAV donde empieza el clip (frames).
    pub src_in: i64,
    /// Duración del clip en frames (de TIMELINE, ya dividida por speed).
    pub len: i64,
    /// Velocidad del clip: la fuente se lee a este ritmo. En vivo cambia el
    /// pitch (remuestreo simple); el export preserva pitch con atempo.
    pub speed: f64,
    /// Ganancia lineal (clip gain_db + volumen de pista, ya convertidos).
    pub gain: f32,
    pub fade_in: i64,
    pub fade_out: i64,
}

pub fn db_to_linear(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

impl MixItem {
    #[inline]
    fn factor_at(&self, rel: i64) -> f32 {
        let mut g = self.gain;
        if self.fade_in > 0 && rel < self.fade_in {
            g *= rel as f32 / self.fade_in as f32;
        }
        if self.fade_out > 0 {
            let from_end = self.len - rel;
            if from_end < self.fade_out {
                g *= (from_end.max(0)) as f32 / self.fade_out as f32;
            }
        }
        g
    }
}

/// Mezcla la muestra del frame `pos` del timeline. Clamp duro a [-1, 1]
/// (limiter suave: backlog).
#[inline]
pub fn mix_frame(items: &[MixItem], pos: i64) -> (f32, f32) {
    let mut acc = (0.0f32, 0.0f32);
    for item in items {
        let rel = pos - item.timeline_start;
        if rel < 0 || rel >= item.len {
            continue;
        }
        let src_rel = if (item.speed - 1.0).abs() > 1e-9 {
            (rel as f64 * item.speed).round() as i64
        } else {
            rel
        };
        let (l, r) = item.wav.frame(item.src_in + src_rel);
        let g = item.factor_at(rel);
        acc.0 += l * g;
        acc.1 += r * g;
    }
    (acc.0.clamp(-1.0, 1.0), acc.1.clamp(-1.0, 1.0))
}

/// Rellena un buffer intercalado estéreo contiguo desde `pos`.
pub fn fill(items: &[MixItem], pos: i64, out: &mut [f32]) {
    for (i, chunk) in out.chunks_exact_mut(2).enumerate() {
        let (l, r) = mix_frame(items, pos + i as i64);
        chunk[0] = l;
        chunk[1] = r;
    }
}
