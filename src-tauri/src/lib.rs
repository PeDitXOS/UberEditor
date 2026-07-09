//! Backend Tauri: expone el ProjectStore de ue-core como comandos IPC.
//! El frontend consulta el estado tras cada mutación (v0); los eventos
//! `state.patch` llegarán cuando el volumen de datos lo justifique.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, Manager, State};
use ue_audio::items::{collect_specs, load_items};
use ue_audio::player::Player;
use ue_core::model::{AudioProps, Clip, Id, MediaKind, Project, TrackKind, Transform2D};
use ue_core::ops::InsertMode;
use ue_core::{ProjectStore, TimeUs};
use ue_media::stream::MjpegSession;

pub mod mcp;

pub struct AppState {
    pub store: Mutex<ProjectStore>,
    pub path: Mutex<Option<PathBuf>>,
    pub cache_dir: Mutex<Option<PathBuf>>,
    pub player: Mutex<Option<Player>>,
    pub frames: Mutex<Option<FrameService>>,
    pub export_cancel: Arc<AtomicBool>,
    /// Registro efectivo (core + packs de usuario) y packs de usuario crudos.
    pub registry: Mutex<Arc<Vec<ue_render::EffectDef>>>,
    pub user_packs: Mutex<Vec<ue_render::EffectDef>>,
    pub effects_dir: Mutex<Option<PathBuf>>,
    pub mcp_port: Mutex<Option<u16>>,
    pub mcp_shutdown: AtomicBool,
    pub mcp_token: Mutex<String>,
    pub models_dir: Mutex<Option<PathBuf>>,
    /// Cachés de visuales del timeline (por asset).
    pub peaks_cache: Mutex<std::collections::HashMap<Id, Arc<Vec<f32>>>>,
    pub thumbs_cache: Mutex<std::collections::HashMap<Id, ue_media::thumbs::ThumbStrip>>,
}

