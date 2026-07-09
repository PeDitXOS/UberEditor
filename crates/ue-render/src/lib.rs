//! ue-render: sistema modular de efectos (PLAN §6.5).
//!
//! v0: cada efecto es un pack con manifest.json que declara parámetros y una
//! plantilla de filtro ffmpeg — el mismo efecto se aplica en preview (sesión
//! MJPEG / render_frame) y en export (filter_complex), así que preview==export.
//! El motor wgpu (WGSL) sustituirá el backend de render manteniendo manifests
//! y parámetros; los packs de usuario en disco llegan con el hot-reload.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ue_core::model::EffectInstance;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("manifest inválido: {0}")]
    Manifest(String),
}

// ---------------------------------------------------------------------------
// Definición de efectos (manifest)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ParamKind {
    Float {
        default: f64,
        min: f64,
        max: f64,
    },
    Color {
        default: String, // "#rrggbb"
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDef {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub kind: ParamKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub params: Vec<ParamDef>,
    /// Plantilla de cadena -vf con {claves} a sustituir.
    pub ffmpeg: String,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Packs core embebidos en el binario. Los packs de usuario (carpeta en disco
/// con hot-reload) se añadirán sobre esta base.
pub fn core_registry() -> Vec<EffectDef> {
    const MANIFESTS: &[&str] = &[
        include_str!("../../../effects/core/color_correct/manifest.json"),
        include_str!("../../../effects/core/chroma_key/manifest.json"),
        include_str!("../../../effects/core/gaussian_blur/manifest.json"),
        include_str!("../../../effects/core/vertical_fill/manifest.json"),
    ];
    MANIFESTS
        .iter()
        .map(|m| serde_json::from_str(m).expect("manifest core válido (verificado por tests)"))
        .collect()
}

pub fn find_effect<'a>(registry: &'a [EffectDef], id: &str) -> Option<&'a EffectDef> {
    registry.iter().find(|d| d.id == id)
}

/// Carga packs de usuario: cada subcarpeta de `dir` con un manifest.json.
/// Los manifests inválidos no rompen nada: se reportan como errores legibles.
pub fn load_packs_from_dir(dir: &std::path::Path) -> (Vec<EffectDef>, Vec<String>) {
    let mut defs = vec![];
    let mut errors = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (defs, errors);
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("manifest.json");
        if !manifest.is_file() {
            continue;
        }
        match std::fs::read_to_string(&manifest) {
            Ok(s) => match serde_json::from_str::<EffectDef>(&s) {
                Ok(d) => defs.push(d),
                Err(e) => errors.push(format!("{}: {e}", manifest.display())),
            },
            Err(e) => errors.push(format!("{}: {e}", manifest.display())),
        }
    }
    defs.sort_by(|a, b| a.id.cmp(&b.id));
    (defs, errors)
}

