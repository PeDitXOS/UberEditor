//! Servidor MCP embebido (PLAN §7.A, v0).
//!
//! Implementación directa del protocolo MCP (JSON-RPC 2.0 sobre HTTP
//! streamable, respuesta application/json) en 127.0.0.1:4599/mcp, sin SDK:
//! initialize, tools/list y tools/call. Solo loopback. El dispatcher
//! (`handle_rpc`) es una función pura sobre AppState → testeable sin HTTP.
//!
//! Conexión desde Claude Code:
//!   claude mcp add --transport http ubereditor http://127.0.0.1:4599/mcp

use std::sync::atomic::Ordering;

use serde_json::{json, Value};
use ue_core::model::TransitionRef;
use ue_core::ops::InsertMode;

use crate::AppState;

pub const MCP_PORT: u16 = 4599;

// ---------------------------------------------------------------------------
// Herramientas
// ---------------------------------------------------------------------------

fn tool_defs() -> Value {
    json!([
        {
            "name": "get_project_summary",
            "description": "Resumen del proyecto abierto: nombre, duración, pistas, clips, medios y estado de guardado.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_timeline",
            "description": "Timeline completo de la secuencia activa: pistas con sus clips (ids, tiempos en µs, payloads, efectos, transiciones).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_media_pool",
            "description": "Medios importados: id, ruta, tipo, duración y metadatos técnicos.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_effects_catalog",
            "description": "Catálogo de efectos disponibles (packs core + usuario) con sus parámetros.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "split_clip",
            "description": "Divide un clip en el tiempo dado del timeline (µs). Devuelve los ids resultantes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "clip_id": { "type": "string" },
                    "t_us": { "type": "integer", "description": "tiempo del timeline en microsegundos" }
                },
                "required": ["clip_id", "t_us"]
            }
        },
        {
            "name": "delete_clips",
            "description": "Elimina clips por id. Con ripple=true cierra los huecos.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" } },
                    "ripple": { "type": "boolean", "default": false }
                },
                "required": ["ids"]
            }
        },
        {
            "name": "add_clip",
            "description": "Añade un clip de un medio del pool al timeline (en at_us o al final de la pista compatible).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset_id": { "type": "string" },
                    "at_us": { "type": "integer", "default": 0 }
                },
                "required": ["asset_id"]
            }
        },
        {
            "name": "set_clip_transition",
            "description": "Pone (o quita, con duration_us=0) un fundido cruzado de entrada en un clip.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "clip_id": { "type": "string" },
                    "duration_us": { "type": "integer", "description": "0 = quitar transición" }
                },
                "required": ["clip_id", "duration_us"]
            }
        },
        {
            "name": "get_transcript",
            "description": "Transcripción word-level de un asset (palabras con timestamps en µs y frases). Vacío si aún no se ha transcrito.",
            "inputSchema": {
                "type": "object",
                "properties": { "asset_id": { "type": "string" } },
                "required": ["asset_id"]
            }
        },
        {
            "name": "remove_silences",
            "description": "Detecta y elimina los silencios de un clip (corta y cierra huecos en todas las pistas; una sola entrada de undo). El clip debe tener audio conformado.",
            "inputSchema": {
                "type": "object",
                "properties": { "clip_id": { "type": "string" } },
                "required": ["clip_id"]
            }
        },
        {
            "name": "move_range",
            "description": "Mueve el rango [from_us, to_us) del timeline a dest_us (reordena material en todas las pistas; 1 undo). Útil para reordenar frases.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_us": { "type": "integer" },
                    "to_us": { "type": "integer" },
                    "dest_us": { "type": "integer" }
                },
                "required": ["from_us", "to_us", "dest_us"]
            }
        },
        {
            "name": "generate_vertical",
            "description": "Genera una secuencia vertical 1080x1920 (fondo desenfocado + video centrado) a partir de la secuencia activa y la activa. Deshacible.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "undo",
            "description": "Deshace la última edición.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "redo",
            "description": "Rehace la última edición deshecha.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

fn text_result(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": v.to_string() }] })
}

fn tool_error(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

fn call_tool(state: &AppState, name: &str, args: &Value) -> Value {
    let parse_id = |key: &str| -> Result<ue_core::model::Id, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("falta {key}"))?
            .parse()
            .map_err(|e| format!("{key} inválido: {e}"))
    };

    match name {
        "get_project_summary" => {
            let store = state.store.lock().unwrap();
            let p = &store.project;
            let seq = p.sequence(p.active_sequence);
            text_result(json!({
                "name": p.name,
                "dirty": store.dirty,
                "assets": p.assets.len(),
                "sequence": seq.map(|s| json!({
                    "name": s.name,
                    "resolution": s.resolution,
                    "fps": s.fps,
                    "duration_us": s.duration_us(),
                    "tracks": s.tracks.iter().map(|t| json!({
                        "id": t.id.to_string(),
                        "name": t.name,
                        "kind": t.kind,
                        "clips": t.clips.len(),
                    })).collect::<Vec<_>>(),
                })),
                "undo_history": store.undo_labels(),
            }))
        }
        "get_timeline" => {
            let store = state.store.lock().unwrap();
            let seq = store.project.sequence(store.project.active_sequence);
            text_result(serde_json::to_value(seq).unwrap_or(Value::Null))
        }
        "get_media_pool" => {
            let store = state.store.lock().unwrap();
            text_result(serde_json::to_value(&store.project.assets).unwrap_or(Value::Null))
        }
        "get_effects_catalog" => {
            text_result(ue_render::catalog_json(&state.registry.lock().unwrap()))
        }
        "split_clip" => {
            let clip_id = match parse_id("clip_id") {
                Ok(v) => v,
                Err(e) => return tool_error(&e),
            };
            let Some(t_us) = args.get("t_us").and_then(|v| v.as_i64()) else {
                return tool_error("falta t_us");
            };
            let mut store = state.store.lock().unwrap();
            match store.split_clip(clip_id, t_us) {
                Ok((l, r)) => text_result(json!({ "left": l.to_string(), "right": r.to_string() })),
                Err(e) => tool_error(&e.to_string()),
            }
        }
        "delete_clips" => {
            let Some(ids) = args.get("ids").and_then(|v| v.as_array()) else {
                return tool_error("falta ids");
            };
            let parsed: Result<Vec<ue_core::model::Id>, _> = ids
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.parse::<ue_core::model::Id>())
                .collect();
            let ripple = args.get("ripple").and_then(|v| v.as_bool()).unwrap_or(false);
            match parsed {
                Ok(ids) => {
                    let mut store = state.store.lock().unwrap();
                    match store.delete_clips(&ids, ripple) {
                        Ok(()) => text_result(json!({ "deleted": ids.len() })),
                        Err(e) => tool_error(&e.to_string()),
                    }
                }
                Err(e) => tool_error(&format!("id inválido: {e}")),
            }
        }
        "add_clip" => {
            let asset_id = match parse_id("asset_id") {
                Ok(v) => v,
                Err(e) => return tool_error(&e),
            };
            let at_us = args.get("at_us").and_then(|v| v.as_i64()).unwrap_or(0);
            match add_clip_inner(state, asset_id, at_us) {
                Ok(clip_id) => text_result(json!({ "clip_id": clip_id })),
                Err(e) => tool_error(&e),
            }
        }
        "set_clip_transition" => {
            let clip_id = match parse_id("clip_id") {
                Ok(v) => v,
                Err(e) => return tool_error(&e),
            };
            let dur = args.get("duration_us").and_then(|v| v.as_i64()).unwrap_or(0);
            let transition = (dur > 0).then(|| TransitionRef {
                effect_id: "core.crossfade".into(),
                duration: dur,
                params: Default::default(),
            });
            let mut store = state.store.lock().unwrap();
            match store.dispatch(
                "[MCP] Editar transición",
                vec![ue_core::Action::SetClipTransition { clip_id, transition }],
            ) {
                Ok(()) => text_result(json!({ "ok": true })),
                Err(e) => tool_error(&e.to_string()),
            }
        }
        "get_transcript" => {
            let asset_id = match parse_id("asset_id") {
                Ok(v) => v,
                Err(e) => return tool_error(&e),
            };
            let store = state.store.lock().unwrap();
            match store.project.transcripts.iter().find(|t| t.asset_id == asset_id) {
                Some(doc) => text_result(serde_json::to_value(doc).unwrap_or(Value::Null)),
                None => tool_error("el asset no tiene transcripción todavía (usa transcribe en la UI)"),
            }
        }
        "remove_silences" => {
            let clip_id = match parse_id("clip_id") {
                Ok(v) => v,
                Err(e) => return tool_error(&e),
            };
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("delete").to_string();
            match remove_silences_inner(state, clip_id, &mode) {
                Ok((n, us)) => text_result(json!({ "removed": n, "removed_us": us })),
                Err(e) => tool_error(&e),
            }
        }
        "move_range" => {
            let get = |k: &str| args.get(k).and_then(|v| v.as_i64());
            let (Some(f), Some(t), Some(d)) = (get("from_us"), get("to_us"), get("dest_us"))
            else {
                return tool_error("faltan from_us/to_us/dest_us");
            };
            let mut store = state.store.lock().unwrap();
            let seq_id = store.project.active_sequence;
            match store.move_range(seq_id, f, t, d) {
                Ok(()) => text_result(json!({ "ok": true })),
                Err(e) => tool_error(&e.to_string()),
            }
        }
        "generate_vertical" => match generate_vertical_inner(state) {
            Ok(id) => text_result(json!({ "sequence_id": id })),
            Err(e) => tool_error(&e),
        },
        "undo" => {
            let mut store = state.store.lock().unwrap();
            match store.undo() {
                Ok(label) => text_result(json!({ "undone": label })),
                Err(e) => tool_error(&e.to_string()),
            }
        }
        "redo" => {
            let mut store = state.store.lock().unwrap();
            match store.redo() {
                Ok(label) => text_result(json!({ "redone": label })),
                Err(e) => tool_error(&e.to_string()),
            }
        }
        _ => tool_error(&format!("herramienta desconocida: {name}")),
    }
}

