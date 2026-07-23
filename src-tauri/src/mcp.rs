//! Embedded MCP server: the full agentic surface of the editor.
//!
//! Direct implementation of the MCP protocol (JSON-RPC 2.0 over streamable
//! HTTP, `application/json` response) on 127.0.0.1:4599/mcp, no SDK:
//! `initialize`, `tools/list` and `tools/call`. Loopback only, Bearer token.
//! The dispatcher (`handle_rpc`) is a pure function over `AppState` →
//! testable without HTTP (see `tests/mcp_tests.rs`).
//!
//! Connecting from Claude Code (the token is shown in the app's MCP pill):
//!   claude mcp add --transport http ubereditor http://127.0.0.1:4599/mcp \
//!     --header "Authorization: Bearer <token>"
//!
//! # Design rules for the tools
//!
//! 1. **One tool call = one undo entry.** Tools that change several things
//!    (`set_clip_properties`) batch their actions into a single `dispatch`, so
//!    a single `undo` reverts the whole call. Never `dispatch` twice.
//! 2. **Every mutation goes through `ProjectStore::dispatch`**, which validates
//!    the project invariants and rolls back atomically on failure. A tool can
//!    therefore never leave the project in a broken state.
//! 3. **Times are always µs (i64) on the timeline**, never seconds or frames.
//! 4. **Errors are tool errors, not protocol errors**: `isError: true` with a
//!    human-readable message the agent can act on.
//! 5. **The MCP path reuses the UI's implementation** (`crate::*_impl`) so an
//!    agent and a human hit the exact same code — the preview/export parity
//!    rule applies to agents too.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};
use ue_core::model::{
    AudioProps, Id, SubtitleMode, TextStyle, TransitionRef, Transform2D,
};
use ue_core::ops::InsertMode;

use crate::AppState;

pub const MCP_PORT: u16 = 4599;

/// Read by the agent right after `initialize`: the map of the territory.
const INSTRUCTIONS: &str = "\
UberEditor — a video editor you can drive end to end.

MODEL
  Project → sequences → tracks (video/audio) → clips. A clip's payload is
  Media | Text | Subtitles | Generator | Avatar. Transcripts live on the
  project and are referenced by asset id.

UNITS
  Every time is an INTEGER of MICROSECONDS (µs) on the TIMELINE, never
  seconds and never frames. 1 s = 1_000_000. Fractional values are rejected.
  Ids are ULID strings; get them from get_timeline / get_media_pool.

HISTORY
  Every mutating tool is one undo entry: `undo` reverts exactly one tool call.
  A call that would break a project invariant fails and changes nothing.

TYPICAL FLOW
  1. get_project_summary            — what is open, what is in it
  2. get_media_pool / get_timeline  — ids to work with
  3. import_media                   — bring files in (conform runs in background)
  4. transcribe_asset               — needed by subtitles, silences, avatar
  5. edit: split_clip, delete_clips, move_clip, trim_clip, cut_ranges,
     set_clip_properties, add_text_clip, add_subtitles_clip, remove_silences
  6. export_video                   — one file, optionally several `ranges`
  7. save_project

GOTCHAS
  • After import_media the audio conform runs in the BACKGROUND. Tools that
    need audio (transcribe_asset, remove_silences, generate_avatar_video) fail
    with 'audio is still being prepared' until it lands. Retry after a moment.
  • transcribe_asset and generate_avatar_video BLOCK for minutes and download
    models on first use.
  • remove_silences/cut_ranges/move_range act on ALL tracks at once.
  • The preview and the export share the same ffmpeg chain: what you render
    with debug_render_frame is what export_video writes.";

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// How a tool behaves, for MCP clients that surface it (and for the agent's
/// own planning): read-only tools are free to call, destructive ones are not.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Reads state, changes nothing.
    Read,
    /// Mutates the project; one `undo` reverts it.
    Edit,
    /// Cannot be undone with `undo` (writes files, replaces the project…).
    Destructive,
}

fn tool(name: &str, description: &str, props: Value, required: &[&str], kind: Kind) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": kind == Kind::Read,
            "destructiveHint": kind == Kind::Destructive,
            "idempotentHint": kind == Kind::Read,
        }
    })
}

fn int(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}
fn str_(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}
fn bool_(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}
fn num(desc: &str) -> Value {
    json!({ "type": "number", "description": desc })
}

/// A number, or a keyframe curve. Mirrors `ue_core::keyframe::Param`.
fn param(desc: &str) -> Value {
    json!({
        "description": format!("{desc} — a plain number, or an animated curve \
            {{\"keys\":[{{\"t\":<µs from the clip start>,\"value\":<n>,\
            \"interp\":{{\"kind\":\"linear\"|\"hold\"|\"smooth\"}}}}, …]}}. \
            Curve keys must be sorted by t (duplicates collapse to the last)."),
        "anyOf": [
            { "type": "number" },
            { "type": "object", "properties": { "keys": { "type": "array" } }, "required": ["keys"] }
        ]
    })
}

