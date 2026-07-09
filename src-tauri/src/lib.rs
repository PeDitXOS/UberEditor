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
    pub models_dir: Mutex<Option<PathBuf>>,
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
            models_dir: Mutex::new(None),
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
        let vf = ue_render::clip_vf(&reg, &r.effects, &r.transform, canvas);

        // ¿sirve la sesión actual? (mismo archivo, misma cadena de efectos,
        // posición alcanzable hacia delante)
        let reusable = session.as_ref().is_some_and(|s| {
            s.asset_path == path
                && session_vf == vf
                && src_t >= s.next_src_us() - 1_000_000 / PLAYBACK_FPS as i64
                && src_t <= s.next_src_us() + 1_500_000
        });
        if !reusable {
            session =
                MjpegSession::open(&path, src_t, PLAYBACK_MAX_W, PLAYBACK_FPS, vf.as_deref()).ok();
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
                    if asset.probe.audio_channels > 0 {
                        if let Some(cache) = &cache_dir {
                            spawn_conform_job(&app, &asset, cache);
                        }
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
fn playback_position(state: State<AppState>) -> Res<(TimeUs, bool)> {
    let _ = sync_player(&state); // barato si no cambió la versión
    let guard = state.player.lock().unwrap();
    match guard.as_ref() {
        Some(p) => Ok((p.position_us(), p.is_playing())),
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
    let mut vf = ue_media::frame::resolve_top_video(&project, seq_id, t_us)
        .and_then(|r| ue_render::clip_vf(&reg, &r.effects, &r.transform, canvas));
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
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia")?;
    let track = seq
        .tracks
        .iter()
        .rev()
        .find(|t| t.kind == TrackKind::Video && !t.locked)
        .ok_or("no hay pista de video desbloqueada")?;
    let track_id = track.id;
    if track.collides(media.start, media.duration, None) {
        return Err("la pista superior está ocupada en ese rango (usa otra pista)".into());
    }
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

    // 3. clip Avatar en la pista superior
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia")?;
    let track = seq
        .tracks
        .iter()
        .rev()
        .find(|t| t.kind == TrackKind::Video && !t.locked && !t.collides(media.start, media.duration, None))
        .ok_or("no hay pista de video libre en ese rango (añade una pista)")?;
    let track_id = track.id;
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
    let model_name = model.unwrap_or_else(|| "base".into());
    std::thread::spawn(move || {
        let result = ue_whisper::ensure_model(&models_dir, &model_name)
            .and_then(|m| ue_whisper::transcribe(&conform, &m, None, id));
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
fn remove_silences(
    state: State<AppState>,
    clip_id: String,
    mode: Option<String>,
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
    let params = ue_ai::silence::SilenceParams::default();
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

/// Añade un clip de texto (título) en la pista de video superior.
#[tauri::command]
fn add_text_clip(state: State<AppState>, content: String, at_us: TimeUs) -> Res<StateSnapshot> {
    let mut store = state.store.lock().unwrap();
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia activa")?;
    let track = seq
        .tracks
        .iter()
        .rev()
        .find(|t| t.kind == TrackKind::Video && !t.locked)
        .ok_or("no hay pista de video desbloqueada")?;
    let track_id = track.id;
    let duration = 4_000_000;
    let at = at_us.max(0);
    let start = if track.collides(at, duration, None) {
        track.clips.iter().map(|c| c.end()).max().unwrap_or(0)
    } else {
        at
    };
    let clip = Clip::new_text(&content, start, duration);
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
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

/// Puerto del servidor MCP embebido (None si no pudo arrancar).
#[tauri::command]
fn mcp_status(state: State<AppState>) -> Option<u16> {
    *state.mcp_port.lock().unwrap()
}

#[tauri::command]
fn cancel_export(state: State<AppState>) -> Res<()> {
    state.export_cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Exporta la secuencia activa a MP4 (bloqueante en un hilo aparte).
/// Emite eventos `export-progress` (0..1); `cancel_export` la aborta.
#[tauri::command]
async fn export_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    max_height: Option<u32>,
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
    let settings = ue_export::ExportSettings { max_height, extra_packs, ..Default::default() };
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

#[tauri::command]
fn save_project(state: State<AppState>, path: Option<String>) -> Res<String> {
    let mut store = state.store.lock().unwrap();
    let mut stored_path = state.path.lock().unwrap();
    let target = match path.map(PathBuf::from).or_else(|| stored_path.clone()) {
        Some(p) => p,
        None => return Err("no hay ruta de guardado; pasa una ruta".into()),
    };
    let json = store.project.to_json().map_err(|e| e.to_string())?;
    // escritura atómica: tmp + rename
    let tmp = target.with_extension("uep.tmp");
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    store.dirty = false;
    *stored_path = Some(target.clone());
    Ok(target.display().to_string())
}

#[tauri::command]
fn open_project(state: State<AppState>, path: String) -> Res<StateSnapshot> {
    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let project = Project::from_json(&json).map_err(|e| e.to_string())?;
    let issues = ue_core::validate::validate(&project);
    if !issues.is_empty() {
        return Err(format!("proyecto inválido: {}", issues.join("; ")));
    }
    let mut store = state.store.lock().unwrap();
    *store = ProjectStore::new(project);
    *state.path.lock().unwrap() = Some(PathBuf::from(path));
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
                    eprintln!("[mcp] escuchando en http://127.0.0.1:{port}/mcp");
                }
                None => eprintln!("[mcp] no se pudo abrir el puerto {}", mcp::MCP_PORT),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            split_clip,
            delete_clips,
            move_clip,
            trim_clip,
            cut_ranges,
            move_range,
            set_clip_speed,
            set_subtitles_props,
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
            add_text_clip,
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
            playback_frame,
            save_project,
            open_project,
            new_project,
        ])
        .run(tauri::generate_context!())
        .expect("error al arrancar UberEditor");
}