fn generate_vertical_inner(state: &AppState) -> Result<String, String> {
    // mismo flujo que el comando de la UI
    crate::generate_vertical_impl(state)
}

fn remove_silences_inner(state: &AppState, clip_id: ue_core::model::Id, mode: &str) -> Result<(usize, i64), String> {
    let mut store = state.store.lock().unwrap();
    let clip = store.project.clip(clip_id).ok_or("clip no encontrado")?.clone();
    let ue_core::model::ClipPayload::Media { asset_id, src_in, src_out } = clip.payload else {
        return Err("el clip no es de media".into());
    };
    let asset = store.project.asset(asset_id).ok_or("asset no encontrado")?;
    let conform = asset.audio_conform.clone().ok_or("audio sin conformar todavía")?;
    let wav = ue_audio::wav::WavMap::open(std::path::Path::new(&conform))
        .map_err(|e| e.to_string())?;
    let params = ue_ai::silence::SilenceParams::default();
    let ranges =
        ue_ai::silence::clip_silences_on_timeline(&wav, clip.start, src_in, src_out, &params);
    if ranges.is_empty() {
        return Ok((0, 0));
    }
    let removed_us: i64 = ranges.iter().map(|(s, e)| e - s).sum();
    let seq_id = store.project.active_sequence;
    if mode == "speedup" {
        store.speedup_ranges(seq_id, &ranges, 4.0).map_err(|e| e.to_string())?;
    } else {
        store.cut_ranges(seq_id, &ranges, true).map_err(|e| e.to_string())?;
    }
    Ok((ranges.len(), removed_us))
}