/// core + usuario; en conflicto de id gana el pack de usuario.
pub fn merge_registries(core: Vec<EffectDef>, user: Vec<EffectDef>) -> Vec<EffectDef> {
    let mut out = core;
    for u in user {
        if let Some(slot) = out.iter_mut().find(|d| d.id == u.id) {
            *slot = u;
        } else {
            out.push(u);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Renderizado de la cadena ffmpeg (backend v0)
// ---------------------------------------------------------------------------

fn format_float(v: f64) -> String {
    // sin notación científica y sin ceros infinitos
    let s = format!("{v:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// "#rrggbb" → "0xRRGGBB" (sintaxis de color de ffmpeg). Valores raros caen al default.
fn format_color(hex: &str) -> Option<String> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", h.to_uppercase()))
}

/// Sustituye los parámetros de una instancia en la plantilla de su definición.
/// Parámetros ausentes usan el default del manifest; los float se clampean al rango.
/// `{u}` se sustituye por un contador único (para plantillas con mini-grafos
/// split/overlay: las etiquetas no colisionan entre usos en el mismo ffmpeg).
pub fn render_effect(def: &EffectDef, inst: &EffectInstance) -> String {
    static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut out = def.ffmpeg.clone();
    if out.contains("{u}") {
        let n = UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        out = out.replace("{u}", &format!("u{n}"));
    }
    for p in &def.params {
        let placeholder = format!("{{{}}}", p.key);
        let value = match &p.kind {
            ParamKind::Float { default, min, max } => {
                let v = inst
                    .params
                    .get(&p.key)
                    .map(|param| param.eval(0))
                    .unwrap_or(*default)
                    .clamp(*min, *max);
                format_float(v)
            }
            ParamKind::Color { default } => {
                let hex = inst.color_params.get(&p.key).map(String::as_str).unwrap_or(default);
                format_color(hex).unwrap_or_else(|| format_color(default).expect("default válido"))
            }
        };
        out = out.replace(&placeholder, &value);
    }
    out
}

/// Cadena -vf completa para los efectos habilitados de un clip (None si no hay).
pub fn render_chain(registry: &[EffectDef], effects: &[EffectInstance]) -> Option<String> {
    let parts: Vec<String> = effects
        .iter()
        .filter(|e| e.enabled)
        .filter_map(|e| find_effect(registry, &e.effect_id).map(|d| render_effect(d, e)))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// Catálogo serializable para la UI / MCP.
pub fn catalog_json(registry: &[EffectDef]) -> serde_json::Value {
    serde_json::to_value(registry).expect("registry serializable")
}

// ---------------------------------------------------------------------------
// Transform2D → cadena ffmpeg (backend v0)
// ---------------------------------------------------------------------------

/// Cadena -vf del transform de un clip: crop → escala → rotación → flips →
/// posición (orden del PLAN §6.8). La posición compone sobre un lienzo del
/// tamaño de la secuencia (color+overlay, requiere `canvas`). Opacidad llega
/// con wgpu. Curvas: se evalúan en t=0.
pub fn transform_vf(
    t: &ue_core::model::Transform2D,
    canvas: Option<(u32, u32)>,
) -> Option<String> {
    let mut parts: Vec<String> = vec![];

    let (l, top, r, b) = (
        t.crop.0.eval(0).clamp(0.0, 0.49),
        t.crop.1.eval(0).clamp(0.0, 0.49),
        t.crop.2.eval(0).clamp(0.0, 0.49),
        t.crop.3.eval(0).clamp(0.0, 0.49),
    );
    if l + top + r + b > 1e-4 {
        parts.push(format!(
            "crop=w=trunc(iw*{}/2)*2:h=trunc(ih*{}/2)*2:x=iw*{}:y=ih*{}",
            format_float(1.0 - l - r),
            format_float(1.0 - top - b),
            format_float(l),
            format_float(top),
        ));
    }

    let (sx, sy) = (t.scale.0.eval(0).clamp(0.01, 10.0), t.scale.1.eval(0).clamp(0.01, 10.0));
    if (sx - 1.0).abs() > 1e-4 || (sy - 1.0).abs() > 1e-4 {
        parts.push(format!(
            "scale=trunc(iw*{}/2)*2:trunc(ih*{}/2)*2",
            format_float(sx),
            format_float(sy),
        ));
    }

    let deg = t.rotation.eval(0);
    if deg.abs() > 1e-4 {
        let rad = format_float(deg.to_radians());
        parts.push(format!("rotate={rad}:ow=rotw({rad}):oh=roth({rad}):c=black"));
    }

    if t.flip_h {
        parts.push("hflip".into());
    }
    if t.flip_v {
        parts.push("vflip".into());
    }

    // posición: componer sobre un lienzo del tamaño de la secuencia
    let (px, py) = (t.position.0.eval(0).round() as i64, t.position.1.eval(0).round() as i64);
    if let Some((cw, ch)) = canvas.filter(|_| px != 0 || py != 0) {
        static POS_UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = POS_UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        parts.push(format!(
            "null[p{n}fg];color=c=black:s={cw}x{ch}[p{n}bg];[p{n}bg][p{n}fg]overlay=x=(W-w)/2+{px}:y=(H-h)/2+{py}:shortest=1"
        ));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// Cadena completa de un clip: efectos + transform (en ese orden).
pub fn clip_vf(
    registry: &[EffectDef],
    effects: &[EffectInstance],
    transform: &ue_core::model::Transform2D,
    canvas: Option<(u32, u32)>,
) -> Option<String> {
    match (render_chain(registry, effects), transform_vf(transform, canvas)) {
        (Some(e), Some(t)) => Some(format!("{e},{t}")),
        (Some(e), None) => Some(e),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ue_core::model::EffectInstance;

    fn inst(id: &str, params: &[(&str, f64)], colors: &[(&str, &str)]) -> EffectInstance {
        EffectInstance {
            effect_id: id.into(),
            enabled: true,
            params: params.iter().map(|(k, v)| (k.to_string(), (*v).into())).collect(),
            color_params: colors
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn core_manifests_parse() {
        let reg = core_registry();
        assert!(reg.len() >= 3);
        assert!(find_effect(&reg, "core.chroma_key").is_some());
        assert!(find_effect(&reg, "core.color_correct").is_some());
    }

    #[test]
    fn render_with_defaults_and_overrides() {
        let reg = core_registry();
        let def = find_effect(&reg, "core.color_correct").unwrap();
        // solo brightness cambiado; el resto usa defaults
        let e = inst("core.color_correct", &[("brightness", 0.25)], &[]);
        let s = render_effect(def, &e);
        assert_eq!(s, "eq=brightness=0.25:contrast=1:saturation=1:gamma=1");
    }

    #[test]
    fn float_params_are_clamped() {
        let reg = core_registry();
        let def = find_effect(&reg, "core.color_correct").unwrap();
        let e = inst("core.color_correct", &[("brightness", 99.0)], &[]);
        assert!(render_effect(def, &e).contains("brightness=1"));
    }

    #[test]
    fn color_param_formats_and_falls_back() {
        let reg = core_registry();
        let def = find_effect(&reg, "core.chroma_key").unwrap();
        let e = inst("core.chroma_key", &[], &[("key_color", "#12abEF")]);
        assert!(render_effect(def, &e).contains("color=0x12ABEF"));
        // color corrupto → default del manifest
        let bad = inst("core.chroma_key", &[], &[("key_color", "verde;rm -rf")]);
        assert!(render_effect(def, &bad).contains("color=0x00FF00"));
    }

    #[test]
    fn user_packs_load_report_errors_and_override_core() {
        let dir = std::env::temp_dir().join("ue-user-packs-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vhs")).unwrap();
        std::fs::create_dir_all(dir.join("roto")).unwrap();
        std::fs::create_dir_all(dir.join("override")).unwrap();
        std::fs::write(
            dir.join("vhs/manifest.json"),
            r#"{"id":"user.vhs","name":"VHS","ffmpeg":"noise=alls=20","params":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("roto/manifest.json"), "{esto no es json").unwrap();
        std::fs::write(
            dir.join("override/manifest.json"),
            r#"{"id":"core.gaussian_blur","name":"Blur custom","ffmpeg":"gblur=sigma=99","params":[]}"#,
        )
        .unwrap();

        let (packs, errors) = load_packs_from_dir(&dir);
        assert_eq!(packs.len(), 2);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("roto"));

        let merged = merge_registries(core_registry(), packs);
        assert!(find_effect(&merged, "user.vhs").is_some());
        assert_eq!(
            find_effect(&merged, "core.gaussian_blur").unwrap().name,
            "Blur custom",
            "el pack de usuario sobreescribe al core"
        );
        // dir inexistente → vacío sin error
        let (empty, errs) = load_packs_from_dir(std::path::Path::new("/no/existe"));
        assert!(empty.is_empty() && errs.is_empty());
    }

    #[test]
    fn transform_noop_is_none_and_order_is_crop_scale_rotate_flip() {
        use ue_core::model::Transform2D;
        assert_eq!(transform_vf(&Transform2D::default(), None), None);

        let mut t = Transform2D::default();
        t.crop.0 = 0.25.into(); // 25% por la izquierda
        t.scale = (0.5.into(), 0.5.into());
        t.rotation = 180.0.into();
        t.flip_h = true;
        let vf = transform_vf(&t, None).unwrap();
        let crop_pos = vf.find("crop=").unwrap();
        let scale_pos = vf.find("scale=").unwrap();
        let rot_pos = vf.find("rotate=").unwrap();
        let flip_pos = vf.find("hflip").unwrap();
        assert!(crop_pos < scale_pos && scale_pos < rot_pos && rot_pos < flip_pos, "{vf}");
        assert!(vf.contains("rotate=3.1416"), "180° en radianes: {vf}");
        assert!(vf.contains("x=iw*0.25"), "{vf}");
    }

    #[test]
    fn clip_vf_combines_effects_then_transform() {
        use ue_core::model::Transform2D;
        let reg = core_registry();
        let fx = [inst("core.gaussian_blur", &[("sigma", 3.0)], &[])];
        let mut t = Transform2D::default();
        t.flip_v = true;
        let vf = clip_vf(&reg, &fx, &t, None).unwrap();
        assert!(vf.starts_with("gblur"), "{vf}");
        assert!(vf.ends_with("vflip"), "{vf}");
    }

    #[test]
    fn chain_joins_enabled_only() {
        let reg = core_registry();
        let mut off = inst("core.gaussian_blur", &[("sigma", 5.0)], &[]);
        off.enabled = false;
        let on = inst("core.color_correct", &[("saturation", 1.5)], &[]);
        let chain = render_chain(&reg, &[off, on]).unwrap();
        assert!(!chain.contains("gblur"));
        assert!(chain.contains("saturation=1.5"));
        assert!(render_chain(&reg, &[]).is_none());
        // efecto desconocido se ignora sin romper
        let unknown = inst("user.no_existe", &[], &[]);
        assert!(render_chain(&reg, &[unknown]).is_none());
    }
}