impl AppState {
    /// Estado inicial (también usado por los tests del servidor MCP).
    pub fn new_default() -> Self {
        AppState {
            store: Mutex::new(ProjectStore::new(Project::new("Proyecto sin título"))),
            path: Mutex::new(None),
            cache_dir: Mutex::new(None),
            player: Mutex::new(None),
            frames: Mutex::new(None),
            export_cancel: Arc::new(AtomicBool::new(false)),
            registry: Mutex::new(Arc::new(ue_render::core_registry())),
            user_packs: Mutex::new(vec![]),
            effects_dir: Mutex::new(None),
            mcp_port: Mutex::new(None),
            mcp_shutdown: AtomicBool::new(false),
            mcp_token: Mutex::new(Id::new().to_string().to_lowercase()),
            models_dir: Mutex::new(None),
            peaks_cache: Mutex::new(std::collections::HashMap::new()),
            thumbs_cache: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// Recarga los packs de usuario desde disco y reconstruye el registro.
/// Devuelve los errores de manifests inválidos (no rompen nada).
fn reload_packs(state: &AppState) -> Vec<String> {
    let dir = state.effects_dir.lock().unwrap().clone();
    let (user, errors) = match dir {
        Some(d) => ue_render::load_packs_from_dir(&d),
        None => (vec![], vec![]),
    };
    let merged = ue_render::merge_registries(ue_render::core_registry(), user.clone());
    *state.user_packs.lock().unwrap() = user;
    *state.registry.lock().unwrap() = Arc::new(merged);
    errors
}

/// Servicio de frames de reproducción: un hilo sigue al reloj de audio con una
/// sesión MJPEG persistente y publica el último frame decodificado.
pub struct FrameService {
    pub latest: Arc<Mutex<Vec<u8>>>,
    pub running: Arc<AtomicBool>,
}

const PLAYBACK_FPS: u32 = 24;
const PLAYBACK_MAX_W: u32 = 960;

fn frame_service_loop(app: tauri::AppHandle, latest: Arc<Mutex<Vec<u8>>>, running: Arc<AtomicBool>) {
    let mut session: Option<MjpegSession> = None;
    let mut session_vf: Option<String> = None;
    while running.load(Ordering::SeqCst) {
        let state = app.state::<AppState>();
        let (t, playing) = {
            let guard = state.player.lock().unwrap();
            match guard.as_ref() {
                Some(p) => (p.position_us(), p.is_playing()),
                None => (0, false),
            }
        };
        if !playing {
            break;
        }
        let resolved = {
            let store = state.store.lock().unwrap();
            ue_media::frame::resolve_top_video(
                &store.project,
                store.project.active_sequence,
                t,
            )
        };
        let Some(r) = resolved else {
            latest.lock().unwrap().clear();
            session = None;
            std::thread::sleep(Duration::from_millis(40));
            continue;
        };
        let path = PathBuf::from(&r.asset_path);
        let src_t = r.src_t_us;
        let reg = state.registry.lock().unwrap().clone();
        let canvas = {
            let store = state.store.lock().unwrap();
            store
                .project
                .sequence(store.project.active_sequence)
                .map(|s| s.resolution)
        };
        // vf canónico (tvar="t") solo para COMPARAR: no cambia con el playhead
        let (av_canonical, av_open) = {
            let store = state.store.lock().unwrap();
            let seq_id = store.project.active_sequence;
            (
                avatar_vf_stream_suffix(&store.project, seq_id, 0, 1.0, PLAYBACK_MAX_W),
                // tl0 = posición de timeline en este tick (t=0 de la sesión nueva)
                avatar_vf_stream_suffix(&store.project, seq_id, t, r.speed, PLAYBACK_MAX_W),
            )
        };
        let mut vf = ue_render::clip_vf(&reg, &r.effects, &r.transform, canvas);
        if let Some(av) = &av_canonical {
            vf = Some(format!("{}{}", vf.as_deref().unwrap_or("null"), av));
        }

        // ¿sirve la sesión actual? (mismo archivo, misma cadena de efectos,
        // posición alcanzable hacia delante)
        let reusable = session.as_ref().is_some_and(|s| {
            s.asset_path == path
                && session_vf == vf
                // hacia atrás (shuttle J): tolerar hasta 400 ms sin reabrir para
                // no lanzar un ffmpeg por tick; el frame se congela ese margen
                && src_t >= s.next_src_us() - 400_000
                && src_t <= s.next_src_us() + 1_500_000
        });
        if !reusable {
            // el stream corre con -ss: t=0 en el punto de apertura, a ritmo de
            // FUENTE → tiempo de clip = t/speed + offset_al_abrir. Así las
            // curvas de transform ANIMAN también durante la reproducción.
            let rel0 = r.clip_rel_us as f64 / 1_000_000.0;
            let tvar = if (r.speed - 1.0).abs() > 1e-9 {
                format!("(t/{}+{rel0:.6})", r.speed)
            } else {
                format!("(t+{rel0:.6})")
            };
            let mut open_vf = ue_render::clip_vf_at(&reg, &r.effects, &r.transform, canvas, &tvar);
            if let Some(av) = &av_open {
                open_vf = Some(format!("{}{}", open_vf.as_deref().unwrap_or("null"), av));
            }
            session =
                MjpegSession::open(&path, src_t, PLAYBACK_MAX_W, PLAYBACK_FPS, open_vf.as_deref())
                    .ok();
            session_vf = vf;
        }
        if let Some(s) = session.as_mut() {
            let mut newest: Option<Vec<u8>> = None;
            let mut dead = false;
            while s.next_src_us() <= src_t {
                match s.next_frame() {
                    Ok(Some(f)) => newest = Some(f),
                    _ => {
                        dead = true;
                        break;
                    }
                }
            }
            if dead {
                session = None;
            }
            if let Some(f) = newest {
                *latest.lock().unwrap() = f;
            }
        }
        std::thread::sleep(Duration::from_millis(1000 / PLAYBACK_FPS as u64 / 2));
    }
    running.store(false, Ordering::SeqCst);
}

fn start_frame_service(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let mut guard = state.frames.lock().unwrap();
    if let Some(fs) = guard.as_ref() {
        if fs.running.load(Ordering::SeqCst) {
            return; // ya corre
        }
    }
    let latest = Arc::new(Mutex::new(Vec::new()));
    let running = Arc::new(AtomicBool::new(true));
    *guard = Some(FrameService { latest: latest.clone(), running: running.clone() });
    let app2 = app.clone();
    std::thread::spawn(move || frame_service_loop(app2, latest, running));
}

fn stop_frame_service(state: &AppState) {
    if let Some(fs) = state.frames.lock().unwrap().as_ref() {
        fs.running.store(false, Ordering::SeqCst);
    }
}

/// Ruta del autosave: junto al .uep si existe, si no en app_data.
fn autosave_path(project_path: Option<&Path>, data_dir: Option<&Path>) -> Option<PathBuf> {
    match project_path {
        Some(p) => Some(p.with_extension("uep.autosave")),
        None => data_dir.map(|d| d.join("recuperacion.uep.autosave")),
    }
}

/// Hilo de autoguardado: cada `settings.autosave_secs`, si hay cambios sin
/// guardar escribe una copia de recuperación (atómica, portable).
fn autosave_loop(app: tauri::AppHandle, data_dir: Option<PathBuf>) {
    let mut last_version: u64 = 0;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let state = app.state::<AppState>();
        let (dirty, version, secs, project_path) = {
            let store = state.store.lock().unwrap();
            (
                store.dirty,
                store.version,
                store.project.settings.autosave_secs.max(10) as u64,
                state.path.lock().unwrap().clone(),
            )
        };
        // respetar la cadencia configurada muestreando cada 5 s
        static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let t = TICKS.fetch_add(5, Ordering::Relaxed) + 5;
        if t % secs.max(5) >= 5 || !dirty || version == last_version {
            continue;
        }
        let Some(target) = autosave_path(project_path.as_deref(), data_dir.as_deref()) else {
            continue;
        };
        let json = {
            let store = state.store.lock().unwrap();
            let portable = make_portable(&store.project, target.parent());
            portable.to_json()
        };
        if let Ok(json) = json {
            let tmp = target.with_extension("autosave.tmp");
            if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &target).is_ok() {
                last_version = version;
            }
        }
    }
}

/// Copia del proyecto lista para disco: rutas de media relativas al .uep
/// cuando sea posible y SIN cachés locales (son de esta máquina).
pub fn make_portable(project: &Project, dir: Option<&Path>) -> Project {
    let mut portable = project.clone();
    for asset in &mut portable.assets {
        if let Some(dir) = dir {
            let p = Path::new(&asset.path);
            if p.is_absolute() {
                if let Ok(rel) = p.strip_prefix(dir) {
                    asset.path = rel.to_string_lossy().into_owned();
                }
            }
        }
        asset.proxy = None;
        asset.audio_conform = None;
        asset.peaks = None;
        asset.thumbnails = None;
    }
    portable
}

/// Resuelve rutas relativas contra la carpeta del proyecto y marca offline.
pub fn resolve_project_paths(project: &mut Project, dir: Option<&Path>) {
    for asset in &mut project.assets {
        let p = Path::new(&asset.path);
        if !p.is_absolute() {
            if let Some(d) = dir {
                asset.path = d.join(p).to_string_lossy().into_owned();
            }
        }
        asset.offline = !Path::new(&asset.path).exists();
    }
}

/// Ruta del WAV conformado de un asset en la caché de la app.
fn conform_target(cache_dir: &Path, content_hash: &str) -> PathBuf {
    cache_dir.join(content_hash.replace(':', "-")).join("audio.wav")
}

/// Sincroniza los items del mezclador con el estado actual (si cambió).
/// Orden de locks SIEMPRE: store → player.
fn sync_player(state: &AppState) -> Result<(), String> {
    let store = state.store.lock().unwrap();
    let mut player_guard = state.player.lock().unwrap();
    if player_guard.is_none() {
        *player_guard = Some(Player::new().map_err(|e| e.to_string())?);
    }
    let player = player_guard.as_ref().unwrap();
    // versión+1 para distinguir del 0 inicial del player
    if player.items_version() != store.version + 1 {
        let specs = collect_specs(&store.project, store.project.active_sequence);
        let (items, _skipped) =
            load_items(&store.project, &specs, |a| a.audio_conform.as_ref().map(PathBuf::from));
        player.set_items(items, store.version + 1);
    }
    Ok(())
}

#[derive(Serialize)]
pub struct StateSnapshot {
    pub project: Project,
    pub version: u64,
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub undo_labels: Vec<String>,
}

fn snapshot(store: &ProjectStore) -> StateSnapshot {
    StateSnapshot {
        project: store.project.clone(),
        version: store.version,
        dirty: store.dirty,
        can_undo: store.can_undo(),
        can_redo: store.can_redo(),
        undo_labels: store.undo_labels().iter().map(|s| s.to_string()).collect(),
    }
}

fn parse_id(s: &str) -> Result<Id, String> {
    s.parse::<Id>().map_err(|e| format!("id inválido '{s}': {e}"))
}

type Res<T> = Result<T, String>;

#[tauri::command]
fn set_project_settings(
    state: State<AppState>,
    whisper_language: String,
    whisper_model: String,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.project.settings.whisper_language = whisper_language;
    store.project.settings.whisper_model = whisper_model;
    store.version += 1;
    store.dirty = true;
    Ok(snapshot(&store))
}

#[tauri::command]
fn get_state(state: State<AppState>) -> Res<StateSnapshot> {
    Ok(snapshot(&state.store.lock().unwrap()))
}

#[tauri::command]
fn split_clip(state: State<AppState>, clip_id: String, t_us: TimeUs) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.split_clip(parse_id(&clip_id)?, t_us).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn delete_clips(state: State<AppState>, ids: Vec<String>, ripple: bool) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let ids: Result<Vec<Id>, String> = ids.iter().map(|s| parse_id(s)).collect();
    store.delete_clips(&ids?, ripple).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn move_clip(
    state: State<AppState>,
    clip_id: String,
    to_track: String,
    to_start_us: TimeUs,
    overwrite: bool,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let mode = if overwrite { InsertMode::Overwrite } else { InsertMode::Strict };
    store
        .move_clip(parse_id(&clip_id)?, parse_id(&to_track)?, to_start_us, mode)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn trim_clip(
    state: State<AppState>,
    clip_id: String,
    left: bool,
    new_edge_us: TimeUs,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.trim_clip(parse_id(&clip_id)?, left, new_edge_us).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn cut_ranges(
    state: State<AppState>,
    sequence_id: String,
    ranges: Vec<(TimeUs, TimeUs)>,
    ripple: bool,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store
        .cut_ranges(parse_id(&sequence_id)?, &ranges, ripple)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Fuentes del sistema (familia, ruta) para el selector de texto.
#[tauri::command]
fn list_fonts() -> Vec<(String, String)> {
    ue_export::graph::list_system_fonts()
}

fn templates_path(state: &AppState) -> Option<PathBuf> {
    state.effects_dir.lock().unwrap().as_ref().map(|d| {
        d.parent().unwrap_or(d).join("text_templates.json")
    })
}

/// Plantillas de título guardadas (nombre → estilo).
#[tauri::command]
fn list_text_templates(state: State<AppState>) -> Res<serde_json::Value> {
    let Some(path) = templates_path(&state) else { return Ok(serde_json::json!({})) };
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
        Err(_) => Ok(serde_json::json!({})),
    }
}

#[tauri::command]
fn save_text_template(
    state: State<AppState>,
    name: String,
    style: ue_core::model::TextStyle,
) -> Res<serde_json::Value> {
    let Some(path) = templates_path(&state) else { return Err("sin carpeta de config".into()) };
    let mut all: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    all.insert(name, serde_json::to_value(style).map_err(|e| e.to_string())?);
    std::fs::write(&path, serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::Value::Object(all))
}

/// Rompe el enlace video↔audio de un clip (todo su grupo, 1 undo).
#[tauri::command]
fn unlink_clip(state: State<AppState>, clip_id: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    let members = ue_core::ops::linked_ids(&store.project, id);
    if members.len() < 2 {
        return Err("el clip no está enlazado".into());
    }
    let actions = members
        .into_iter()
        .map(|clip_id| ue_core::Action::SetClipGroup { clip_id, group: None })
        .collect();
    store.dispatch("Desenlazar clips", actions).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_subtitles_props(
    state: State<AppState>,
    clip_id: String,
    style: ue_core::model::TextStyle,
    mode: ue_core::model::SubtitleMode,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar subtítulos",
            vec![ue_core::Action::SetClipSubtitles { clip_id: id, style, mode }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Cambia la velocidad de un clip (rate stretch, pitch preservado en export).
#[tauri::command]
fn set_clip_speed(state: State<AppState>, clip_id: String, speed: f64) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.set_clip_speed(parse_id(&clip_id)?, speed).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Mueve un rango del timeline a otro punto (todas las pistas, 1 undo).
#[tauri::command]
fn move_range(
    state: State<AppState>,
    sequence_id: String,
    from_us: TimeUs,
    to_us: TimeUs,
    dest_us: TimeUs,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store
        .move_range(parse_id(&sequence_id)?, from_us, to_us, dest_us)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn undo(state: State<AppState>) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.undo().map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn redo(state: State<AppState>) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    store.redo().map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_clip_audio(state: State<AppState>, clip_id: String, audio: AudioProps) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar audio",
            vec![ue_core::Action::SetClipAudio { clip_id: id, audio }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_clip_transform(
    state: State<AppState>,
    clip_id: String,
    transform: Transform2D,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar transformación",
            vec![ue_core::Action::SetClipTransform { clip_id: id, transform }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Importa archivos al pool (probe + hash). No entra al historial (PLAN §6.10).
/// El conformado de audio se lanza en segundo plano; al terminar se emite
/// `state-changed` para que la UI refresque.
#[tauri::command]
fn import_media(
    app: tauri::AppHandle,
    state: State<AppState>,
    paths: Vec<String>,
) -> Res<StateSnapshot> {
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    let mut store = state.store.lock().unwrap();
    let mut errors: Vec<String> = vec![];
    let mut imported = 0usize;
    for p in &paths {
        match ue_media::import_file(Path::new(p)) {
            Ok(asset) => {
                // re-import del mismo contenido → no duplicar
                if !store.project.assets.iter().any(|a| a.content_hash == asset.content_hash) {
                    if let Some(cache) = &cache_dir {
                        if asset.probe.audio_channels > 0 {
                            spawn_conform_job(&app, &asset, cache);
                        }
                        spawn_proxy_job(&app, &asset, cache);
                    }
                    store.project.assets.push(asset);
                }
                imported += 1;
            }
            Err(e) => errors.push(format!("{p}: {e}")),
        }
    }
    if imported > 0 {
        store.version += 1;
        store.dirty = true;
    }
    if imported == 0 && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(snapshot(&store))
}

fn spawn_conform_job(app: &tauri::AppHandle, asset: &ue_core::model::MediaAsset, cache: &Path) {
    let app = app.clone();
    let asset_id = asset.id;
    let src = PathBuf::from(&asset.path);
    let out = conform_target(cache, &asset.content_hash);
    std::thread::spawn(move || {
        match ue_media::conform_audio(&src, &out) {
            Ok(()) => {
                let state = app.state::<AppState>();
                {
                    let mut store = state.store.lock().unwrap();
                    if let Some(a) = store.project.assets.iter_mut().find(|a| a.id == asset_id) {
                        a.audio_conform = Some(out.to_string_lossy().into_owned());
                    }
                    store.version += 1;
                }
                let _ = app.emit("state-changed", ());
            }
            Err(e) => eprintln!("[conform] {src:?}: {e}"),
        }
    });
}

/// Genera el proxy de preview en segundo plano para videos grandes.
fn spawn_proxy_job(app: &tauri::AppHandle, asset: &ue_core::model::MediaAsset, cache: &Path) {
    // solo vale la pena si el original es más ancho que el proxy
    if asset.kind != ue_core::model::MediaKind::Video
        || asset.probe.width <= ue_media::proxy::PROXY_MAX_W
    {
        return;
    }
    let app = app.clone();
    let asset_id = asset.id;
    let src = PathBuf::from(&asset.path);
    let cache = cache.to_path_buf();
    let hash = asset.content_hash.clone();
    std::thread::spawn(move || {
        match ue_media::proxy::generate_proxy(&src, &cache, &hash) {
            Ok(out) => {
                let state = app.state::<AppState>();
                {
                    let mut store = state.store.lock().unwrap();
                    if let Some(a) = store.project.assets.iter_mut().find(|a| a.id == asset_id) {
                        a.proxy = Some(out.to_string_lossy().into_owned());
                    }
                    store.version += 1;
                }
                // el FrameService detecta el cambio de ruta y reabre la sesión
                let _ = app.emit("state-changed", ());
            }
            Err(e) => eprintln!("[proxy] {src:?}: {e}"),
        }
    });
}

// ---- transporte (el audio es el reloj maestro) ----

#[tauri::command]
fn playback_play(app: tauri::AppHandle, state: State<AppState>, from_us: TimeUs) -> Res<()> {
    sync_player(&state)?;
    {
        let guard = state.player.lock().unwrap();
        guard.as_ref().unwrap().play(from_us);
    }
    start_frame_service(&app);
    Ok(())
}

#[tauri::command]
fn playback_pause(state: State<AppState>) -> Res<TimeUs> {
    stop_frame_service(&state);
    let guard = state.player.lock().unwrap();
    match guard.as_ref() {
        Some(p) => Ok(p.pause()),
        None => Err("sin reproductor".into()),
    }
}

/// Último frame del stream de reproducción (vacío = sin señal todavía).
#[tauri::command]
fn playback_frame(state: State<AppState>) -> Res<tauri::ipc::Response> {
    let bytes = match state.frames.lock().unwrap().as_ref() {
        Some(fs) => fs.latest.lock().unwrap().clone(),
        None => vec![],
    };
    Ok(tauri::ipc::Response::new(bytes))
}

/// Picos de audio reales del asset (25 bins/s, mezcla mono), para la waveform
/// del timeline. Cachea en memoria y en disco junto al conformado.
#[tauri::command]
async fn get_audio_peaks(state: State<'_, AppState>, asset_id: String) -> Res<Vec<f32>> {
    let id: Id = asset_id.parse().map_err(|_| "id inválido")?;
    if let Some(p) = state.peaks_cache.lock().unwrap().get(&id) {
        return Ok(p.as_ref().clone());
    }
    let conform = {
        let store = state.store.lock().unwrap();
        store
            .project
            .asset(id)
            .ok_or("asset no encontrado")?
            .audio_conform
            .clone()
            .ok_or("audio sin conformar todavía")?
    };
    let peaks = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<f32>, String> {
        let disk = PathBuf::from(format!("{conform}.peaks"));
        if let Ok(bytes) = std::fs::read(&disk) {
            return Ok(bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect());
        }
        let wav =
            ue_audio::wav::WavMap::open(Path::new(&conform)).map_err(|e| e.to_string())?;
        let peaks = ue_audio::wav::compute_peaks(&wav, 25);
        let bytes: Vec<u8> = peaks.iter().flat_map(|f| f.to_le_bytes()).collect();
        let _ = std::fs::write(&disk, bytes); // caché best-effort
        Ok(peaks)
    })
    .await
    .map_err(|e| e.to_string())??;
    state.peaks_cache.lock().unwrap().insert(id, Arc::new(peaks.clone()));
    Ok(peaks)
}

/// Genera (o devuelve del caché) la tira de miniaturas del asset.
#[tauri::command]
async fn ensure_thumbs(
    state: State<'_, AppState>,
    asset_id: String,
) -> Res<ue_media::thumbs::ThumbStrip> {
    let id: Id = asset_id.parse().map_err(|_| "id inválido")?;
    if let Some(t) = state.thumbs_cache.lock().unwrap().get(&id) {
        return Ok(t.clone());
    }
    let (src, dur, hash, cache_dir) = {
        let store = state.store.lock().unwrap();
        let asset = store.project.asset(id).ok_or("asset no encontrado")?;
        if asset.kind == ue_core::model::MediaKind::Audio {
            return Err("los assets de audio no llevan miniaturas".into());
        }
        let cache = state.cache_dir.lock().unwrap().clone().ok_or("sin caché")?;
        (
            PathBuf::from(&asset.path),
            asset.probe.duration_us.max(1_000_000),
            asset.content_hash.clone(),
            cache,
        )
    };
    let strip = tauri::async_runtime::spawn_blocking(move || {
        ue_media::thumbs::generate_thumb_strip(&src, dur, &cache_dir, &hash)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    state.thumbs_cache.lock().unwrap().insert(id, strip.clone());
    Ok(strip)
}

/// Bytes JPEG de la tira de miniaturas ya generada.
#[tauri::command]
fn get_thumb_strip(state: State<AppState>, asset_id: String) -> Res<tauri::ipc::Response> {
    let id: Id = asset_id.parse().map_err(|_| "id inválido")?;
    let path = state
        .thumbs_cache
        .lock()
        .unwrap()
        .get(&id)
        .map(|t| t.path.clone())
        .ok_or("miniaturas no generadas (llama ensure_thumbs)")?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Shuttle JKL: fija la velocidad de reproducción (negativa = reversa).
/// Si no estaba sonando, arranca desde el playhead dado.
#[tauri::command]
fn playback_set_rate(
    app: tauri::AppHandle,
    state: State<AppState>,
    rate: f64,
    from_us: TimeUs,
) -> Res<()> {
    sync_player(&state)?;
    {
        let guard = state.player.lock().unwrap();
        let p = guard.as_ref().ok_or("sin reproductor")?;
        if !p.is_playing() {
            p.play(from_us);
        }
        p.set_rate(rate);
    }
    start_frame_service(&app);
    Ok(())
}

#[tauri::command]
fn playback_seek(state: State<AppState>, t_us: TimeUs) -> Res<()> {
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.seek(t_us);
    }
    Ok(())
}

/// (posición µs, reproduciendo). También re-sincroniza los items si el
/// proyecto cambió durante la reproducción (editar mientras suena).
#[tauri::command]
fn playback_position(state: State<AppState>) -> Res<(TimeUs, bool, f32, f32)> {
    let _ = sync_player(&state); // barato si no cambió la versión
    let guard = state.player.lock().unwrap();
    match guard.as_ref() {
        Some(p) => {
            let (ml, mr) = p.meters();
            Ok((p.position_us(), p.is_playing(), ml, mr))
        }
        None => Err("sin reproductor".into()),
    }
}

/// Añade un clip del asset a la primera pista compatible: en `at_us` si cabe,
/// si no al final de la pista.
#[tauri::command]
fn add_clip(state: State<AppState>, asset_id: String, at_us: TimeUs) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let asset_id = parse_id(&asset_id)?;
    let asset = store
        .project
        .asset(asset_id)
        .ok_or_else(|| format!("asset {asset_id} no existe"))?
        .clone();
    let duration = ue_media::default_clip_duration(&asset);
    if duration <= 0 {
        return Err("el archivo no tiene duración utilizable".into());
    }
    let want_kind = if asset.kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video };
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("secuencia activa no existe")?;
    let track = seq
        .tracks
        .iter()
        .find(|t| t.kind == want_kind && !t.locked)
        .ok_or("no hay pista compatible desbloqueada")?;
    let track_id = track.id;
    let at = at_us.max(0);
    let fits = !track.collides(at, duration, None);
    let start = if fits {
        at
    } else {
        track.clips.iter().map(|c| c.end()).max().unwrap_or(0)
    };
    let clip = Clip::new_media(asset.id, 0, duration, start);
    store
        .insert_clip(track_id, clip, InsertMode::Strict)
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Overlay del avatar activo en `t` para el frame pausado: sufijo -vf con
/// movie+overlay (estático, sin shake: es un frame quieto).
fn avatar_vf_suffix(
    project: &Project,
    seq_id: Id,
    t_us: TimeUs,
    out_w: u32,
) -> Option<String> {
    use ue_core::model::{ClipPayload, TrackKind};
    let seq = project.sequence(seq_id)?;
    for track in seq.tracks.iter().rev().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            let ClipPayload::Avatar { driver_asset, avatars, scale, .. } = &clip.payload else {
                continue;
            };
            if !(clip.start <= t_us && t_us < clip.end()) {
                continue;
            }
            let default = avatars.keys().next()?.clone();
            // emoción del segmento activo (o default)
            let emotion = project
                .transcripts
                .iter()
                .find(|d| d.asset_id == *driver_asset)
                .and_then(|doc| {
                    doc.segments.iter().find_map(|seg| {
                        let tl = crate::asset_tl(project, seq, *driver_asset, seg.start_us)?;
                        let end = tl + (seg.end_us - seg.start_us);
                        (tl <= t_us && t_us < end).then(|| seg.emotion.clone()).flatten()
                    })
                })
                .unwrap_or(default.clone());
            let path = avatars.get(&emotion).or_else(|| avatars.get(&default))?;
            if !Path::new(path).exists() {
                return None;
            }
            let aw = (((out_w as f64) * scale.clamp(0.05, 1.0)) as u32) & !1;
            let escaped = path.replace('\\', "/").replace(':', "\\\\:").replace('\'', "\\\\'");
            // escalar el main ANTES del overlay para que aw sea proporcional
            return Some(format!(
                ",scale='min({out_w},iw)':-2[main];movie=filename='{escaped}',\
                 scale={aw}:-2[av];[main][av]overlay=W-w-16:H-h-16"
            ));
        }
    }
    None
}

/// Overlays de avatar para el STREAM de reproducción. A diferencia del export
/// (un overlay por segmento), agrupa por EMOCIÓN — una sola instancia de
/// `movie` por video de avatar — y enciende cada una con ventanas `enable`
/// expresadas en el dominio t de la sesión: t corre en segundos de FUENTE
/// desde el punto de -ss, así que timeline = tl0 + t/speed ⇒ una ventana
/// [from,to] de timeline es [(from-tl0)·speed, (to-tl0)·speed] en t.
/// Con `tl0_us=0, speed=1` produce la forma CANÓNICA (estable entre ticks)
/// que el FrameService usa para decidir si reabre la sesión.
pub fn avatar_vf_stream_suffix(
    project: &Project,
    seq_id: Id,
    tl0_us: TimeUs,
    speed: f64,
    out_w: u32,
) -> Option<String> {
    use std::collections::BTreeMap;
    use ue_core::model::{ClipPayload, TrackKind};
    let seq = project.sequence(seq_id)?;
    for track in seq.tracks.iter().rev().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            let ClipPayload::Avatar { driver_asset, avatars, scale, .. } = &clip.payload else {
                continue;
            };
            let default = avatars.keys().next()?.clone();
            // tramos (emoción, from_tl, to_tl) rellenando huecos con la default
            let ce = clip.end();
            let mut spans: Vec<(String, TimeUs, TimeUs)> = vec![];
            let mut cursor = clip.start;
            if let Some(doc) = project.transcripts.iter().find(|d| d.asset_id == *driver_asset)
            {
                for seg in &doc.segments {
                    let Some(tl) = asset_tl(project, seq, *driver_asset, seg.start_us) else {
                        continue;
                    };
                    let from = tl.max(clip.start);
                    let to = (tl + (seg.end_us - seg.start_us)).min(ce);
                    if to <= from {
                        continue;
                    }
                    if from > cursor {
                        spans.push((default.clone(), cursor, from));
                    }
                    spans.push((seg.emotion.clone().unwrap_or_else(|| default.clone()), from, to));
                    cursor = to.max(cursor);
                }
            }
            if cursor < ce {
                spans.push((default.clone(), cursor, ce));
            }
            // agrupar ventanas por video de avatar (emociones sin video → default)
            let mut windows: BTreeMap<String, Vec<(TimeUs, TimeUs)>> = BTreeMap::new();
            for (emotion, from, to) in spans {
                let key = if avatars.contains_key(&emotion) { emotion } else { default.clone() };
                windows.entry(key).or_default().push((from, to));
            }
            let aw = (((out_w as f64) * scale.clamp(0.05, 1.0)) as u32) & !1;
            let mut out = format!(",scale='min({out_w},iw)':-2[avm0]");
            let mut stage = 0usize;
            let n_total = windows.len();
            for (i, (emotion, wins)) in windows.iter().enumerate() {
                let Some(path) = avatars.get(emotion) else { continue };
                if !Path::new(path).exists() {
                    return None;
                }
                let escaped =
                    path.replace('\\', "/").replace(':', "\\\\:").replace('\'', "\\\\'");
                let enable = wins
                    .iter()
                    .map(|(f, t)| {
                        format!(
                            "between(t,{:.4},{:.4})",
                            ((f - tl0_us) as f64 / 1e6 * speed).max(0.0),
                            ((t - tl0_us) as f64 / 1e6 * speed).max(0.0),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("+");
                // loop=0 = infinito; setpts monótono para que overlay no se atasque
                out.push_str(&format!(
                    ";movie=filename='{escaped}':loop=0,setpts=N/(FRAME_RATE*TB),\
                     scale={aw}:-2[ave{stage}]"
                ));
                let dst = if i + 1 == n_total {
                    String::new()
                } else {
                    format!("[avm{}]", stage + 1)
                };
                out.push_str(&format!(
                    ";[avm{stage}][ave{stage}]overlay=W-w-16:H-h-16:enable='{enable}'{dst}"
                ));
                stage += 1;
            }
            if stage == 0 {
                return None;
            }
            return Some(out);
        }
    }
    None
}

/// Tiempo de asset → timeline (helper compartido con avatar_vf_suffix).
pub(crate) fn asset_tl(
    _project: &Project,
    seq: &ue_core::model::Sequence,
    asset_id: Id,
    t_asset: TimeUs,
) -> Option<TimeUs> {
    use ue_core::model::ClipPayload;
    for track in &seq.tracks {
        for clip in &track.clips {
            if let ClipPayload::Media { asset_id: aid, src_in, src_out } = &clip.payload {
                if *aid == asset_id && t_asset >= *src_in && t_asset < *src_out {
                    return Some(clip.start + (t_asset - src_in));
                }
            }
        }
    }
    None
}

/// Frame real JPEG del tiempo dado (bytes crudos; vacío = sin señal).
#[tauri::command]
fn render_frame(
    state: State<AppState>,
    t_us: TimeUs,
    max_width: u32,
) -> Res<tauri::ipc::Response> {
    let (project, seq_id, base_dir) = {
        let store = state.store.lock().unwrap();
        let base = state
            .path
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        (store.project.clone(), store.project.active_sequence, base)
    }; // soltar el lock antes de invocar ffmpeg
    let reg = state.registry.lock().unwrap().clone();
    let canvas = project.sequence(seq_id).map(|s| s.resolution);
    // scrub animado: evaluar las curvas del transform en el tiempo del clip
    let mut vf = ue_media::frame::resolve_top_video(&project, seq_id, t_us)
        .and_then(|r| {
            ue_render::clip_vf(&reg, &r.effects, &r.transform.sampled(r.clip_rel_us), canvas)
        });
    // avatar sobre el frame pausado (grafo movie+overlay en el mismo -vf)
    if let Some(suffix) = avatar_vf_suffix(&project, seq_id, t_us, max_width) {
        vf = Some(format!("{}{}", vf.unwrap_or_else(|| "null".into()), suffix));
    }
    let bytes =
        ue_media::frame::render_frame(&project, seq_id, t_us, max_width, &base_dir, vf.as_deref())
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
    Ok(tauri::ipc::Response::new(bytes))
}

/// Catálogo de efectos disponibles (para la UI y MCP).
#[tauri::command]
fn get_effects_catalog(state: State<AppState>) -> serde_json::Value {
    ue_render::catalog_json(&state.registry.lock().unwrap())
}

/// Recarga los packs de usuario desde disco (carpeta effects/ de la config).
#[tauri::command]
fn reload_effect_packs(state: State<AppState>) -> Res<serde_json::Value> {
    let errors = reload_packs(&state);
    Ok(serde_json::json!({
        "catalog": ue_render::catalog_json(&state.registry.lock().unwrap()),
        "errors": errors,
        "dir": state.effects_dir.lock().unwrap().as_ref().map(|d| d.display().to_string()),
    }))
}

#[tauri::command]
fn set_clip_effects(
    state: State<AppState>,
    clip_id: String,
    effects: Vec<ue_core::model::EffectInstance>,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar efectos",
            vec![ue_core::Action::SetClipEffects { clip_id: id, effects }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Duplica una secuencia con IDs nuevos (los ids son únicos globalmente).
fn duplicate_sequence(seq: &ue_core::model::Sequence) -> ue_core::model::Sequence {
    let mut copy = seq.clone();
    copy.id = Id::new();
    for track in &mut copy.tracks {
        track.id = Id::new();
        for clip in &mut track.clips {
            clip.id = Id::new();
        }
    }
    for marker in &mut copy.markers {
        marker.id = Id::new();
    }
    copy
}

/// Genera la versión vertical (1080x1920) de la secuencia activa: nueva
/// secuencia con el efecto core.vertical_fill en cada clip de video (el look
/// fondo-desenfocado del toolkit). Una sola entrada de undo.
pub(crate) fn generate_vertical_impl(state: &AppState) -> Result<String, String> {
    use ue_core::model::{ClipPayload, EffectInstance};
    let mut store = state.store.lock().unwrap();
    let seq = store
        .project
        .sequence(store.project.active_sequence)
        .ok_or("sin secuencia activa")?;
    let mut vertical = duplicate_sequence(seq);
    vertical.name = format!("{} (Vertical)", seq.name);
    vertical.resolution = (1080, 1920);
    for track in vertical.tracks.iter_mut().filter(|t| t.kind == TrackKind::Video) {
        for clip in &mut track.clips {
            if matches!(clip.payload, ClipPayload::Media { .. }) {
                clip.effects.push(EffectInstance {
                    effect_id: "core.vertical_fill".into(),
                    enabled: true,
                    params: Default::default(),
                    color_params: Default::default(),
                });
            }
        }
    }
    let new_id = vertical.id;
    store
        .dispatch(
            "Generar vertical",
            vec![
                ue_core::Action::AddSequence { sequence: vertical },
                ue_core::Action::SetActiveSequence { sequence_id: new_id },
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(new_id.to_string())
}

#[tauri::command]
fn generate_vertical(state: State<AppState>) -> Res<StateSnapshot> {
    generate_vertical_impl(&state)?;
    Ok(snapshot(&state.store.lock().unwrap()))
}

#[tauri::command]
fn set_active_sequence(state: State<AppState>, sequence_id: String) -> Res<StateSnapshot> {
    let id = parse_id(&sequence_id)?;
    let mut store = state.store.lock().unwrap();
    store
        .dispatch(
            "Cambiar secuencia",
            vec![ue_core::Action::SetActiveSequence { sequence_id: id }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Crea un clip de subtítulos automáticos sobre un clip de media transcrito.
#[tauri::command]
fn add_subtitles_clip(state: State<AppState>, clip_id: String) -> Res<StateSnapshot> {
    use ue_core::model::{ClipPayload, SubtitleMode, TextStyle};
    let id = parse_id(&clip_id)?;
    let mut store = state.store.lock().unwrap();
    let media = store.project.clip(id).ok_or("clip no encontrado")?.clone();
    let ClipPayload::Media { asset_id, .. } = media.payload else {
        return Err("el clip no es de media".into());
    };
    let transcript_id = store
        .project
        .transcripts
        .iter()
        .find(|t| t.asset_id == asset_id)
        .map(|t| t.id)
        .ok_or("el medio no tiene transcripción; transcríbelo primero (botón T)")?;
    let track_id = ensure_free_video_track(&mut store, media.start, media.duration)?;
    // tercio inferior a 1080p
    let style = TextStyle { size: 48.0, y_offset: 380.0, ..Default::default() };
    let clip = Clip {
        id: ue_core::model::Id::new(),
        payload: ClipPayload::Subtitles { transcript_id, style, mode: SubtitleMode::Phrase },
        start: media.start,
        duration: media.duration,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
        group: None,
    };
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Crea un clip de Avatar sobre un clip de media transcrito, a partir de un
/// config.json compatible con el avatar_config del Youtubers-toolkit.
/// Clasifica emociones (API OpenAI-compatible si hay OPENAI_API_KEY, si no
/// heurística offline) y mide volúmenes por segmento.
#[tauri::command]
fn add_avatar_clip(state: State<AppState>, clip_id: String, config_path: String) -> Res<StateSnapshot> {
    use ue_core::model::ClipPayload;
    let id = parse_id(&clip_id)?;

    // 1. parsear config del toolkit
    let raw = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let cfg: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let base = Path::new(&config_path).parent().unwrap_or(Path::new("."));
    let mut avatars = std::collections::BTreeMap::new();
    for (emotion, p) in cfg
        .get("avatars")
        .and_then(|v| v.as_object())
        .ok_or("config sin mapa 'avatars'")?
    {
        let path = p.as_str().ok_or("ruta de avatar inválida")?;
        let abs = {
            let pp = Path::new(path);
            if pp.is_absolute() { pp.to_path_buf() } else { base.join(pp.file_name().unwrap_or_default()) }
        };
        // el config del toolkit usa rutas tipo "avatar_config/x.mp4": probar tal cual y por basename
        let candidate = if abs.exists() { abs } else { base.join(path) };
        if candidate.exists() {
            avatars.insert(emotion.clone(), candidate.to_string_lossy().into_owned());
        }
    }
    if avatars.is_empty() {
        return Err("ningún archivo de avatar del config existe en disco".into());
    }
    let shake_factor = cfg.get("shake_factor").and_then(|v| v.as_f64()).unwrap_or(1.0);

    let mut store = state.store.lock().unwrap();
    let media = store.project.clip(id).ok_or("clip no encontrado")?.clone();
    let ClipPayload::Media { asset_id, .. } = media.payload else {
        return Err("el clip no es de media".into());
    };
    let conform = store
        .project
        .asset(asset_id)
        .and_then(|a| a.audio_conform.clone())
        .ok_or("el audio aún se está preparando (conformado)")?;

    // 2. análisis: volúmenes + emociones sobre el transcript existente
    {
        let doc = store
            .project
            .transcripts
            .iter_mut()
            .find(|t| t.asset_id == asset_id)
            .ok_or("el medio no tiene transcripción; transcríbelo primero (botón T)")?;
        let wav = ue_audio::wav::WavMap::open(Path::new(&conform)).map_err(|e| e.to_string())?;
        ue_ai::emotion::measure_volumes(doc, &wav);
        let api = ue_ai::emotion::ApiConfig::from_env();
        ue_ai::emotion::classify_segments(doc, &avatars, api.as_ref());
        store.version += 1;
    }

    // 3. clip Avatar en una pista de video libre (se crea si hace falta)
    let track_id = ensure_free_video_track(&mut store, media.start, media.duration)?;
    let clip = Clip {
        id: Id::new(),
        payload: ClipPayload::Avatar {
            driver_asset: asset_id,
            avatars,
            shake_factor,
            scale: 0.3,
        },
        start: media.start,
        duration: media.duration,
        speed: 1.0,
        effects: vec![],
        transform: Default::default(),
        audio: Default::default(),
        transition_in: None,
        label_color: None,
        group: None,
    };
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Transcribe un asset con Whisper (word-level) en segundo plano.
/// Descarga el modelo ggml si hace falta. Al terminar emite state-changed.
#[tauri::command]
fn transcribe_asset(
    app: tauri::AppHandle,
    state: State<AppState>,
    asset_id: String,
    model: Option<String>,
) -> Res<()> {
    let id = parse_id(&asset_id)?;
    let (conform, models_dir) = {
        let store = state.store.lock().unwrap();
        let asset = store.project.asset(id).ok_or("asset no encontrado")?;
        if asset.probe.audio_channels == 0 {
            return Err("el archivo no tiene audio".into());
        }
        let conform = asset
            .audio_conform
            .clone()
            .ok_or("el audio aún se está preparando (conformado); prueba en unos segundos")?;
        let models = state
            .models_dir
            .lock()
            .unwrap()
            .clone()
            .ok_or("sin carpeta de modelos")?;
        (PathBuf::from(conform), models)
    };
    let (settings_model, settings_lang) = {
        let store = state.store.lock().unwrap();
        (
            store.project.settings.whisper_model.clone(),
            store.project.settings.whisper_language.clone(),
        )
    };
    let model_name = model.unwrap_or(settings_model);
    let lang: Option<String> = (settings_lang != "auto").then_some(settings_lang);
    std::thread::spawn(move || {
        let result = ue_whisper::ensure_model(&models_dir, &model_name)
            .and_then(|m| ue_whisper::transcribe(&conform, &m, lang.as_deref(), id));
        let state = app.state::<AppState>();
        match result {
            Ok(doc) => {
                let mut store = state.store.lock().unwrap();
                let doc_id = doc.id;
                store.project.transcripts.retain(|t| t.asset_id != id);
                store.project.transcripts.push(doc);
                if let Some(a) = store.project.assets.iter_mut().find(|a| a.id == id) {
                    a.transcript = Some(doc_id);
                }
                store.version += 1;
                store.dirty = true;
            }
            Err(e) => eprintln!("[whisper] {conform:?}: {e}"),
        }
        let _ = app.emit("state-changed", ());
    });
    Ok(())
}

/// Elimina los silencios de un clip (corta y cierra huecos en TODAS las
/// pistas: una sola entrada de undo). Requiere el audio conformado.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn remove_silences(
    state: State<AppState>,
    clip_id: String,
    mode: Option<String>,
    threshold_db: Option<f64>,
    min_silence_ms: Option<i64>,
    pad_ms: Option<i64>,
) -> Res<serde_json::Value> {
    let id = parse_id(&clip_id)?;
    let mut store = state.store.lock().unwrap();
    let clip = store.project.clip(id).ok_or("clip no encontrado")?.clone();
    let ue_core::model::ClipPayload::Media { asset_id, src_in, src_out } = clip.payload else {
        return Err("el clip no es de media".into());
    };
    let asset = store.project.asset(asset_id).ok_or("asset no encontrado")?;
    let conform = asset
        .audio_conform
        .clone()
        .ok_or("el audio aún se está preparando (conformado); prueba en unos segundos")?;
    let wav = ue_audio::wav::WavMap::open(Path::new(&conform)).map_err(|e| e.to_string())?;
    let mut params = ue_ai::silence::SilenceParams::default();
    if let Some(db) = threshold_db {
        params.threshold_db = db.clamp(-80.0, -10.0);
    }
    if let Some(ms) = min_silence_ms {
        params.min_silence_us = (ms.clamp(50, 5000)) * 1000;
    }
    if let Some(ms) = pad_ms {
        params.pad_pre_us = (ms.clamp(0, 1000)) * 1000;
        params.pad_post_us = (ms.clamp(0, 1000)) * 1000;
    }
    let ranges =
        ue_ai::silence::clip_silences_on_timeline(&wav, clip.start, src_in, src_out, &params);
    if ranges.is_empty() {
        return Ok(serde_json::json!({ "removed": 0, "removed_us": 0, "snapshot": snapshot(&store) }));
    }
    let removed_us: i64 = ranges.iter().map(|(s, e)| e - s).sum();
    let seq_id = store.project.active_sequence;
    match mode.as_deref() {
        Some("speedup") => {
            store.speedup_ranges(seq_id, &ranges, 4.0).map_err(|e| e.to_string())?;
        }
        _ => {
            store.cut_ranges(seq_id, &ranges, true).map_err(|e| e.to_string())?;
        }
    }
    Ok(serde_json::json!({
        "removed": ranges.len(),
        "removed_us": removed_us,
        "snapshot": snapshot(&store),
    }))
}

/// Pista de video con hueco libre en [start, start+duration); si no existe,
/// añade una pista nueva encima (dentro de la MISMA transacción del caller no:
/// se despacha aparte con su propio undo agrupado por el label).
fn ensure_free_video_track(
    store: &mut ProjectStore,
    start: TimeUs,
    duration: TimeUs,
) -> Result<Id, String> {
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia activa")?;
    if let Some(t) = seq
        .tracks
        .iter()
        .rev()
        .find(|t| t.kind == TrackKind::Video && !t.locked && !t.collides(start, duration, None))
    {
        return Ok(t.id);
    }
    // crear V(n+1) encima de todo
    let n = seq.tracks.iter().filter(|t| t.kind == TrackKind::Video).count();
    let track = ue_core::model::Track::new(TrackKind::Video, &format!("V{}", n + 1));
    let track_id = track.id;
    let index = seq.tracks.len();
    store
        .dispatch(
            "Añadir pista",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index, track }],
        )
        .map_err(|e| e.to_string())?;
    Ok(track_id)
}

/// Añade un clip de texto (título) en la pista de video superior.
#[tauri::command]
fn add_text_clip(state: State<AppState>, content: String, at_us: TimeUs) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let duration = 4_000_000;
    let start = at_us.max(0);
    let track_id = ensure_free_video_track(&mut store, start, duration)?;
    let clip = Clip::new_text(&content, start, duration);
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Catálogo de generadores (manifests core) para la UI.
#[tauri::command]
fn get_generators() -> serde_json::Value {
    ue_render::generators_catalog_json(&ue_render::core_generators())
}

/// Añade un clip generador (rectángulo, degradado, …) en el playhead, en una
/// pista de video libre (se crea si hace falta).
#[tauri::command]
fn add_generator_clip(
    state: State<AppState>,
    generator_id: String,
    at_us: TimeUs,
) -> Res<StateSnapshot> {
    if ue_render::find_generator(&ue_render::core_generators(), &generator_id).is_none() {
        return Err(format!("generador desconocido: {generator_id}"));
    }
    let mut store = state.store.lock().unwrap();
    let duration = 4_000_000;
    let start = at_us.max(0);
    let track_id = ensure_free_video_track(&mut store, start, duration)?;
    let clip = Clip::new_generator(&generator_id, start, duration);
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Edita los parámetros de un generador (deshacible).
#[tauri::command]
fn set_clip_generator(
    state: State<AppState>,
    clip_id: String,
    generator_id: String,
    params: std::collections::BTreeMap<String, ue_core::keyframe::Param>,
    color_params: std::collections::BTreeMap<String, String>,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar generador",
            vec![ue_core::Action::SetClipGenerator {
                clip_id: id,
                generator_id,
                params,
                color_params,
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_clip_text(
    state: State<AppState>,
    clip_id: String,
    content: String,
    style: ue_core::model::TextStyle,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar texto",
            vec![ue_core::Action::SetClipText { clip_id: id, content, style }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_track_prop(
    state: State<AppState>,
    track_id: String,
    prop: String,
    value: bool,
) -> Res<StateSnapshot> {
    use ue_core::action::TrackProp;
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&track_id)?;
    let prop = match prop.as_str() {
        "muted" => TrackProp::Muted(value),
        "solo" => TrackProp::Solo(value),
        "locked" => TrackProp::Locked(value),
        other => return Err(format!("propiedad desconocida: {other}")),
    };
    store
        .dispatch("Pista", vec![ue_core::Action::SetTrackProp { track_id: id, prop }])
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Añade una pista al final de su grupo (video arriba, audio abajo). Deshacible.
#[tauri::command]
fn add_track(state: State<AppState>, kind: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let seq_id = store.project.active_sequence;
    let kind = match kind.as_str() {
        "video" => TrackKind::Video,
        "audio" => TrackKind::Audio,
        other => return Err(format!("tipo de pista desconocido: {other}")),
    };
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia")?;
    let n = seq.tracks.iter().filter(|t| t.kind == kind).count();
    let prefix = if kind == TrackKind::Video { "V" } else { "A" };
    let track = ue_core::model::Track::new(kind, &format!("{prefix}{}", n + 1));
    // video: al final del vec (se dibuja arriba); audio: también al final
    let index = seq.tracks.len();
    store
        .dispatch(
            "Añadir pista",
            vec![ue_core::Action::AddTrack { sequence_id: seq_id, index, track }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Elimina una pista (los clips que tuviera se van con ella; 1 undo la restaura).
#[tauri::command]
fn remove_track(state: State<AppState>, track_id: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&track_id)?;
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia")?;
    // no dejar la secuencia sin pistas de un tipo
    let kind = seq.tracks.iter().find(|t| t.id == id).ok_or("pista no encontrada")?.kind;
    if seq.tracks.iter().filter(|t| t.kind == kind).count() <= 1 {
        return Err("no se puede eliminar la última pista de su tipo".into());
    }
    store
        .dispatch("Eliminar pista", vec![ue_core::Action::RemoveTrack { track_id: id }])
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Renombra una pista (deshacible).
#[tauri::command]
fn rename_track(state: State<AppState>, track_id: String, name: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&track_id)?;
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 24 {
        return Err("nombre de pista inválido".into());
    }
    store
        .dispatch(
            "Renombrar pista",
            vec![ue_core::Action::SetTrackProp {
                track_id: id,
                prop: ue_core::action::TrackProp::Name(name),
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// Volumen de pista en dB (deshacible).
#[tauri::command]
fn set_track_volume(state: State<AppState>, track_id: String, db: f32) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&track_id)?;
    store
        .dispatch(
            "Volumen de pista",
            vec![ue_core::Action::SetTrackProp {
                track_id: id,
                prop: ue_core::action::TrackProp::VolumeDb(db.clamp(-60.0, 12.0)),
            }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

#[tauri::command]
fn set_clip_transition(
    state: State<AppState>,
    clip_id: String,
    transition: Option<ue_core::model::TransitionRef>,
) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let id = parse_id(&clip_id)?;
    store
        .dispatch(
            "Editar transición",
            vec![ue_core::Action::SetClipTransition { clip_id: id, transition }],
        )
        .map_err(|e| e.to_string())?;
    Ok(snapshot(&store))
}

/// (puerto, token) del servidor MCP embebido (None si no pudo arrancar).
#[tauri::command]
fn mcp_status(state: State<AppState>) -> Option<(u16, String)> {
    let port = (*state.mcp_port.lock().unwrap())?;
    Some((port, state.mcp_token.lock().unwrap().clone()))
}

#[tauri::command]
fn cancel_export(state: State<AppState>) -> Res<()> {
    state.export_cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Exporta la secuencia activa a MP4 (bloqueante en un hilo aparte).
/// Emite eventos `export-progress` (0..1); `cancel_export` la aborta.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn export_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    max_height: Option<u32>,
    crf: Option<u8>,
    preset: Option<String>,
    audio_bitrate_k: Option<u32>,
    loudnorm: Option<bool>,
    range_in_us: Option<i64>,
    range_out_us: Option<i64>,
    format: Option<String>,
) -> Res<String> {
    let (project, seq_id, base_dir) = {
        let store = state.store.lock().unwrap();
        let base = state
            .path
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        (store.project.clone(), store.project.active_sequence, base)
    };
    let cancel = state.export_cancel.clone();
    cancel.store(false, Ordering::SeqCst);
    let out = PathBuf::from(&path);
    let extra_packs = state.user_packs.lock().unwrap().clone();
    let defaults = ue_export::ExportSettings::default();
    let range = match (range_in_us, range_out_us) {
        (Some(a), Some(b)) if b > a => Some((a.max(0), b)),
        _ => None,
    };
    let format = match format.as_deref() {
        None | Some("mp4") => ue_export::ExportFormat::Mp4,
        Some("m4a") => ue_export::ExportFormat::M4a,
        Some("gif") => ue_export::ExportFormat::Gif,
        Some(other) => return Err(format!("formato desconocido: {other}")),
    };
    let settings = ue_export::ExportSettings {
        format,
        max_height,
        crf: crf.map(|c| c.clamp(10, 40)).unwrap_or(defaults.crf),
        preset: preset.unwrap_or(defaults.preset),
        audio_bitrate_k: audio_bitrate_k.unwrap_or(defaults.audio_bitrate_k),
        loudnorm: loudnorm.unwrap_or(false),
        range,
        extra_packs,
    };
    tauri::async_runtime::spawn_blocking(move || {
        ue_export::export_sequence_with_progress(
            &project,
            seq_id,
            &base_dir,
            &out,
            &settings,
            |p| {
                let _ = app.emit("export-progress", p);
            },
            &cancel,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(path)
}

/// ¿Hay un autosave más reciente que el proyecto dado (o huérfano)? → su ruta.
#[tauri::command]
fn check_recovery(app: tauri::AppHandle, state: State<AppState>, path: Option<String>) -> Res<Option<String>> {
    let data_dir = app.path().app_data_dir().ok();
    let project_path = path.map(PathBuf::from).or_else(|| state.path.lock().unwrap().clone());
    let Some(auto) = autosave_path(project_path.as_deref(), data_dir.as_deref()) else {
        return Ok(None);
    };
    if !auto.exists() {
        return Ok(None);
    }
    let newer = match &project_path {
        Some(p) if p.exists() => {
            let (Ok(ma), Ok(mp)) = (std::fs::metadata(&auto), std::fs::metadata(p)) else {
                return Ok(None);
            };
            matches!((ma.modified(), mp.modified()), (Ok(a), Ok(b)) if a > b)
        }
        _ => true, // proyecto nunca guardado: cualquier autosave cuenta
    };
    Ok(newer.then(|| auto.display().to_string()))
}

/// Elimina el autosave activo (tras guardar o descartar la recuperación).
#[tauri::command]
fn discard_recovery(app: tauri::AppHandle, state: State<AppState>) -> Res<()> {
    let data_dir = app.path().app_data_dir().ok();
    let project_path = state.path.lock().unwrap().clone();
    if let Some(auto) = autosave_path(project_path.as_deref(), data_dir.as_deref()) {
        let _ = std::fs::remove_file(auto);
    }
    // el autosave huérfano también, por si acaba de guardarse con nombre
    if let Some(d) = app.path().app_data_dir().ok() {
        let _ = std::fs::remove_file(d.join("recuperacion.uep.autosave"));
    }
    Ok(())
}

/// Carga una copia de recuperación conservando la ruta del proyecto original
/// (el siguiente Guardar escribe el .uep de verdad) y marcando cambios.
#[tauri::command]
fn recover_project(
    app: tauri::AppHandle,
    state: State<AppState>,
    autosave: String,
    original: Option<String>,
) -> Res<StateSnapshot> {
    let json = std::fs::read_to_string(&autosave).map_err(|e| e.to_string())?;
    let mut project = Project::from_json(&json).map_err(|e| e.to_string())?;
    let dir = original
        .as_deref()
        .and_then(|p| Path::new(p).parent().map(|d| d.to_path_buf()))
        .or_else(|| Path::new(&autosave).parent().map(|d| d.to_path_buf()));
    resolve_project_paths(&mut project, dir.as_deref());
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    for asset in &mut project.assets {
        if let Some(cache) = &cache_dir {
            let conform = conform_target(cache, &asset.content_hash);
            if conform.exists() {
                asset.audio_conform = Some(conform.to_string_lossy().into_owned());
            } else if !asset.offline && asset.probe.audio_channels > 0 {
                spawn_conform_job(&app, asset, cache);
            }
            let proxy = cache.join(format!("{}.proxy.mp4", asset.content_hash));
            if proxy.exists() {
                asset.proxy = Some(proxy.to_string_lossy().into_owned());
            }
        }
    }
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(project);
    store.dirty = true;
    *state.path.lock().unwrap() = original.map(PathBuf::from);
    Ok(snapshot(&store))
}

#[tauri::command]
fn save_project(state: State<AppState>, path: Option<String>) -> Res<String> {
    let mut store = state.store.lock().unwrap();
    let mut stored_path = state.path.lock().unwrap();
    let target = match path.map(PathBuf::from).or_else(|| stored_path.clone()) {
        Some(p) => p,
        None => return Err("no hay ruta de guardado; pasa una ruta".into()),
    };
    // PORTABILIDAD: serializar con rutas relativas al .uep cuando sea posible
    let portable = make_portable(&store.project, target.parent());
    let json = portable.to_json().map_err(|e| e.to_string())?;
    // escritura atómica: tmp + rename
    let tmp = target.with_extension("uep.tmp");
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    store.dirty = false;
    // el guardado real invalida las copias de recuperación
    let _ = std::fs::remove_file(target.with_extension("uep.autosave"));
    *stored_path = Some(target.clone());
    Ok(target.display().to_string())
}

#[tauri::command]
fn open_project(
    app: tauri::AppHandle,
    state: State<AppState>,
    path: String,
) -> Res<StateSnapshot> {
    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut project = Project::from_json(&json).map_err(|e| e.to_string())?;
    let issues = ue_core::validate::validate(&project);
    if !issues.is_empty() {
        return Err(format!("proyecto inválido: {}", issues.join("; ")));
    }
    // resolver rutas relativas contra la carpeta del .uep y marcar offline;
    // re-derivar cachés locales por hash y relanzar conformado si falta
    let dir = Path::new(&path).parent().map(|d| d.to_path_buf());
    resolve_project_paths(&mut project, dir.as_deref());
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    for asset in &mut project.assets {
        if let Some(cache) = &cache_dir {
            let conform = conform_target(cache, &asset.content_hash);
            if conform.exists() {
                asset.audio_conform = Some(conform.to_string_lossy().into_owned());
            } else if !asset.offline && asset.probe.audio_channels > 0 {
                spawn_conform_job(&app, asset, cache);
            }
            let proxy = cache.join(format!("{}.proxy.mp4", asset.content_hash));
            if proxy.exists() {
                asset.proxy = Some(proxy.to_string_lossy().into_owned());
            } else if !asset.offline {
                spawn_proxy_job(&app, asset, cache);
            }
        }
    }
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(project);
    *state.path.lock().unwrap() = Some(PathBuf::from(path));
    Ok(snapshot(&store))
}

/// Relocaliza un medio offline: nueva ruta, re-probe y conformado.
#[tauri::command]
fn relink_asset(
    app: tauri::AppHandle,
    state: State<AppState>,
    asset_id: String,
    new_path: String,
) -> Res<StateSnapshot> {
    let id = parse_id(&asset_id)?;
    let fresh = ue_media::import_file(Path::new(&new_path)).map_err(|e| e.to_string())?;
    let cache_dir = state.cache_dir.lock().unwrap().clone();
    let mut store = state.store.lock().unwrap();
    let asset = store
        .project
        .assets
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or("asset no encontrado")?;
    asset.path = new_path;
    asset.content_hash = fresh.content_hash;
    asset.probe = fresh.probe;
    asset.offline = false;
    asset.audio_conform = None;
    asset.proxy = None;
    let asset_snapshot = asset.clone();
    if let Some(cache) = &cache_dir {
        if asset_snapshot.probe.audio_channels > 0 {
            spawn_conform_job(&app, &asset_snapshot, cache);
        }
        spawn_proxy_job(&app, &asset_snapshot, cache);
    }
    store.version += 1;
    store.dirty = true;
    Ok(snapshot(&store))
}

#[tauri::command]
fn new_project(state: State<AppState>, name: String) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(Project::new(&name));
    *state.path.lock().unwrap() = None;
    Ok(snapshot(&store))
}

pub fn run() {
    let state = AppState::new_default();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(|app| {
            let state = app.state::<AppState>();
            if let Ok(dir) = app.path().app_cache_dir() {
                let _ = std::fs::create_dir_all(&dir);
                *state.cache_dir.lock().unwrap() = Some(dir);
            }
            if let Ok(dir) = app.path().app_config_dir() {
                let effects = dir.join("effects");
                let _ = std::fs::create_dir_all(&effects);
                *state.effects_dir.lock().unwrap() = Some(effects);
                let errors = reload_packs(&state);
                for e in errors {
                    eprintln!("[packs] manifest inválido: {e}");
                }
            }
            if let Ok(dir) = app.path().app_data_dir() {
                let models = dir.join("models");
                let _ = std::fs::create_dir_all(&models);
                *state.models_dir.lock().unwrap() = Some(models);
            }
            match mcp::start(app.handle().clone()) {
                Some(port) => {
                    *state.mcp_port.lock().unwrap() = Some(port);
                    let token = state.mcp_token.lock().unwrap().clone();
                    eprintln!(
                        "[mcp] escuchando en http://127.0.0.1:{port}/mcp (token: {token})"
                    );
                }
                None => eprintln!("[mcp] no se pudo abrir el puerto {}", mcp::MCP_PORT),
            }
            let data_dir = app.path().app_data_dir().ok();
            let handle = app.handle().clone();
            std::thread::spawn(move || autosave_loop(handle, data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_project_settings,
            split_clip,
            delete_clips,
            move_clip,
            trim_clip,
            cut_ranges,
            move_range,
            set_clip_speed,
            set_subtitles_props,
            unlink_clip,
            list_fonts,
            list_text_templates,
            save_text_template,
            undo,
            redo,
            set_clip_audio,
            set_clip_transform,
            import_media,
            add_clip,
            render_frame,
            get_effects_catalog,
            reload_effect_packs,
            mcp_status,
            set_clip_effects,
            set_clip_transition,
            set_track_prop,
            add_track,
            remove_track,
            rename_track,
            set_track_volume,
            add_text_clip,
            get_generators,
            add_generator_clip,
            set_clip_generator,
            set_clip_text,
            remove_silences,
            transcribe_asset,
            add_subtitles_clip,
            generate_vertical,
            set_active_sequence,
            add_avatar_clip,
            export_video,
            cancel_export,
            playback_play,
            playback_pause,
            playback_seek,
            playback_position,
            playback_set_rate,
            get_audio_peaks,
            ensure_thumbs,
            get_thumb_strip,
            playback_frame,
            save_project,
            check_recovery,
            recover_project,
            discard_recovery,
            open_project,
            relink_asset,
            new_project,
        ])
        .run(tauri::generate_context!())
        .expect("error al arrancar UberEditor");
}