fn tool_defs() -> Value {
    let clip_id = str_("clip id (from get_timeline)");

    json!([
        // ---------------------------------------------------------------- read
        tool(
            "get_project_summary",
            "START HERE. Name and save path of the open project, its sequences \
             (id, resolution, fps, duration, tracks), how many assets and \
             transcripts it has, the avatar setups, and the undo history. \
             Cheap; call it whenever you need to re-orient.",
            json!({}), &[], Kind::Read,
        ),
        tool(
            "get_timeline",
            "The full sequence as JSON: every track with every clip (id, start, \
             duration, speed, payload, transform, audio, effects, transition). \
             This is where clip ids come from. Defaults to the active sequence.",
            json!({ "sequence_id": str_("defaults to the active sequence") }),
            &[], Kind::Read,
        ),
        tool(
            "get_media_pool",
            "Imported media: id, path, kind (video/audio/image), duration, probe \
             metadata, and whether the audio conform / proxy / transcript are \
             ready. An asset with `audio_conform: null` is not ready for \
             transcribe_asset or remove_silences yet.",
            json!({}), &[], Kind::Read,
        ),
        tool(
            "get_transcript",
            "The transcript of an asset, at the granularity you ask for. A full \
             word-level dump is HUGE (100k+ chars) — default to 'phrases' \
             (caption-sized chunks with timestamps: the right unit for choosing \
             cuts) and narrow with start_us/end_us. Use 'words' only for a small \
             window. Identify the transcript by asset_id or transcript_id.",
            json!({
                "asset_id": str_("asset id (from get_media_pool)"),
                "transcript_id": str_("or the transcript id directly"),
                "granularity": {
                    "type": "string",
                    "enum": ["text", "segments", "phrases", "sentences", "words"],
                    "description": "text = plain words, no timing; segments = Whisper's own \
                        coarse chunks (carry emotion); phrases/sentences = caption-sized chunks \
                        with timestamps (default); words = every word with its µs span"
                },
                "start_us": int("only include material overlapping [start_us, end_us)"),
                "end_us": int("window end in µs"),
                "max_chars": int("phrases: target characters per phrase (default 48)"),
            }),
            &[], Kind::Read,
        ),
        tool(
            "find_words",
            "Search the transcript for a word or short phrase and get each hit's \
             timestamp plus a few neighbour words for context. This is how you \
             locate a cut: e.g. find 'in conclusion' → its start_us, then \
             cut_ranges or move_range around it. Matching ignores case and \
             punctuation.",
            json!({
                "query": str_("a word or a short exact phrase to find"),
                "asset_id": str_("asset id (or transcript_id)"),
                "transcript_id": str_("or the transcript id directly"),
                "context": int("neighbour words to include on each side (default 4, max 20)"),
            }),
            &["query"], Kind::Read,
        ),
        tool(
            "get_catalog",
            "Everything you can reference by id: video effects and their \
             parameters, generators (solid, gradient…), installed font families, \
             the avatar setups saved in the project, subtitle modes and \
             transition ids. Read this before set_clip_properties or \
             add_generator_clip.",
            json!({}), &[], Kind::Read,
        ),

        // --------------------------------------------------------------- media
        tool(
            "import_media",
            "Imports files into the media pool and returns their asset ids. \
             Importing the same content twice is idempotent (matched by content \
             hash) and returns the existing id. Does NOT put anything on the \
             timeline — call add_clip next. The audio conform and the proxy are \
             built in the BACKGROUND, so transcribe_asset / remove_silences may \
             need a few seconds before they work. Not undoable.",
            json!({ "paths": {
                "type": "array", "items": { "type": "string" },
                "description": "absolute paths to video/audio/image files"
            }}),
            &["paths"], Kind::Destructive,
        ),
        tool(
            "transcribe_asset",
            "Transcribes an asset's audio with Whisper, word by word, and stores \
             the transcript on the project. Required by add_subtitles_clip, \
             replace_words and generate_avatar_video. RUNS IN THE BACKGROUND: \
             returns a `job_id` immediately (transcription takes minutes and \
             downloads the ggml model on first use); poll get_job_status until \
             it's done — the `result` then has {transcript_id, words}. \
             Re-transcribing replaces the previous transcript. Not undoable.",
            json!({
                "asset_id": str_("asset id; must have audio and a ready conform"),
                "model": str_("ggml model name, e.g. 'base', 'small', 'medium' (default: the project setting)"),
            }),
            &["asset_id"], Kind::Destructive,
        ),
        tool(
            "relink_asset",
            "Points an asset at a new file. This is the fix for media flagged \
             `offline` after opening a project whose footage moved. Re-probes the \
             file and rebuilds its proxy and audio conform in the background. \
             Not undoable.",
            json!({
                "asset_id": str_("asset id (from get_media_pool)"),
                "new_path": str_("absolute path to the file"),
            }),
            &["asset_id", "new_path"], Kind::Destructive,
        ),
        tool(
            "set_project_settings",
            "Transcription defaults used by transcribe_asset when it is not given \
             a model. Set the language BEFORE transcribing: 'auto' detects it, \
             otherwise pass a code like 'es' or 'en'. Not undoable.",
            json!({
                "whisper_language": str_("'auto' or a language code ('es', 'en', …)"),
                "whisper_model": str_("default ggml model, e.g. 'base', 'small', 'medium'"),
            }),
            &[], Kind::Destructive,
        ),

        // ------------------------------------------------------------ timeline
        tool(
            "add_clip",
            "Puts a media asset on the timeline and returns the new clip id. \
             Picks a compatible unlocked track (audio assets → audio track) \
             unless `track_id` says otherwise. If `at_us` is occupied, the clip \
             lands after the last clip of that track instead of overlapping.",
            json!({
                "asset_id": str_("asset id (from get_media_pool)"),
                "at_us": int("timeline position in µs (default 0)"),
                "track_id": str_("force a specific track (default: first compatible unlocked one)"),
            }),
            &["asset_id"], Kind::Edit,
        ),
        tool(
            "add_text_clip",
            "Adds a title (text) clip on a free video track, creating one if \
             needed. Returns the clip id. Style it afterwards with \
             set_clip_content.",
            json!({
                "content": str_("the text to show"),
                "at_us": int("timeline position in µs (default 0)"),
                "duration_us": int("length in µs (default 4_000_000 = 4 s)"),
            }),
            &["content"], Kind::Edit,
        ),
        tool(
            "add_generator_clip",
            "Adds a synthetic clip (solid colour, gradient…) on a free video \
             track. See get_catalog → generators for the ids and their params. \
             Returns the clip id.",
            json!({
                "generator_id": str_("e.g. 'core.solid', 'core.gradient'"),
                "at_us": int("timeline position in µs (default 0)"),
                "duration_us": int("length in µs (default 4_000_000 = 4 s)"),
            }),
            &["generator_id"], Kind::Edit,
        ),
        tool(
            "add_subtitles_clip",
            "Adds an auto-subtitles clip spanning a transcribed media clip. The \
             captions are built from the WORD timestamps (phrases are chunked by \
             gaps and length), so they follow any later cut. The media clip's \
             asset must be transcribed first. Returns the subtitles clip id; \
             restyle it with set_clip_content.",
            json!({ "clip_id": str_("id of a MEDIA clip whose asset has a transcript") }),
            &["clip_id"], Kind::Edit,
        ),
        tool(
            "split_clip",
            "Cuts a clip in two at a timeline time. Keyframes and effects are \
             split with it. Returns the two resulting clip ids.",
            json!({ "clip_id": clip_id.clone(), "t_us": int("timeline time in µs, strictly inside the clip") }),
            &["clip_id", "t_us"], Kind::Edit,
        ),
        tool(
            "delete_clips",
            "Deletes clips by id. With `ripple: true` the following clips slide \
             left to close the gap (on those tracks).",
            json!({
                "ids": { "type": "array", "items": { "type": "string" }, "description": "clip ids" },
                "ripple": bool_("close the gap afterwards (default false)"),
            }),
            &["ids"], Kind::Edit,
        ),
        tool(
            "move_clip",
            "Moves a clip to another track and/or timeline position. Fails on \
             collision unless `overwrite` is true, in which case whatever it \
             lands on is trimmed away.",
            json!({
                "clip_id": clip_id.clone(),
                "to_track": str_("destination track id (may be the current one)"),
                "to_start_us": int("new start on the timeline, in µs"),
                "overwrite": bool_("trim whatever is underneath (default false)"),
            }),
            &["clip_id", "to_track", "to_start_us"], Kind::Edit,
        ),
        tool(
            "trim_clip",
            "Drags one edge of a clip. `left: true` moves the in point (the \
             start), `left: false` the out point (the end). Media clips consume \
             source material accordingly; you cannot trim past the source.",
            json!({
                "clip_id": clip_id.clone(),
                "left": bool_("true = the clip's start edge, false = its end edge"),
                "new_edge_us": int("the new timeline position of that edge, in µs"),
            }),
            &["clip_id", "left", "new_edge_us"], Kind::Edit,
        ),
        tool(
            "unlink_clip",
            "Breaks the video↔audio link of a clip's group, so the two halves \
             can be edited separately. One undo re-links them.",
            json!({ "clip_id": clip_id.clone() }),
            &["clip_id"], Kind::Edit,
        ),
        tool(
            "cut_ranges",
            "Deletes one or more timeline ranges across ALL tracks at once, \
             closing the gaps by default (ripple). The workhorse for 'remove \
             these sentences': take the word timestamps from get_transcript, \
             pass the ranges here. One undo entry for the whole call.",
            json!({
                "ranges": {
                    "type": "array",
                    "items": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                    "description": "[[start_us, end_us], …]; overlapping ranges are merged"
                },
                "ripple": bool_("close the gaps (default true)"),
                "sequence_id": str_("defaults to the active sequence"),
            }),
            &["ranges"], Kind::Edit,
        ),
        tool(
            "move_range",
            "Lifts the timeline range [from_us, to_us) out (across all tracks) \
             and re-inserts it at dest_us, closing the hole it left. Use it to \
             reorder sentences. One undo entry.",
            json!({
                "from_us": int("range start, µs"),
                "to_us": int("range end (exclusive), µs"),
                "dest_us": int("where to re-insert it, in the timeline AFTER the range was lifted"),
                "sequence_id": str_("defaults to the active sequence"),
            }),
            &["from_us", "to_us", "dest_us"], Kind::Edit,
        ),

        // ------------------------------------------------------ clip properties
        tool(
            "set_clip_properties",
            "Edits any combination of a clip's transform, audio, effects, \
             transition and speed in ONE undoable call. Omitted fields are left \
             alone; `transform` and `audio` are PATCHES (only the keys you send \
             change), while `effects` REPLACES the whole list. Every numeric \
             field also accepts a keyframe curve — that is how you animate.",
            json!({
                "clip_id": clip_id.clone(),
                "transform": {
                    "type": "object",
                    "additionalProperties": false,
                    "description": "Partial patch of the 2D transform. Position is in \
                        pixels of the sequence canvas from its centre; scale is a \
                        multiplier (1 = fit); rotation is degrees; opacity 0..1; \
                        crop is a 0..1 fraction of each edge.",
                    "properties": {
                        "position_x": param("horizontal offset in px"),
                        "position_y": param("vertical offset in px"),
                        "scale_x": param("horizontal scale, 1 = original"),
                        "scale_y": param("vertical scale, 1 = original"),
                        "rotation": param("degrees, clockwise"),
                        "opacity": param("0 = transparent, 1 = opaque"),
                        "crop_left": param("fraction 0..1 cropped from the left"),
                        "crop_top": param("fraction 0..1 cropped from the top"),
                        "crop_right": param("fraction 0..1 cropped from the right"),
                        "crop_bottom": param("fraction 0..1 cropped from the bottom"),
                        "flip_h": bool_("mirror horizontally"),
                        "flip_v": bool_("mirror vertically"),
                    }
                },
                "audio": {
                    "type": "object",
                    "additionalProperties": false,
                    "description": "Partial patch of the clip's audio.",
                    "properties": {
                        "gain_db": param("volume in dB, 0 = unchanged"),
                        "pan": param("-1 = left, 0 = centre, 1 = right"),
                        "fade_in_us": int("fade-in length in µs"),
                        "fade_out_us": int("fade-out length in µs"),
                        "muted": bool_("silence this clip"),
                        "denoise": bool_("neural background-noise removal (DNS64); the \
                            denoised audio renders in the background for playback and \
                            is applied again on export"),
                    }
                },
                "effects": {
                    "type": "array",
                    "description": "REPLACES the clip's effect chain, in order. Ids and \
                        parameter names come from get_catalog → effects. Numeric params \
                        accept keyframe curves; colours are '#rrggbb' in `color_params`.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "effect_id": { "type": "string" },
                            "enabled": { "type": "boolean" },
                            "params": { "type": "object" },
                            "color_params": { "type": "object" }
                        },
                        "required": ["effect_id"]
                    }
                },
                "transition_in": {
                    "description": "Transition at the start of the clip (A/B blend with an \
                        adjacent previous base clip, or an entrance from black/transparent \
                        otherwise — it always runs). `null` removes it.",
                    "anyOf": [
                        { "type": "null" },
                        {
                            "type": "object",
                            "properties": {
                                "effect_id": { "type": "string", "description": "default 'core.crossfade'" },
                                "duration_us": { "type": "integer", "description": "length in µs" }
                            },
                            "required": ["duration_us"]
                        }
                    ]
                },
                "transition_out": {
                    "description": "Exit transition over the clip's tail (to black on the base \
                        track, to transparent on layers). `null` removes it.",
                    "anyOf": [
                        { "type": "null" },
                        {
                            "type": "object",
                            "properties": {
                                "effect_id": { "type": "string", "description": "default 'core.crossfade'" },
                                "duration_us": { "type": "integer", "description": "length in µs" }
                            },
                            "required": ["duration_us"]
                        }
                    ]
                },
                "speed": num("playback rate: 2 = twice as fast (and half as long). \
                    Pitch is preserved. The clip may not grow past the next clip."),
            }),
            &["clip_id"], Kind::Edit,
        ),
        tool(
            "set_clip_content",
            "Edits what a clip SHOWS, according to its payload: the words and \
             style of a Text clip, the style and mode of a Subtitles clip, or \
             the parameters of a Generator clip. `style` is a patch. One undo.",
            json!({
                "clip_id": clip_id.clone(),
                "text": str_("Text clips: the new content"),
                "style": {
                    "type": "object",
                    "additionalProperties": false,
                    "description": "Text/Subtitles clips: partial patch of the style.",
                    "properties": {
                        "font": str_("family name or a .ttf path (see get_catalog → fonts)"),
                        "size": num("size in px at the sequence resolution"),
                        "color": str_("'#rrggbb'"),
                        "bg": { "description": "'#rrggbb' box behind the text, or null", "anyOf": [{"type":"null"},{"type":"string"}] },
                        "stroke_color": { "description": "outline colour, or null", "anyOf": [{"type":"null"},{"type":"string"}] },
                        "stroke_width": num("outline width in px"),
                        "highlight_color": { "description": "karaoke highlight colour, or null", "anyOf": [{"type":"null"},{"type":"string"}] },
                        "x_offset": num("px from the horizontal anchor"),
                        "y_offset": num("vertical position: px DOWN from the frame's centre, at a \
                            1080p reference (scaled to the output, ~1.8 px/unit on a 1080x1920 \
                            vertical). Larger = lower, negative = higher; ~380 sits in the lower third"),
                        "align": str_("'left' | 'center' | 'right'"),
                    }
                },
                "subtitles_mode": str_("Subtitles clips: 'phrase' (a line at a time), \
                    'word' (one word at a time) or 'karaoke' (the phrase, current word lit)"),
                "subtitles_max_words": int("Subtitles clips: how many words a caption may hold \
                    (1..20). This is the TOTAL per caption, not per line — a caption that does \
                    not fit the frame is wrapped onto several lines. 0 or null = fit as many \
                    words as the frame width allows (the default)."),
                "generator_id": str_("Generator clips: switch the generator"),
                "generator_params": { "type": "object", "description": "Generator clips: numeric/curve params (replaces them)" },
                "generator_colors": { "type": "object", "description": "Generator clips: '#rrggbb' params (replaces them)" },
            }),
            &["clip_id"], Kind::Edit,
        ),
        tool(
            "set_clip_name",
            "Gives a clip a human-friendly name so it reads as e.g. 'intro hook' \
             instead of a ULID in get_timeline. Purely a label — the id never \
             changes. Send an empty name to clear it (back to the derived \
             label).",
            json!({
                "clip_id": clip_id.clone(),
                "name": str_("the name to show, or '' to clear it"),
            }),
            &["clip_id", "name"], Kind::Edit,
        ),

        // -------------------------------------------------------------- tracks
        tool(
            "add_track",
            "Adds an empty video or audio track, named V(n)/A(n). Returns its id.",
            json!({ "kind": str_("'video' or 'audio'") }),
            &["kind"], Kind::Edit,
        ),
        tool(
            "remove_track",
            "Deletes a track and every clip on it (one undo restores both). \
             Refuses to remove the last track of its kind.",
            json!({ "track_id": str_("track id") }),
            &["track_id"], Kind::Edit,
        ),
        tool(
            "set_track_prop",
            "Sets exactly one track property: rename it, mute/solo/lock it, or \
             set its volume. Send exactly one of the optional fields.",
            json!({
                "track_id": str_("track id"),
                "name": str_("new name, 1..24 chars"),
                "muted": bool_("silence the track"),
                "solo": bool_("mute every other track of its kind"),
                "locked": bool_("refuse edits on this track"),
                "volume_db": num("track volume in dB, clamped to -60..12"),
            }),
            &["track_id"], Kind::Edit,
        ),

        // ----------------------------------------------------------- sequences
        tool(
            "set_sequence_props",
            "Changes a sequence's resolution and frame rate (e.g. 4K, 60 fps, \
             portrait). Clips are not moved; the canvas changes underneath them.",
            json!({
                "width": int("canvas width in px (≥16)"),
                "height": int("canvas height in px (≥16)"),
                "fps_num": int("frame-rate numerator, e.g. 30 or 30000"),
                "fps_den": int("frame-rate denominator, e.g. 1 or 1001"),
                "sequence_id": str_("defaults to the active sequence"),
            }),
            &["width", "height", "fps_num", "fps_den"], Kind::Edit,
        ),
        tool(
            "set_active_sequence",
            "Switches which sequence the other tools act on (and what the app \
             previews).",
            json!({ "sequence_id": str_("sequence id (from get_project_summary)") }),
            &["sequence_id"], Kind::Edit,
        ),
        tool(
            "remove_sequence",
            "Deletes a sequence and everything in it. If it was the active one, \
             another becomes active. Refuses to delete the last sequence.",
            json!({ "sequence_id": str_("sequence id") }),
            &["sequence_id"], Kind::Edit,
        ),
        tool(
            "generate_vertical",
            "Creates a 1080x1920 vertical version of the active sequence \
             (blurred background, video centred) and makes it active. Idempotent: \
             calling it twice switches to the existing vertical twin instead of \
             stacking another one.",
            json!({}), &[], Kind::Edit,
        ),

        // ------------------------------------------------------------------ AI
        tool(
            "remove_silences",
            "Detects the silences inside a media clip (from its conformed audio) \
             and either cuts them out (`delete`, closing the gaps), speeds them \
             up 4× (`speedup`), or just cuts at their edges without deleting \
             anything (`split`). Acts on ALL tracks; one undo entry. Returns how \
             many silences were found and how many µs were removed.",
            json!({
                "clip_id": str_("id of a media clip whose asset has a conformed audio"),
                "mode": { "type": "string", "enum": ["delete", "speedup", "split"],
                          "description": "default 'delete'" },
                "threshold_db": num("silence threshold in dBFS, -80..-10 (default -38)"),
                "min_silence_ms": int("ignore silences shorter than this, 50..5000 (default 400)"),
                "pad_ms": int("keep this much speech around each cut, 0..1000 (default 150)"),
            }),
            &["clip_id"], Kind::Edit,
        ),
        tool(
            "replace_words",
            "Fixes a recurring transcription error: every whole-word occurrence \
             of `from` (case-insensitive) gets `to` as its DISPLAY spelling. \
             Audio and timings are untouched; captions show the correction. \
             E.g. 'godo' → 'Godot'. Returns how many words changed.",
            json!({
                "transcript_id": str_("transcript id (from get_project_summary or get_transcript)"),
                "from": str_("the word as Whisper heard it"),
                "to": str_("the correct spelling; empty string restores the original"),
            }),
            &["transcript_id", "from", "to"], Kind::Edit,
        ),
        tool(
            "set_word_text",
            "Overrides the display spelling of ONE word by index (as returned by \
             get_transcript). Empty text restores the original.",
            json!({
                "transcript_id": str_("transcript id"),
                "index": int("0-based index into the transcript's `words`"),
                "text": str_("the corrected spelling; '' restores the original"),
            }),
            &["transcript_id", "index", "text"], Kind::Edit,
        ),
        tool(
            "save_avatar_config",
            "Creates or updates an avatar setup (expressions + emotion-classifier \
             settings) and returns its id. Pass the `id` of an existing setup to \
             update it, or omit it to create one. The api_key is kept in the \
             project but never written to an exported setup.",
            json!({
                "config": {
                    "type": "object",
                    "description": "{ id?, name, expressions: [{name, path}], \
                        shake_factor?: 0..3, scale?: 0.05..1, model?, api_base?, api_key? }. \
                        `path` is an image or video per expression; `name` is the emotion \
                        the LLM matches the speech against; the FIRST expression is the \
                        default.",
                    "properties": { "name": { "type": "string" }, "expressions": { "type": "array" } },
                    "required": ["name", "expressions"]
                }
            }),
            &["config"], Kind::Edit,
        ),
        tool(
            "remove_avatar_config",
            "Deletes an avatar setup from the project. The videos it already \
             generated stay in the media pool.",
            json!({ "config_id": str_("avatar setup id") }),
            &["config_id"], Kind::Edit,
        ),
        tool(
            "import_avatar_config",
            "Loads an avatar setup from a JSON file — ours, or a Youtubers-toolkit \
             `config.json` ({\"avatars\": {emotion: path}}). Expression paths \
             resolve relative to the file. Re-importing a setup with the same name \
             replaces it instead of duplicating it. Returns its id.",
            json!({ "path": str_("absolute path to the JSON file") }),
            &["path"], Kind::Edit,
        ),
        tool(
            "export_avatar_config",
            "Writes an avatar setup to a shareable JSON file. The api_key is NEVER \
             written out.",
            json!({
                "config_id": str_("avatar setup id"),
                "path": str_("absolute output path (.json)"),
            }),
            &["config_id", "path"], Kind::Destructive,
        ),
        tool(
            "generate_avatar_video",
            "Renders a transparent avatar video driven by an asset's VOICE: each \
             transcript segment is classified into one of the avatar's \
             expressions (via the configured LLM, or an offline heuristic) and \
             the avatar shakes with the speaker's volume. The result is imported \
             into the media pool; put it on the timeline with add_clip. \
             The driver's VIDEO is irrelevant — only its transcript and audio. \
             RUNS IN THE BACKGROUND: returns a `job_id` immediately (minutes of \
             work); poll get_job_status until done — the `result` then has \
             {asset_id}. Not undoable (it adds an asset).",
            json!({
                "config_id": str_("avatar setup id (get_catalog → avatar_setups)"),
                "driver_asset": str_("asset id of the VOICE; must be transcribed"),
            }),
            &["config_id", "driver_asset"], Kind::Destructive,
        ),
        tool(
            "list_tts_voices",
            "TTS engine catalog for voiceovers: built-ins (macOS `say`, the \
             self-contained Kokoro AI) plus user engines defined by JSON \
             manifests in the tts_engines folder. Each engine reports \
             availability, voices and its rate semantics.",
            json!({}),
            &[],
            Kind::Read,
        ),
        tool(
            "generate_speech",
            "Voiceover from text: synthesizes the script with a TTS engine \
             (see list_tts_voices), imports the audio into the media pool \
             and, when `at_us` is given, also drops a clip there on an audio \
             track. RUNS IN THE BACKGROUND: returns a `job_id` immediately; \
             poll get_job_status until done — the `result` then has \
             {asset_id}. `say` takes seconds; kokoro's FIRST run may install \
             its own environment and model (minutes). Not undoable (it adds \
             an asset).",
            json!({
                "text": str_("the exact words to speak"),
                "engine": str_("engine id from list_tts_voices (say | kokoro | …)"),
                "voice": str_("voice id from list_tts_voices (omit for the engine's default)"),
                "rate": {
                    "type": "number",
                    "description": "engine-specific rate (say: words/min 90-400; kokoro: speed × 0.5-2)"
                },
                "at_us": int("timeline µs to insert the clip at; omit to only import"),
            }),
            &["text", "engine"],
            Kind::Destructive,
        ),
        tool(
            "reload_effect_packs",
            "Re-reads the user effect packs from the effects folder and rebuilds \
             the catalog. Call it after writing a new pack manifest to disk; \
             returns the folder and any manifest errors (a bad manifest is \
             skipped, never fatal).",
            json!({}), &[], Kind::Edit,
        ),

        // ------------------------------------------------------------- project
        tool(
            "new_project",
            "Throws away the open project (unsaved changes AND the undo history) \
             and starts an empty one. Save first if it matters.",
            json!({ "name": str_("project name") }),
            &["name"], Kind::Destructive,
        ),
        tool(
            "open_project",
            "Opens a .uep file, replacing the open project and its undo history. \
             Relative media paths resolve against the .uep's folder; missing \
             files are flagged `offline`.",
            json!({ "path": str_("absolute path to a .uep file") }),
            &["path"], Kind::Destructive,
        ),
        tool(
            "save_project",
            "Writes the project to disk (atomically) and returns the path. \
             Without `path` it saves over the file it was opened from. Media \
             under the project folder is stored relative, so the folder stays \
             portable.",
            json!({ "path": str_("absolute .uep path (default: the current one)") }),
            &[], Kind::Destructive,
        ),

        // -------------------------------------------------------------- render
        tool(
            "export_video",
            "Renders the active sequence to a file with ffmpeg. RUNS IN THE \
             BACKGROUND: returns a `job_id` immediately (a real export takes tens \
             of seconds to minutes); poll get_job_status until done — the \
             `result` then has {path, pieces}. Snapshots the timeline at call \
             time, so later edits don't affect this render. Pass `ranges` to \
             render several chunks concatenated, in order, into ONE file (the \
             'pieces' feature); omit it to render everything.",
            json!({
                "path": str_("absolute output path; the extension should match `format`"),
                "ranges": {
                    "type": "array",
                    "items": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                    "description": "[[start_us, end_us], …] pieces, concatenated in the order given"
                },
                "format": { "type": "string", "enum": ["mp4", "m4a", "gif"], "description": "default mp4 (m4a = audio only)" },
                "max_height": int("downscale so the height is at most this (e.g. 1080)"),
                "crf": int("x264 quality, 10 (best) .. 40 (worst); default 18"),
                "loudnorm": bool_("normalise loudness to broadcast levels (default false)"),
            }),
            &["path"], Kind::Destructive,
        ),
        tool(
            "debug_render_frame",
            "Renders the paused-preview frame at t_us and RETURNS IT AS AN IMAGE \
             you can see, plus the temp path it was saved to. The frame includes \
             the titles and subtitles active at t_us, composited the same way the \
             export burns them in — so this is a faithful check of what \
             export_video will write. Only the top video layer is shown for the \
             video itself (extra video tracks are export-only). Errors instead of \
             returning a black frame when no clip covers t_us.",
            json!({
                "t_us": int("timeline time in µs"),
                "max_width": int("render width in px, 64..1600 (default 1280)"),
            }),
            &["t_us"], Kind::Read,
        ),
        tool(
            "playback",
            "Drives the real player, so you can reproduce what the user sees: \
             `play` (from from_us), `pause`, `seek` (move the playhead to from_us \
             without playing), or `position` (where it is now).",
            json!({
                "action": { "type": "string", "enum": ["play", "pause", "seek", "position"] },
                "from_us": int("play/seek: the target time in µs (default 0)"),
            }),
            &["action"], Kind::Edit,
        ),

        // -------------------------------------------------------------- jobs
        tool(
            "get_job_status",
            "Poll a background job started by transcribe_asset, export_video or \
             generate_avatar_video. Returns {status: running|done|error, \
             progress: 0..1, message, result?, error?, output_path?, output_bytes?}. \
             `output_path`/`output_bytes` tell you WHICH file the job is writing and \
             how big it has got, so a slow job is distinguishable from a hung one \
             without inspecting the ffmpeg process. Those tools return a \
             `job_id` immediately (they can run for minutes) — keep calling this \
             until status is 'done' (the tool's real result is in `result`) or \
             'error'. A client-side timeout on the launching call is NOT a \
             failure; the job keeps running here, so poll instead of re-running.",
            json!({ "job_id": str_("the id returned by the launching tool") }),
            &["job_id"], Kind::Read,
        ),
        tool(
            "list_jobs",
            "All background jobs this session (running and finished), newest \
             first: their kind, status, progress and message.",
            json!({}), &[], Kind::Read,
        ),
        tool(
            "cancel_job",
            "Stops a running job (transcribe or export). The work unwinds on its \
             own — a transcription aborts inside whisper, an export kills its \
             ffmpeg and removes the partial file — so the app stays usable. \
             Before this the only way out of a long transcription was killing the \
             whole editor.",
            json!({ "job_id": str_("the id returned by the launching tool") }),
            &["job_id"], Kind::Destructive,
        ),

        // ------------------------------------------------------------- AI vision
        tool(
            "analyze_frame",
            "Extract a frame from a video/image asset at a given time and return it as base64 JPEG for AI analysis.",
            json!({
                "asset_id": str_("asset id from get_media_pool"),
                "time_us": int("time in microseconds (default 0)"),
                "prompt": str_("what to analyze"),
            }),
            &["asset_id"], Kind::Read,
        ),

        // ------------------------------------------------------------- history
        tool(
            "undo",
            "Reverts the last edit (exactly one tool call) and returns its label.",
            json!({}), &[], Kind::Edit,
        ),
        tool(
            "redo",
            "Re-applies the last undone edit and returns its label.",
            json!({}), &[], Kind::Edit,
        ),
    ])
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