/// Igual que el comando add_clip de la UI (duplicado consciente y pequeño).
fn add_clip_inner(state: &AppState, asset_id: ue_core::model::Id, at_us: i64) -> Result<String, String> {
    use ue_core::model::{Clip, MediaKind, TrackKind};
    let mut store = state.store.lock().unwrap();
    let asset = store
        .project
        .asset(asset_id)
        .ok_or_else(|| format!("asset {asset_id} no existe"))?
        .clone();
    let duration = ue_media::default_clip_duration(&asset);
    if duration <= 0 {
        return Err("el archivo no tiene duración utilizable".into());
    }
    let want = if asset.kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video };
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("sin secuencia activa")?;
    let track = seq
        .tracks
        .iter()
        .find(|t| t.kind == want && !t.locked)
        .ok_or("no hay pista compatible")?;
    let track_id = track.id;
    let at = at_us.max(0);
    let start = if track.collides(at, duration, None) {
        track.clips.iter().map(|c| c.end()).max().unwrap_or(0)
    } else {
        at
    };
    let clip = Clip::new_media(asset.id, 0, duration, start);
    let clip_id = clip.id;
    store.insert_clip(track_id, clip, InsertMode::Strict).map_err(|e| e.to_string())?;
    Ok(clip_id.to_string())
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

/// Procesa un mensaje JSON-RPC. `None` = notificación sin respuesta.
pub fn handle_rpc(state: &AppState, req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    let id = req.get("id").cloned();
    // notificaciones (sin id) no llevan respuesta
    if id.is_none() || id == Some(Value::Null) {
        return None;
    }
    let id = id.unwrap();

    let result = match method {
        "initialize" => {
            let requested = req
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2025-06-18");
            json!({
                "protocolVersion": requested,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "ubereditor",
                    "title": "UberEditor",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": "Editor de video UberEditor. Lee el estado con get_project_summary/get_timeline; edita con split_clip/delete_clips/add_clip. Toda edición es deshacible (undo)."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_defs() }),
        "tools/call" => {
            let name = req.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("");
            let empty = json!({});
            let args = req.pointer("/params/arguments").unwrap_or(&empty);
            call_tool(state, name, args)
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("método no soportado: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Arranca el servidor en un hilo. Devuelve el puerto si pudo escuchar.
pub fn start(app: tauri::AppHandle) -> Option<u16> {
    let server = tiny_http::Server::http(("127.0.0.1", MCP_PORT)).ok()?;
    std::thread::Builder::new()
        .name("ue-mcp".into())
        .spawn(move || {
            use tauri::Manager;
            for mut request in server.incoming_requests() {
                let state = app.state::<AppState>();
                if state.mcp_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let response = match serde_json::from_str::<Value>(&body) {
                    Ok(msg) => handle_rpc(&state, &msg),
                    Err(_) => Some(json!({
                        "jsonrpc": "2.0", "id": Value::Null,
                        "error": { "code": -32700, "message": "JSON inválido" }
                    })),
                };
                let (status, text) = match response {
                    Some(v) => (200, v.to_string()),
                    None => (202, String::new()),
                };
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap();
                let _ = request.respond(
                    tiny_http::Response::from_string(text)
                        .with_status_code(status)
                        .with_header(header),
                );
            }
        })
        .ok()?;
    Some(MCP_PORT)
}