fn text_result(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": v.to_string() }] })
}

fn tool_error(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

/// A JPEG result the agent can actually SEE: an MCP `image` content block
/// (base64) plus a one-line caption and the temp path it was also written to.
fn image_result(jpeg: &[u8], filename: &str, caption: &str) -> Value {
    use base64::Engine;
    let path = std::env::temp_dir().join(filename);
    let _ = std::fs::write(&path, jpeg);
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
    json!({ "content": [
        { "type": "text", "text": format!("{caption} — {} bytes, also at {}", jpeg.len(), path.display()) },
        { "type": "image", "data": b64, "mimeType": "image/jpeg" },
    ]})
}

/// `Result` → MCP tool result, so every handler can be written with `?`.
fn finish(r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => text_result(v),
        Err(e) => tool_error(&e),
    }
}

/// Runs `work` as a background job and returns `{job_id, status, …}` so the
/// launching call never blocks the client into a timeout. With a real app the
/// job runs on its own thread and the agent polls get_job_status; in tests
/// (no app) it runs inline and comes back already `done`, so the SAME polling
/// path is exercised. `work(state, job_id)` may report progress via
/// `crate::job_progress` and returns the tool's real result.
fn run_async(
    state: &AppState,
    app: Option<&tauri::AppHandle>,
    kind: &str,
    message: &str,
    work: impl FnOnce(&AppState, &str) -> Result<Value, String> + Send + 'static,
) -> Value {
    let job_id = crate::job_start(state, kind, message);
    match app {
        Some(app) => {
            let app = app.clone();
            let jid = job_id.clone();
            std::thread::spawn(move || {
                use tauri::Manager;
                let state = app.state::<AppState>();
                let r = work(&state, &jid);
                let ok = r.is_ok();
                crate::job_finish(&state, &jid, r);
                if ok {
                    use tauri::Emitter;
                    let _ = app.emit("state-changed", ());
                }
            });
        }
        None => {
            let r = work(state, &job_id);
            crate::job_finish(state, &job_id, r);
        }
    }
    let mut snap = state
        .jobs
        .lock()
        .unwrap()
        .get(&job_id)
        .map(|j| j.to_value())
        .unwrap_or_else(|| json!({ "job_id": job_id }));
    if let Some(obj) = snap.as_object_mut() {
        obj.insert("poll_with".into(), json!("get_job_status"));
    }
    text_result(snap)
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    fn id(&self, key: &str) -> Result<Id, String> {
        self.0
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("missing {key}"))?
            .parse()
            .map_err(|e| format!("invalid {key}: {e}"))
    }
    fn opt_id(&self, key: &str) -> Result<Option<Id>, String> {
        match self.0.get(key).and_then(|v| v.as_str()) {
            None => Ok(None),
            Some(s) => s.parse().map(Some).map_err(|e| format!("invalid {key}: {e}")),
        }
    }
    fn i64(&self, key: &str) -> Result<i64, String> {
        self.0.get(key).and_then(|v| v.as_i64()).ok_or_else(|| format!("missing {key}"))
    }
    fn i64_or(&self, key: &str, default: i64) -> i64 {
        self.0.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
    }
    fn f64(&self, key: &str) -> Option<f64> {
        self.0.get(key).and_then(|v| v.as_f64())
    }
    fn str(&self, key: &str) -> Result<&'a str, String> {
        self.0.get(key).and_then(|v| v.as_str()).ok_or_else(|| format!("missing {key}"))
    }
    fn bool_or(&self, key: &str, default: bool) -> bool {
        self.0.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    }
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key)
    }
    /// [[a, b], …] pairs, dropping the malformed and the empty ones.
    fn ranges(&self, key: &str) -> Vec<(i64, i64)> {
        self.0
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let p = p.as_array()?;
                        Some((p.first()?.as_i64()?, p.get(1)?.as_i64()?))
                    })
                    .filter(|(a, b)| b > a)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The active sequence unless the caller named another one.
fn target_sequence(state: &AppState, args: &Args) -> Result<Id, String> {
    let store = state.store.lock().unwrap();
    match args.opt_id("sequence_id")? {
        Some(id) if store.project.sequence(id).is_none() => Err(format!("sequence {id} not found")),
        Some(id) => Ok(id),
        None => Ok(store.project.active_sequence),
    }
}

// ---------------------------------------------------------------------------
// Partial patches (agents send only what changes)
// ---------------------------------------------------------------------------

/// Deserializes `T` from the current value with the patch's keys overwritten.
/// `flat` maps a patch key onto a JSON pointer inside the serialized value, so
/// tuples like `position: [x, y]` can be addressed as `position_x`.
fn patch<T: serde::Serialize + serde::de::DeserializeOwned>(
    current: &T,
    patch: &Value,
    flat: &[(&str, &str)],
    what: &str,
) -> Result<T, String> {
    let mut base = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let Some(obj) = patch.as_object() else {
        return Err(format!("{what} must be an object"));
    };
    let flat_keys: std::collections::HashMap<&str, &str> = flat.iter().copied().collect();
    for (key, value) in obj {
        match flat_keys.get(key.as_str()) {
            // e.g. "position_x" → "/position/0"
            Some(pointer) => {
                let slot = base
                    .pointer_mut(pointer)
                    .ok_or_else(|| format!("{what}: cannot set {key}"))?;
                *slot = value.clone();
            }
            // a plain field of the struct
            None if base.get(key).is_some() => {
                base[key] = value.clone();
            }
            None => return Err(format!("{what}: unknown field '{key}'")),
        }
    }
    serde_json::from_value(base).map_err(|e| format!("{what}: {e}"))
}

const TRANSFORM_FLAT: &[(&str, &str)] = &[
    ("position_x", "/position/0"),
    ("position_y", "/position/1"),
    ("scale_x", "/scale/0"),
    ("scale_y", "/scale/1"),
    ("crop_left", "/crop/0"),
    ("crop_top", "/crop/1"),
    ("crop_right", "/crop/2"),
    ("crop_bottom", "/crop/3"),
];

fn patch_transform(current: &Transform2D, p: &Value) -> Result<Transform2D, String> {
    patch(current, p, TRANSFORM_FLAT, "transform")
}
fn patch_audio(current: &AudioProps, p: &Value) -> Result<AudioProps, String> {
    patch(current, p, &[], "audio")
}
fn patch_style(current: &TextStyle, p: &Value) -> Result<TextStyle, String> {
    patch(current, p, &[], "style")
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

fn call_tool(state: &AppState, app: Option<&tauri::AppHandle>, name: &str, raw: &Value) -> Value {
    let args = Args(raw);
    match name {
        // ---------------------------------------------------------------- read
        "get_project_summary" => finish(get_project_summary(state)),
        "get_timeline" => finish(get_timeline(state, &args)),
        "get_media_pool" => finish(
            serde_json::to_value(&state.store.lock().unwrap().project.assets)
                .map_err(|e| e.to_string()),
        ),
        "get_transcript" => finish(get_transcript(state, &args)),
        "find_words" => finish(find_words(state, &args)),
        "get_catalog" => finish(get_catalog(state)),

        // --------------------------------------------------------------- media
        "import_media" => finish((|| {
            let paths: Vec<String> = args
                .get("paths")
                .and_then(|v| v.as_array())
                .ok_or("missing paths")?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if paths.is_empty() {
                return Err("paths is empty".into());
            }
            let ids = crate::import_media_impl(app, state, &paths)?;
            let store = state.store.lock().unwrap();
            let assets: Vec<Value> = ids
                .iter()
                .filter_map(|id| store.project.asset(*id))
                .map(|a| {
                    json!({
                        "asset_id": a.id.to_string(),
                        "kind": a.kind,
                        "path": a.path,
                        "duration_us": a.probe.duration_us,
                        "has_audio": a.probe.audio_channels > 0,
                        "audio_ready": a.audio_conform.is_some(),
                    })
                })
                .collect();
            Ok(json!({ "imported": assets.len(), "assets": assets }))
        })()),
        "transcribe_asset" => match (|| {
            let asset_id = args.id("asset_id")?;
            let model = args.get("model").and_then(|v| v.as_str()).map(str::to_string);
            Ok::<_, String>((asset_id, model))
        })() {
            Err(e) => tool_error(&e),
            Ok((asset_id, model)) => run_async(state, app, "transcribe", "transcribing…", move |s, jid| {
                let cancel = crate::job_cancel_flag(s, jid);
                let (transcript_id, words) = crate::transcribe_blocking_with(
                    s,
                    asset_id,
                    model,
                    |p| crate::job_progress(s, jid, p, ""),
                    &cancel,
                )?;
                Ok(json!({ "transcript_id": transcript_id.to_string(), "words": words }))
            }),
        },
        "relink_asset" => finish((|| {
            let asset_id = args.id("asset_id")?;
            let new_path = args.str("new_path")?.to_string();
            crate::relink_asset_impl(app, state, asset_id, new_path)?;
            Ok(json!({ "ok": true }))
        })()),
        "set_project_settings" => finish((|| {
            let lang = args.get("whisper_language").and_then(|v| v.as_str()).map(str::to_string);
            let model = args.get("whisper_model").and_then(|v| v.as_str()).map(str::to_string);
            if lang.is_none() && model.is_none() {
                return Err("pass whisper_language and/or whisper_model".into());
            }
            crate::set_project_settings_impl(state, lang, model);
            Ok(json!({ "ok": true }))
        })()),

        // ------------------------------------------------------------ timeline
        "add_clip" => finish((|| {
            let asset_id = args.id("asset_id")?;
            let track_id = args.opt_id("track_id")?;
            let at_us = args.i64_or("at_us", 0);
            let clip_id = add_clip_inner(state, asset_id, at_us, track_id)?;
            Ok(json!({ "clip_id": clip_id.to_string() }))
        })()),
        "add_text_clip" => finish((|| {
            let content = args.str("content")?;
            let id = crate::add_text_clip_impl(
                state,
                content,
                args.i64_or("at_us", 0),
                args.i64_or("duration_us", 4_000_000),
            )?;
            Ok(json!({ "clip_id": id.to_string() }))
        })()),
        "add_generator_clip" => finish((|| {
            let gen = args.str("generator_id")?;
            let id = crate::add_generator_clip_impl(
                state,
                gen,
                args.i64_or("at_us", 0),
                args.i64_or("duration_us", 4_000_000),
            )?;
            Ok(json!({ "clip_id": id.to_string() }))
        })()),
        "add_subtitles_clip" => finish((|| {
            let id = crate::add_subtitles_clip_impl(state, args.id("clip_id")?)?;
            Ok(json!({ "clip_id": id.to_string() }))
        })()),
        "split_clip" => finish((|| {
            let clip_id = args.id("clip_id")?;
            let t_us = args.i64("t_us")?;
            let (l, r) = state
                .store
                .lock()
                .unwrap()
                .split_clip(clip_id, t_us)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "left": l.to_string(), "right": r.to_string() }))
        })()),
        "delete_clips" => finish((|| {
            let ids: Result<Vec<Id>, String> = args
                .get("ids")
                .and_then(|v| v.as_array())
                .ok_or("missing ids")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .ok_or_else(|| "ids must be strings".to_string())?
                        .parse::<Id>()
                        .map_err(|e| format!("invalid clip id: {e}"))
                })
                .collect();
            let ids = ids?;
            state
                .store
                .lock()
                .unwrap()
                .delete_clips(&ids, args.bool_or("ripple", false))
                .map_err(|e| e.to_string())?;
            Ok(json!({ "deleted": ids.len() }))
        })()),
        "move_clip" => finish((|| {
            let mode = if args.bool_or("overwrite", false) {
                InsertMode::Overwrite
            } else {
                InsertMode::Strict
            };
            state
                .store
                .lock()
                .unwrap()
                .move_clip(args.id("clip_id")?, args.id("to_track")?, args.i64("to_start_us")?, mode)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),
        "trim_clip" => finish((|| {
            let left = args
                .get("left")
                .and_then(|v| v.as_bool())
                .ok_or("missing left (true = start edge)")?;
            state
                .store
                .lock()
                .unwrap()
                .trim_clip(args.id("clip_id")?, left, args.i64("new_edge_us")?)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),
        "unlink_clip" => finish((|| {
            let n = crate::unlink_clip_impl(state, args.id("clip_id")?)?;
            Ok(json!({ "unlinked": n }))
        })()),
        "cut_ranges" => finish((|| {
            let seq_id = target_sequence(state, &args)?;
            let ranges = args.ranges("ranges");
            if ranges.is_empty() {
                return Err("ranges is empty (each item is [start_us, end_us] with end > start)".into());
            }
            let removed: i64 = ranges.iter().map(|(s, e)| e - s).sum();
            state
                .store
                .lock()
                .unwrap()
                .cut_ranges(seq_id, &ranges, args.bool_or("ripple", true))
                .map_err(|e| e.to_string())?;
            Ok(json!({ "cut": ranges.len(), "removed_us": removed }))
        })()),
        "move_range" => finish((|| {
            let seq_id = target_sequence(state, &args)?;
            state
                .store
                .lock()
                .unwrap()
                .move_range(seq_id, args.i64("from_us")?, args.i64("to_us")?, args.i64("dest_us")?)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),

        // ------------------------------------------------------ clip properties
        "set_clip_properties" => finish(set_clip_properties(state, app, &args)),
        "set_clip_content" => finish(set_clip_content(state, &args)),
        "set_clip_name" => finish((|| {
            let clip_id = args.id("clip_id")?;
            let name = args.str("name")?.trim();
            let name = (!name.is_empty()).then(|| name.to_string());
            state
                .store
                .lock()
                .unwrap()
                .dispatch("Rename clip", vec![ue_core::Action::SetClipName { clip_id, name }])
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),

        // -------------------------------------------------------------- tracks
        "add_track" => finish((|| {
            let id = crate::add_track_impl(state, args.str("kind")?)?;
            Ok(json!({ "track_id": id.to_string() }))
        })()),
        "remove_track" => finish((|| {
            crate::remove_track_impl(state, args.id("track_id")?)?;
            Ok(json!({ "ok": true }))
        })()),
        "set_track_prop" => finish(set_track_prop(state, &args)),

        // ----------------------------------------------------------- sequences
        "set_sequence_props" => finish((|| {
            let sequence_id = target_sequence(state, &args)?;
            let dim = |k: &str| -> Result<u32, String> {
                let v = args.i64(k)?;
                u32::try_from(v).map_err(|_| format!("{k} out of range"))
            };
            let (width, height) = (dim("width")?, dim("height")?);
            let (fps_num, fps_den) = (dim("fps_num")?, dim("fps_den")?);
            state
                .store
                .lock()
                .unwrap()
                .dispatch(
                    "Sequence settings",
                    vec![ue_core::Action::SetSequenceProps {
                        sequence_id,
                        resolution: (width, height),
                        fps: (fps_num, fps_den),
                    }],
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),
        "set_active_sequence" => finish((|| {
            let sequence_id = args.id("sequence_id")?;
            state
                .store
                .lock()
                .unwrap()
                .dispatch(
                    "Change sequence",
                    vec![ue_core::Action::SetActiveSequence { sequence_id }],
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),
        "remove_sequence" => finish(remove_sequence(state, &args)),
        "generate_vertical" => finish(
            crate::generate_vertical_impl(state).map(|id| json!({ "sequence_id": id })),
        ),

        // ------------------------------------------------------------------ AI
        "remove_silences" => finish(remove_silences(state, &args)),
        "replace_words" => finish(replace_words(state, &args)),
        "set_word_text" => finish((|| {
            let transcript_id = args.id("transcript_id")?;
            let index = usize::try_from(args.i64("index")?).map_err(|_| "index must be ≥ 0")?;
            let text = args.str("text")?.trim();
            let display = (!text.is_empty()).then(|| text.to_string());
            state
                .store
                .lock()
                .unwrap()
                .dispatch(
                    "Edit word",
                    vec![ue_core::Action::SetWordText { transcript_id, index, display }],
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),
        "save_avatar_config" => finish((|| {
            let config = args.get("config").ok_or("missing config")?.clone();
            let id = crate::save_avatar_config_impl(state, config)?;
            Ok(json!({ "config_id": id.to_string() }))
        })()),
        "remove_avatar_config" => finish((|| {
            let config_id = args.id("config_id")?;
            state
                .store
                .lock()
                .unwrap()
                .dispatch("Delete avatar", vec![ue_core::Action::RemoveAvatarConfig { config_id }])
                .map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        })()),
        "import_avatar_config" => finish((|| {
            let id = crate::import_avatar_config_impl(state, args.str("path")?)?;
            Ok(json!({ "config_id": id.to_string() }))
        })()),
        "export_avatar_config" => finish((|| {
            let config_id = args.id("config_id")?;
            let path = crate::export_avatar_config_impl(state, config_id, args.str("path")?)?;
            Ok(json!({ "path": path }))
        })()),
        "generate_avatar_video" => match (|| {
            Ok::<_, String>((args.id("config_id")?, args.id("driver_asset")?))
        })() {
            Err(e) => tool_error(&e),
            Ok((config_id, driver_asset)) => {
                run_async(state, app, "avatar", "generating avatar…", move |s, jid| {
                    let jid = jid.to_string();
                    let asset_id = crate::avatar_generate_blocking(
                        s,
                        config_id,
                        driver_asset,
                        &|_stage, p, msg| crate::job_progress(s, &jid, p, &msg),
                    )?;
                    Ok(json!({ "asset_id": asset_id.to_string() }))
                })
            }
        },
        "list_tts_voices" => {
            let tts_dir = state.tts_dir.lock().unwrap().clone();
            let kokoro_env = state.kokoro_env_dir.lock().unwrap().clone();
            text_result(json!({
                "engines": ue_ai::tts::catalog(kokoro_env.as_deref(), tts_dir.as_deref()),
            }))
        }
        "generate_speech" => match (|| {
            Ok::<_, String>((args.str("text")?.to_string(), args.str("engine")?.to_string()))
        })() {
            Err(e) => tool_error(&e),
            Ok((text, engine_id)) => {
                let voice = args.get("voice").and_then(|v| v.as_str()).map(str::to_string);
                let rate = args.f64("rate");
                let at_us = args.get("at_us").and_then(|v| v.as_i64());
                let app_handle = app.cloned();
                run_async(state, app, "tts", "generating voiceover…", move |s, jid| {
                    let jid = jid.to_string();
                    let asset_id = crate::speech_generate_blocking(
                        app_handle.as_ref(),
                        s,
                        &text,
                        &engine_id,
                        voice.as_deref(),
                        rate,
                        at_us,
                        &|_stage, p, msg| crate::job_progress(s, &jid, p, &msg),
                    )?;
                    Ok(json!({ "asset_id": asset_id.to_string() }))
                })
            }
        },
        "reload_effect_packs" => {
            let errors = crate::reload_packs(state);
            text_result(json!({
                "effects": ue_render::catalog_json(&state.registry.lock().unwrap()),
                "errors": errors,
                "dir": state.effects_dir.lock().unwrap().as_ref().map(|d| d.display().to_string()),
            }))
        }

        // ------------------------------------------------------------- project
        "new_project" => finish((|| {
            crate::new_project_impl(state, args.str("name")?);
            Ok(json!({ "ok": true }))
        })()),
        "open_project" => finish((|| {
            crate::open_project_impl(app, state, args.str("path")?)?;
            Ok(json!({ "ok": true }))
        })()),
        "save_project" => finish(
            crate::save_project_impl(
                state,
                args.get("path").and_then(|v| v.as_str()).map(str::to_string),
            )
            .map(|p| json!({ "path": p })),
        ),

        // -------------------------------------------------------------- render
        "export_video" => match export_plan(state, &args) {
            Err(e) => tool_error(&e),
            Ok(plan) => run_async(state, app, "export", "rendering…", move |s, jid| {
                plan.run(s, jid)
            }),
        },
        "debug_render_frame" => {
            let t_us = args.i64_or("t_us", 0);
            let max_width = args.i64_or("max_width", 1280).clamp(64, 1600) as u32;
            match crate::render_frame_impl(state, t_us, max_width) {
                Ok(bytes) if bytes.is_empty() => tool_error(
                    "no video at that time (black frame): no clip covers t_us, or -ss ran past the clip",
                ),
                Ok(bytes) => image_result(&bytes, "ue_debug_frame.jpg", &format!("paused preview @ {:.3}s (includes titles + subtitles, exactly like the export)", t_us as f64 / 1e6)),
                Err(e) => tool_error(&e),
            }
        }
        "playback" => finish(playback(state, app, &args)),

        // ---------------------------------------------------------------- jobs
        "cancel_job" => finish((|| {
            let id = args.get("job_id").and_then(|v| v.as_str()).ok_or("missing job_id")?;
            let stopped = crate::job_request_cancel(state, id);
            Ok(json!({
                "job_id": id,
                "cancelled": stopped,
                "note": if stopped { "the job was asked to stop" } else { "no running job with that id" },
            }))
        })()),
        "get_job_status" => finish((|| {
            let id = args.str("job_id")?;
            state
                .jobs
                .lock()
                .unwrap()
                .get(id)
                .map(|j| j.to_value())
                .ok_or_else(|| format!("no job '{id}' (it may have been evicted; jobs are session-scoped)"))
        })()),
        "list_jobs" => {
            let mut jobs: Vec<Value> =
                state.jobs.lock().unwrap().values().map(|j| j.to_value()).collect();
            // newest first (ULID ids sort by time)
            jobs.sort_by(|a, b| b["job_id"].as_str().cmp(&a["job_id"].as_str()));
            text_result(json!({ "jobs": jobs }))
        }

        // ------------------------------------------------------------- AI vision
        "analyze_frame" => finish((|| {
            let asset_id = args.id("asset_id")?;
            let time_us = args.i64("time_us").unwrap_or(0);
            let prompt = args.str("prompt").unwrap_or("Describe the image in detail: colors, brightness, contrast, sharpness, any issues.");

            let store = state.store.lock().unwrap();
            let asset = store.project.asset(asset_id).ok_or("asset not found")?;
            let path = asset.path.clone();
            drop(store);

            let time_sec = time_us as f64 / 1_000_000.0;
            let ss_str = format!("{:.6}", time_sec);
            let path_str = path.to_string_lossy().into_owned();
            let output = std::process::Command::new("ffmpeg")
                .args([
                    "-ss", &ss_str,
                    "-i", &path_str,
                    "-frames:v", "1",
                    "-f", "image2",
                    "-c:v", "mjpeg",
                    "-q:v", "5",
                    "-vf", "scale=512:-1",
                    "pipe:1",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("ffmpeg failed: {e}"))?;

            if !output.status.success() {
                return Err("ffmpeg frame extraction failed".into());
            }

            let b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &output.stdout,
            );

            Ok(json!({
                "frame_base64": b64,
                "mime_type": "image/jpeg",
                "prompt": prompt,
                "time_us": time_us,
            }))
        })()),
        // ------------------------------------------------------------- history
        "undo" => finish(
            state
                .store
                .lock()
                .unwrap()
                .undo()
                .map(|label| json!({ "undone": label }))
                .map_err(|e| e.to_string()),
        ),
        "redo" => finish(
            state
                .store
                .lock()
                .unwrap()
                .redo()
                .map(|label| json!({ "redone": label }))
                .map_err(|e| e.to_string()),
        ),

        _ => tool_error(&format!("unknown tool: {name} (call tools/list)")),
    }
}

// ---------------------------------------------------------------------------
// Handlers that need more than a few lines
// ---------------------------------------------------------------------------

fn get_project_summary(state: &AppState) -> Result<Value, String> {
    let store = state.store.lock().unwrap();
    let p = &store.project;
    let sequences: Vec<Value> = p
        .sequences
        .iter()
        .map(|s| {
            json!({
                "sequence_id": s.id.to_string(),
                "name": s.name,
                "active": s.id == p.active_sequence,
                "resolution": s.resolution,
                "fps": s.fps,
                "duration_us": s.duration_us(),
                "tracks": s.tracks.iter().map(|t| json!({
                    "track_id": t.id.to_string(),
                    "name": t.name,
                    "kind": t.kind,
                    "clips": t.clips.len(),
                    "muted": t.muted,
                    "locked": t.locked,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "name": p.name,
        "path": state.path.lock().unwrap().as_ref().map(|p| p.display().to_string()),
        "dirty": store.dirty,
        "assets": p.assets.len(),
        "sequences": sequences,
        "transcripts": p.transcripts.iter().map(|t| json!({
            "transcript_id": t.id.to_string(),
            "asset_id": t.asset_id.to_string(),
            "language": t.language,
            "words": t.words.len(),
            "segments": t.segments.len(),
        })).collect::<Vec<_>>(),
        "avatar_setups": p.avatars.iter().map(|c| json!({
            "config_id": c.id.to_string(),
            "name": c.name,
            "expressions": c.expressions.iter().map(|e| &e.name).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "can_undo": store.can_undo(),
        "can_redo": store.can_redo(),
        "undo_history": store.undo_labels(),
    }))
}

/// The sequence as JSON, with a human-friendly `label` added to every clip
/// (the custom name, or one derived from the payload) so the agent isn't
/// staring at raw ULIDs.
fn get_timeline(state: &AppState, args: &Args) -> Result<Value, String> {
    let seq_id = target_sequence(state, args)?;
    let store = state.store.lock().unwrap();
    let seq = store.project.sequence(seq_id).ok_or("sequence not found")?;
    let mut val = serde_json::to_value(seq).map_err(|e| e.to_string())?;
    // enrich each serialized clip with its display label (id → readable name)
    if let Some(tracks) = val.get_mut("tracks").and_then(|t| t.as_array_mut()) {
        for (ti, track) in seq.tracks.iter().enumerate() {
            let Some(clips) = tracks
                .get_mut(ti)
                .and_then(|t| t.get_mut("clips"))
                .and_then(|c| c.as_array_mut())
            else {
                continue;
            };
            for (ci, clip) in track.clips.iter().enumerate() {
                if let Some(obj) = clips.get_mut(ci).and_then(|c| c.as_object_mut()) {
                    obj.insert("label".into(), json!(clip.display_label(&store.project)));
                }
            }
        }
    }
    Ok(val)
}

/// Finds a transcript by asset id OR transcript id (agents have either).
fn find_transcript<'a>(
    project: &'a ue_core::model::Project,
    args: &Args,
) -> Result<&'a ue_core::model::TranscriptDoc, String> {
    if let Some(tid) = args.opt_id("transcript_id")? {
        return project
            .transcripts
            .iter()
            .find(|t| t.id == tid)
            .ok_or_else(|| "transcript not found".to_string());
    }
    let asset_id = args.id("asset_id")?;
    project
        .transcripts
        .iter()
        .find(|t| t.asset_id == asset_id)
        .ok_or_else(|| "the asset has no transcript yet; call transcribe_asset first".to_string())
}

/// The raw word dump is enormous (100k+ chars). Default to a compact,
/// phrase-level view with timestamps, and let the caller pick the granularity
/// and a time window so it never has to pull everything.
fn get_transcript(state: &AppState, args: &Args) -> Result<Value, String> {
    let store = state.store.lock().unwrap();
    let doc = find_transcript(&store.project, args)?;
    let granularity = args.get("granularity").and_then(|v| v.as_str()).unwrap_or("phrases");
    // optional [start_us, end_us) window (defaults to the whole thing)
    let win_start = args.get("start_us").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    let win_end = args.get("end_us").and_then(|v| v.as_i64()).unwrap_or(i64::MAX);
    let overlaps = |a: i64, b: i64| a < win_end && b > win_start;

    let base = json!({
        "transcript_id": doc.id.to_string(),
        "asset_id": doc.asset_id.to_string(),
        "language": doc.language,
        "word_count": doc.words.len(),
    });
    let mut out = base.as_object().unwrap().clone();
    match granularity {
        // just the words, no timing: cheap, for reading/searching
        "text" => {
            let text: String = doc
                .words
                .iter()
                .filter(|w| !w.rejected && overlaps(w.start_us, w.end_us))
                .map(|w| w.label())
                .collect::<Vec<_>>()
                .join(" ");
            out.insert("text".into(), json!(text));
        }
        // Whisper's own segments (coarse, carry emotion/volume)
        "segments" => {
            let segs: Vec<Value> = doc
                .segments
                .iter()
                .filter(|s| overlaps(s.start_us, s.end_us))
                .map(|s| json!({
                    "text": s.text, "start_us": s.start_us, "end_us": s.end_us,
                    "emotion": s.emotion,
                }))
                .collect();
            out.insert("segments".into(), json!(segs));
        }
        // phrase-level chunks with timestamps: the sweet spot for choosing cuts
        "phrases" | "sentences" => {
            let max_chars = args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(48) as usize;
            let phrases: Vec<Value> = ue_export::graph::transcript_phrases(doc, max_chars)
                .into_iter()
                .filter(|(_, a, b)| overlaps(*a, *b))
                .map(|(text, start_us, end_us)| json!({
                    "text": text, "start_us": start_us, "end_us": end_us,
                }))
                .collect();
            out.insert("phrases".into(), json!(phrases));
        }
        // the full word-level detail (can be huge): use a window
        "words" => {
            let words: Vec<Value> = doc
                .words
                .iter()
                .enumerate()
                .filter(|(_, w)| overlaps(w.start_us, w.end_us))
                .map(|(i, w)| json!({
                    "index": i, "text": w.label(), "start_us": w.start_us, "end_us": w.end_us,
                    "rejected": w.rejected,
                }))
                .collect();
            if doc.words.len() > 4000 && win_start == i64::MIN && win_end == i64::MAX {
                out.insert("note".into(), json!(
                    "large transcript returned whole; pass start_us/end_us to window it, \
                     or use granularity 'phrases'"
                ));
            }
            out.insert("words".into(), json!(words));
        }
        other => {
            return Err(format!(
                "unknown granularity '{other}' (text|segments|phrases|words)"
            ))
        }
    }
    Ok(Value::Object(out))
}

/// Locate a word or short phrase in a transcript and return each hit with its
/// timestamp and a few neighbour words for context — so an agent can find the
/// cut point for "remove the sentence about X" without pulling everything.
fn find_words(state: &AppState, args: &Args) -> Result<Value, String> {
    let query = args.str("query")?.trim().to_lowercase();
    if query.is_empty() {
        return Err("query is empty".into());
    }
    let context = args.get("context").and_then(|v| v.as_i64()).unwrap_or(4).clamp(0, 20) as usize;
    let terms: Vec<&str> = query.split_whitespace().collect();
    let store = state.store.lock().unwrap();
    let doc = find_transcript(&store.project, args)?;
    let words = &doc.words;
    let norm = |w: &ue_core::model::Word| {
        w.label().to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>()
    };
    let normed: Vec<String> = words.iter().map(norm).collect();
    let want: Vec<String> = terms
        .iter()
        .map(|t| t.chars().filter(|c| c.is_alphanumeric()).collect())
        .collect();

    let mut hits: Vec<Value> = vec![];
    for i in 0..normed.len() {
        if i + want.len() > normed.len() {
            break;
        }
        if (0..want.len()).all(|k| normed[i + k] == want[k]) {
            let last = i + want.len() - 1;
            let ctx_a = i.saturating_sub(context);
            let ctx_b = (last + context + 1).min(words.len());
            let ctx = words[ctx_a..ctx_b]
                .iter()
                .map(|w| w.label())
                .collect::<Vec<_>>()
                .join(" ");
            hits.push(json!({
                "index": i,
                "start_us": words[i].start_us,
                "end_us": words[last].end_us,
                "context": ctx,
            }));
        }
        if hits.len() >= 100 {
            break;
        }
    }
    Ok(json!({
        "transcript_id": doc.id.to_string(),
        "query": query,
        "matches": hits.len(),
        "hits": hits,
    }))
}

fn get_catalog(state: &AppState) -> Result<Value, String> {
    let store = state.store.lock().unwrap();
    // families only: the raw list has one entry per style/weight file
    let mut fonts: Vec<String> =
        ue_export::graph::list_system_fonts().into_iter().map(|(family, _)| family).collect();
    fonts.sort();
    fonts.dedup();
    Ok(json!({
        "effects": ue_render::catalog_json(&state.registry.lock().unwrap()),
        "generators": ue_render::generators_catalog_json(&ue_render::core_generators()),
        "fonts": fonts,
        "avatar_setups": store.project.avatars.iter().map(|c| json!({
            "config_id": c.id.to_string(),
            "name": c.name,
            "scale": c.scale,
            "shake_factor": c.shake_factor,
            "model": c.model,
            "expressions": c.expressions.iter().map(|e| json!({
                "name": e.name, "path": e.path,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "text_templates": crate::text_templates(state),
        "subtitle_modes": ["phrase", "word", "karaoke"],
        "transitions": ["core.crossfade"],
    }))
}

/// Every requested change as ONE undo entry. `transform` and `audio` are
/// patches over the clip's current values; `effects` replaces the chain.
fn set_clip_properties(
    state: &AppState,
    app: Option<&tauri::AppHandle>,
    args: &Args,
) -> Result<Value, String> {
    let clip_id = args.id("clip_id")?;
    let mut actions: Vec<ue_core::Action> = vec![];
    let mut changed: Vec<&str> = vec![];
    let mut wants_denoise = false;

    {
        let store = state.store.lock().unwrap();
        let clip = store.project.clip(clip_id).ok_or("clip not found")?;

        if let Some(p) = args.get("transform") {
            let transform = patch_transform(&clip.transform, p)?;
            actions.push(ue_core::Action::SetClipTransform { clip_id, transform });
            changed.push("transform");
        }
        if let Some(p) = args.get("audio") {
            let audio = patch_audio(&clip.audio, p)?;
            wants_denoise = audio.denoise && !clip.audio.denoise;
            actions.push(ue_core::Action::SetClipAudio { clip_id, audio });
            changed.push("audio");
        }
        if let Some(v) = args.get("effects") {
            let effects: Vec<ue_core::model::EffectInstance> =
                serde_json::from_value(v.clone()).map_err(|e| format!("effects: {e}"))?;
            let known = state.registry.lock().unwrap();
            if let Some(bad) = effects
                .iter()
                .find(|e| !known.iter().any(|d| d.id == e.effect_id))
            {
                return Err(format!(
                    "unknown effect '{}' (see get_catalog → effects)",
                    bad.effect_id
                ));
            }
            actions.push(ue_core::Action::SetClipEffects { clip_id, effects });
            changed.push("effects");
        }
        for (key, out) in [("transition_in", false), ("transition_out", true)] {
            let Some(v) = args.get(key) else { continue };
            let transition = if v.is_null() {
                None
            } else {
                let duration = v
                    .get("duration_us")
                    .and_then(|d| d.as_i64())
                    .ok_or_else(|| format!("{key}.duration_us is required"))?;
                if duration <= 0 {
                    return Err(format!("{key}.duration_us must be > 0 (send null to remove)"));
                }
                Some(TransitionRef {
                    effect_id: v
                        .get("effect_id")
                        .and_then(|e| e.as_str())
                        .unwrap_or("core.crossfade")
                        .to_string(),
                    duration,
                    params: Default::default(),
                })
            };
            actions.push(ue_core::Action::SetClipTransition { clip_id, transition, out });
            changed.push(key);
        }
        if let Some(speed) = args.f64("speed") {
            if speed <= 0.0 {
                return Err("speed must be > 0".into());
            }
            // ops computes the new duration and validates the room available
            actions.extend(
                ue_core::ops::set_clip_speed(&store.project, clip_id, speed)
                    .map_err(|e| e.to_string())?,
            );
            changed.push("speed");
        }
    }

    if actions.is_empty() {
        return Err("nothing to change: pass transform, audio, effects, transition_in or speed".into());
    }
    state
        .store
        .lock()
        .unwrap()
        .dispatch("Set clip properties", actions)
        .map_err(|e| e.to_string())?;

    // background job, deliberately outside the transaction above
    if wants_denoise {
        match app {
            Some(app) => crate::spawn_denoise_job(app, state, clip_id),
            None => return Ok(json!({ "changed": changed, "denoise": "export only (no app)" })),
        }
    }
    Ok(json!({ "changed": changed }))
}

/// Payload-specific edits (text, subtitles, generator) as ONE undo entry.
fn set_clip_content(state: &AppState, args: &Args) -> Result<Value, String> {
    use ue_core::model::ClipPayload;
    let clip_id = args.id("clip_id")?;
    let store = state.store.lock().unwrap();
    let clip = store.project.clip(clip_id).ok_or("clip not found")?;

    // captured so we can warn (not fail) if the chosen font isn't installed
    let mut font_used: Option<String> = None;
    let action = match &clip.payload {
        ClipPayload::Text { content, style } => {
            let content =
                args.get("text").and_then(|v| v.as_str()).unwrap_or(content).to_string();
            let style = match args.get("style") {
                Some(p) => patch_style(style, p)?,
                None => style.clone(),
            };
            font_used = Some(style.font.clone());
            ue_core::Action::SetClipText { clip_id, content, style }
        }
        ClipPayload::Subtitles { style, mode, max_words, .. } => {
            let style = match args.get("style") {
                Some(p) => patch_style(style, p)?,
                None => style.clone(),
            };
            font_used = Some(style.font.clone());
            let mode = match args.get("subtitles_mode").and_then(|v| v.as_str()) {
                None => *mode,
                Some("phrase") => SubtitleMode::Phrase,
                Some("word") => SubtitleMode::Word,
                Some("karaoke") => SubtitleMode::Karaoke,
                Some(o) => return Err(format!("unknown subtitles_mode '{o}' (phrase|word|karaoke)")),
            };
            // 0 (or null) = back to fitting the line to the frame width
            let max_words = match args.get("subtitles_max_words") {
                None => *max_words,
                Some(v) if v.is_null() => None,
                Some(v) => v.as_u64().filter(|n| *n > 0).map(|n| n.min(20) as u32),
            };
            ue_core::Action::SetClipSubtitles { clip_id, style, mode, max_words }
        }
        ClipPayload::Generator { generator_id, params, color_params } => {
            let generator_id = args
                .get("generator_id")
                .and_then(|v| v.as_str())
                .unwrap_or(generator_id)
                .to_string();
            if ue_render::find_generator(&ue_render::core_generators(), &generator_id).is_none() {
                return Err(format!("unknown generator '{generator_id}' (see get_catalog)"));
            }
            let params = match args.get("generator_params") {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| format!("generator_params: {e}"))?,
                None => params.clone(),
            };
            let color_params = match args.get("generator_colors") {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| format!("generator_colors: {e}"))?,
                None => color_params.clone(),
            };
            ue_core::Action::SetClipGenerator { clip_id, generator_id, params, color_params }
        }
        ClipPayload::Media { .. } => {
            return Err("a media clip has no editable content; use set_clip_properties".into())
        }
    };
    drop(store);

    state
        .store
        .lock()
        .unwrap()
        .dispatch("Set clip content", vec![action])
        .map_err(|e| e.to_string())?;

    // a font that doesn't resolve draws NOTHING, silently — warn loudly
    if let Some(font) = font_used {
        if !ue_export::graph::font_is_available(&font) {
            return Ok(json!({
                "ok": true,
                "warning": format!(
                    "font '{font}' is not installed and will render as empty text; \
                     pick one from get_catalog.fonts or use 'sans-serif'"
                ),
            }));
        }
    }
    Ok(json!({ "ok": true }))
}

fn set_track_prop(state: &AppState, args: &Args) -> Result<Value, String> {
    use ue_core::action::TrackProp;
    let track_id = args.id("track_id")?;
    let mut props: Vec<TrackProp> = vec![];
    if let Some(v) = args.get("name").and_then(|v| v.as_str()) {
        props.push(TrackProp::Name(v.to_string()));
    }
    if let Some(v) = args.get("muted").and_then(|v| v.as_bool()) {
        props.push(TrackProp::Muted(v));
    }
    if let Some(v) = args.get("solo").and_then(|v| v.as_bool()) {
        props.push(TrackProp::Solo(v));
    }
    if let Some(v) = args.get("locked").and_then(|v| v.as_bool()) {
        props.push(TrackProp::Locked(v));
    }
    if let Some(v) = args.f64("volume_db") {
        props.push(TrackProp::VolumeDb(v as f32));
    }
    match props.len() {
        0 => Err("pass exactly one of: name, muted, solo, locked, volume_db".into()),
        1 => {
            crate::set_track_prop_impl(state, track_id, props.remove(0))?;
            Ok(json!({ "ok": true }))
        }
        _ => Err("pass exactly ONE property per call (they are separate undo entries)".into()),
    }
}

fn remove_sequence(state: &AppState, args: &Args) -> Result<Value, String> {
    let sequence_id = args.id("sequence_id")?;
    let mut store = state.store.lock().unwrap();
    if store.project.sequences.len() <= 1 {
        return Err("cannot delete the last sequence".into());
    }
    if store.project.sequence(sequence_id).is_none() {
        return Err("sequence not found".into());
    }
    let mut actions = vec![];
    // the active sequence cannot be removed: hand the crown over first
    if store.project.active_sequence == sequence_id {
        let fallback = store
            .project
            .sequences
            .iter()
            .find(|s| s.id != sequence_id)
            .map(|s| s.id)
            .ok_or("no remaining sequence")?;
        actions.push(ue_core::Action::SetActiveSequence { sequence_id: fallback });
    }
    actions.push(ue_core::Action::RemoveSequence { sequence_id });
    store.dispatch("Delete sequence", actions).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

fn remove_silences(state: &AppState, args: &Args) -> Result<Value, String> {
    let clip_id = args.id("clip_id")?;
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("delete");
    if !matches!(mode, "delete" | "speedup" | "split") {
        return Err(format!("unknown mode '{mode}' (delete|speedup|split)"));
    }
    let mut params = ue_ai::silence::SilenceParams::default();
    if let Some(db) = args.f64("threshold_db") {
        params.threshold_db = db.clamp(-80.0, -10.0);
    }
    if let Some(ms) = args.get("min_silence_ms").and_then(|v| v.as_i64()) {
        params.min_silence_us = ms.clamp(50, 5000) * 1000;
    }
    if let Some(ms) = args.get("pad_ms").and_then(|v| v.as_i64()) {
        params.pad_pre_us = ms.clamp(0, 1000) * 1000;
        params.pad_post_us = ms.clamp(0, 1000) * 1000;
    }

    let mut store = state.store.lock().unwrap();
    let clip = store.project.clip(clip_id).ok_or("clip not found")?.clone();
    let ue_core::model::ClipPayload::Media { asset_id, src_in, src_out } = clip.payload else {
        return Err("the clip is not media".into());
    };
    let asset = store.project.asset(asset_id).ok_or("asset not found")?;
    let conform = asset
        .audio_conform
        .clone()
        .ok_or("the audio is still being prepared (conform); try again in a few seconds")?;
    let wav =
        ue_audio::wav::WavMap::open(std::path::Path::new(&conform)).map_err(|e| e.to_string())?;
    let ranges =
        ue_ai::silence::clip_silences_on_timeline(&wav, clip.start, src_in, src_out, &params);
    if ranges.is_empty() {
        return Ok(json!({ "removed": 0, "removed_us": 0 }));
    }
    let removed_us: i64 = ranges.iter().map(|(s, e)| e - s).sum();
    let seq_id = store.project.active_sequence;
    match mode {
        "speedup" => store.speedup_ranges(seq_id, &ranges, 4.0).map_err(|e| e.to_string())?,
        "split" => store.split_ranges(seq_id, &ranges).map_err(|e| e.to_string())?,
        _ => store.cut_ranges(seq_id, &ranges, true).map_err(|e| e.to_string())?,
    }
    Ok(json!({ "removed": ranges.len(), "removed_us": removed_us, "mode": mode }))
}

fn replace_words(state: &AppState, args: &Args) -> Result<Value, String> {
    let transcript_id = args.id("transcript_id")?;
    let from = args.str("from")?;
    let to = args.str("to")?;
    let needle = from.trim().to_lowercase();
    if needle.is_empty() {
        return Err("`from` is empty".into());
    }
    let mut store = state.store.lock().unwrap();
    let doc = store
        .project
        .transcripts
        .iter()
        .find(|t| t.id == transcript_id)
        .ok_or("transcript not found")?;
    let display = (!to.trim().is_empty()).then(|| to.trim().to_string());
    let actions: Vec<ue_core::Action> = doc
        .words
        .iter()
        .enumerate()
        .filter(|(_, w)| w.label().trim().to_lowercase() == needle)
        .map(|(index, _)| ue_core::Action::SetWordText {
            transcript_id,
            index,
            display: display.clone(),
        })
        .collect();
    let n = actions.len();
    if n > 0 {
        store.dispatch("Replace words", actions).map_err(|e| e.to_string())?;
    }
    Ok(json!({ "replaced": n }))
}

/// A fully-resolved export request: the timeline snapshot + settings + output.
/// Built synchronously (so bad args fail fast) then run on a job thread.
struct ExportPlan {
    project: ue_core::model::Project,
    seq_id: Id,
    base_dir: std::path::PathBuf,
    path: String,
    settings: ue_export::ExportSettings,
}

impl ExportPlan {
    fn run(self, state: &AppState, job_id: &str) -> Result<Value, String> {
        let cancel = state.export_cancel.clone();
        cancel.store(false, Ordering::SeqCst);
        crate::job_set_output(state, job_id, &self.path);
        let pieces = self.settings.ranges.len();
        // a cancelled JOB must also stop the ffmpeg the export is driving
        let job_cancel = crate::job_cancel_flag(state, job_id);
        let export_cancel = cancel.clone();
        std::thread::spawn(move || {
            while !export_cancel.load(Ordering::SeqCst) {
                if job_cancel.load(Ordering::SeqCst) {
                    export_cancel.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });
        // the progress closure runs synchronously inside this call, so it can
        // borrow state/job_id directly (no escape)
        ue_export::export_sequence_with_progress(
            &self.project,
            self.seq_id,
            &self.base_dir,
            std::path::Path::new(&self.path),
            &self.settings,
            |p| crate::job_progress(state, job_id, p as f64, ""),
            &cancel,
        )
        .map_err(|e| e.to_string())?;
        Ok(json!({ "path": self.path, "pieces": pieces }))
    }
}

fn export_plan(state: &AppState, args: &Args) -> Result<ExportPlan, String> {
    let path = args.str("path")?.to_string();
    let format = match args.get("format").and_then(|v| v.as_str()) {
        None | Some("mp4") => ue_export::ExportFormat::Mp4,
        Some("m4a") => ue_export::ExportFormat::M4a,
        Some("gif") => ue_export::ExportFormat::Gif,
        Some(o) => return Err(format!("unknown format: {o} (mp4|m4a|gif)")),
    };
    let defaults = ue_export::ExportSettings::default();
    let settings = ue_export::ExportSettings {
        format,
        max_height: args.get("max_height").and_then(|v| v.as_u64()).map(|v| v as u32),
        crf: args
            .get("crf")
            .and_then(|v| v.as_u64())
            .map(|v| (v as u8).clamp(10, 40))
            .unwrap_or(defaults.crf),
        loudnorm: args.bool_or("loudnorm", false),
        ranges: args.ranges("ranges"),
        extra_packs: state.user_packs.lock().unwrap().clone(),
        ..defaults
    };
    let store = state.store.lock().unwrap();
    let base_dir = state
        .path
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    Ok(ExportPlan {
        project: store.project.clone(),
        seq_id: store.project.active_sequence,
        base_dir,
        path,
        settings,
    })
}

/// The player is created lazily on the first `play`, so pause/seek/position
/// before that have nothing to talk to.
const NO_PLAYER: &str = "no player yet: call playback {\"action\":\"play\"} first";

fn playback(
    state: &AppState,
    app: Option<&tauri::AppHandle>,
    args: &Args,
) -> Result<Value, String> {
    match args.str("action")? {
        "play" => {
            let from = args.i64_or("from_us", 0);
            crate::playback_play_impl(state, app, from)?;
            Ok(json!({ "playing": true, "from_us": from }))
        }
        "pause" => {
            let guard = state.player.lock().unwrap();
            let p = guard.as_ref().ok_or(NO_PLAYER)?;
            Ok(json!({ "paused_at_us": p.pause() }))
        }
        "seek" => {
            let to = args.i64_or("from_us", 0);
            let guard = state.player.lock().unwrap();
            let p = guard.as_ref().ok_or(NO_PLAYER)?;
            p.seek(to);
            Ok(json!({ "t_us": to }))
        }
        "position" => {
            let guard = state.player.lock().unwrap();
            let p = guard.as_ref().ok_or(NO_PLAYER)?;
            Ok(json!({ "t_us": p.position_us(), "playing": p.is_playing() }))
        }
        other => Err(format!("unknown action: {other} (play|pause|seek|position)")),
    }
}

/// Same placement rules as the UI's add_clip: a compatible unlocked track, and
/// no overlap (a busy `at_us` pushes the clip to the end of the track).
fn add_clip_inner(
    state: &AppState,
    asset_id: Id,
    at_us: i64,
    track_id: Option<Id>,
) -> Result<Id, String> {
    use ue_core::model::{Clip, MediaKind, TrackKind};
    let mut store = state.store.lock().unwrap();
    let asset = store
        .project
        .asset(asset_id)
        .ok_or_else(|| format!("asset {asset_id} does not exist"))?
        .clone();
    let duration = ue_media::default_clip_duration(&asset);
    if duration <= 0 {
        return Err("the file has no usable duration".into());
    }
    let want = if asset.kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video };
    let seq_id = store.project.active_sequence;
    let seq = store.project.sequence(seq_id).ok_or("no active sequence")?;
    let track = match track_id {
        Some(id) => seq
            .tracks
            .iter()
            .find(|t| t.id == id)
            .ok_or("track not found in the active sequence")?,
        None => seq
            .tracks
            .iter()
            .find(|t| t.kind == want && !t.locked)
            .ok_or("no compatible unlocked track; call add_track first")?,
    };
    if track.locked {
        return Err("the track is locked".into());
    }
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
    Ok(clip_id)
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

/// Does this tool change the project (so the running UI must refresh)? The
/// pure reads and the debug-frame dumps do not; everything else might.
fn is_mutation(name: &str) -> bool {
    !matches!(
        name,
        "get_project_summary"
            | "get_timeline"
            | "get_media_pool"
            | "get_transcript"
            | "find_words"
            | "get_catalog"
            | "get_job_status"
            | "list_jobs"
            | "debug_render_frame"
    )
}

/// Processes a JSON-RPC message. `None` = notification with no response.
pub fn handle_rpc(state: &AppState, app: Option<&tauri::AppHandle>, req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    let id = req.get("id").cloned();
    // notifications (no id) carry no response
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
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_defs() }),
        "tools/call" => {
            let name = req.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("");
            let empty = json!({});
            let args = req.pointer("/params/arguments").unwrap_or(&empty);
            ue_core::dlog("mcp", &format!("tool {name} {args}"));
            let out = call_tool(state, app, name, args);
            let failed = out.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
            if failed {
                ue_core::dlog(
                    "mcp",
                    &format!("tool {name} FAILED: {}", out.pointer("/content/0/text").unwrap_or(&Value::Null)),
                );
            } else if let Some(app) = app {
                // an agent's edit must show up in the running editor: nudge the
                // UI to re-fetch, exactly like the in-app commands do. Reads and
                // debug frames change nothing, so they stay quiet.
                if is_mutation(name) {
                    use tauri::Emitter;
                    let _ = app.emit("state-changed", ());
                }
            }
            out
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unsupported method: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Constant-ish check of the `Authorization` header against the session token.
/// Split out of `start` so it can be tested without an HTTP server: an empty
/// or absent header must never authorize, whatever the token is.
pub fn is_authorized(authorization: Option<&str>, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    authorization.map(str::trim) == Some(format!("Bearer {token}").as_str())
}

/// Starts the server on a thread. Returns the port if it could listen.
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
                // authentication: Authorization: Bearer <token>
                let expected = state.mcp_token.lock().unwrap().clone();
                let header = request
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"))
                    .map(|h| h.value.as_str());
                if !is_authorized(header, &expected) {
                    let _ = request.respond(
                        tiny_http::Response::from_string(
                            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"invalid token: use Authorization: Bearer <token> (shown in the app's MCP pill)"}}"#,
                        )
                        .with_status_code(401),
                    );
                    continue;
                }
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let response = match serde_json::from_str::<Value>(&body) {
                    Ok(msg) => handle_rpc(&state, Some(&app), &msg),
                    Err(_) => Some(json!({
                        "jsonrpc": "2.0", "id": Value::Null,
                        "error": { "code": -32700, "message": "invalid JSON" }
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
