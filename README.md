# UberEditor
*[Español](README.es.md)*

Cross-platform desktop video editor (Tauri 2 + Rust + React) built for content creators, with AI superpowers: **text-based editing** (word-by-word Whisper), **silences gone with one click**, **automatic verticals**, **emotion-reactive avatar**, **karaoke subtitles**, and an **embedded MCP server** so an agent (Claude, etc.) can edit your project for you.

**The master plan lives in [PLAN.md](PLAN.md)** — architecture, all 16 features in detail, the Youtubers-toolkit mapping, and the roadmap.

- UI is 100% in English · warm charcoal theme with an amber accent
- Everything is undoable: every operation (including the AI and MCP ones) is **one** undo entry
- Same render engine in preview and export (shared ffmpeg chains): what you see is what you get
- 105 tests (unit, pixel tests over real exports, and 13 visual steps with Playwright)

---

## Requirements

| What | Version | Notes |
|---|---|---|
| **FFmpeg + FFprobe** | ≥ 6 on the `PATH` | The heart of the render. `brew install ffmpeg` / `apt install ffmpeg`. You can point to specific binaries with `UE_FFMPEG` and `UE_FFPROBE` |
| **Rust** | stable | Only for building |
| **Node** | ≥ 20 | Only for building / development |

```bash
npm install
npx tauri dev        # the full desktop app
```

Nothing else to install: the Whisper models download themselves the first time you transcribe (into the app's data folder).

---

## The interface

```
┌────────────┬──────────────────────────────┬─────────────┐
│ Media /    │                              │  Inspector  │
│ Text       │        Preview               │  (of the    │
│ (pool +    │   (real frames + overlays)   │  selected   │
│ transcript)│                              │  clip)      │
├────────────┴──────────────────────────────┴─────────────┤
│  Timeline (V/A tracks, real waveforms and thumbs)       │
├─────────────────────────────────────────────────────────┤
│  Status bar (saved, selection, last action)             │
└─────────────────────────────────────────────────────────┘
```

- **Media**: import with `+ Import` or by dragging files from Finder/Explorer; **double-click** a media item to add it to the timeline.
- **Text**: appears when there are transcripts; from there you edit the video by deleting or moving words.
- **Preview**: real frames from the engine (uses a lightweight proxy if the file is large). When paused, the frame is exact to the export.
- **Inspector**: all the properties of the selected clip; with nothing selected it shows the AI settings (Whisper model/language).

---

## Keyboard shortcuts

| Key | Action |
|---|---|
| `Space` | Play / pause |
| `J` / `K` / `L` | Shuttle: back / pause / forward (repeat doubles: 1→2→4→8×) |
| `S` (or `⌘K`) | Split the clip under the playhead (linked clips split together) |
| `Del` / `Backspace` | Delete selection · with `⇧` it deletes **and closes the gap** (ripple) |
| `←` / `→` | One frame back/forward · with `⇧` ten frames |
| `Home` | Go to 0 |
| `I` / `O` | Mark in / out of the **work range** (amber band on the ruler) |
| `⇧X` | Clear the I–O range |
| `⌘Z` / `⌘⇧Z` | Undo / redo |
| `⌘S` / `⌘O` | Save / open project |
| `Alt` (while dragging) | Disables snapping |

---

## Timeline editing

- **Move**: drag a clip; the **magnet** snaps it to other clips' edges, the playhead, 0, the I–O range, and markers (a dotted guide appears when it snaps; `Alt` disables it).
- **Trim**: drag a clip's **edges** (handles visible when selected).
- **Multi-selection**: drag a **rectangle** over empty area (marquee); `⇧` adds to the selection. The status bar shows how many clips you have so far.
- **Linked clips 🔗**: adding a video with audio creates two clips (video on `V*`, audio on `A*`) that behave as one: split, move, trim, change speed, or delete affects both. `Inspector → Link → Unlink` separates them.
- **Tracks**: `+V` / `+A` add tracks; in the header: `M` mutes, `S` solos, 🔒 locks, **double-click the name** renames, ✕ deletes (undoable), and on audio tracks the **dB is dragged** vertically (double-click → 0 dB).
- **Real multi-layer**: the lowest video track is the base; clips on higher tracks are composited on top (with their position, scale, and opacity) in the export too.
- **Speed**: 0.25×–4× presets in the Inspector. In the export **the voice's pitch is preserved** (atempo); in live playback the pitch changes for now.

## Transform and keyframes

Every clip has **Position X/Y, Opacity, Scale, and Rotation** (and audio, **Gain**). Next to each slider:

- `◇` — the property doesn't animate; click = **create a keyframe** at the playhead (the property becomes animated).
- `◆` — there's a keyframe right at the playhead; click = remove it.
- With the property animated, **moving the slider writes a keyframe** at the playhead (like Premiere/Resolve) and the displayed value is the one at the playhead.
- Below it, the **curve editor** appears: drag the diamonds (time and value), **double-click** adds or removes keys, and when you select a key you choose its interpolation (**linear / step / smooth**).
- The diamonds are also drawn on the selected clip in the timeline.

The animation looks **the same when paused, playing, and in the export** (same curve math on all three paths). Crop exists but isn't animatable from the UI yet.

## Effects (modular packs)

`Inspector → Effects → + Add effect`. Included: **Chroma Key**, Color correction, Gaussian blur, and Vertical: blurred background. Each effect comes from a `manifest.json` (parameters + ffmpeg template), so preview and export use exactly the same chain.

**Your own packs**: create a folder at `«app config»/effects/<my-effect>/manifest.json` and press `↻ packs`. An invalid manifest breaks nothing (it's reported) and a pack with the same `id` as a core one replaces it.

## Generators (shapes and backgrounds)

**▦ Shape** button in the timeline → adds a generated clip:

- **Solid rectangle**: color, width, and height.
- **Gradient**: two colors (diagonal).

You change the type and parameters in `Inspector → Generator`. Since they're normal clips, the full transform applies to them: you can keyframe a panel that slides in, make it semi-transparent behind a title, etc. Same manifest system as the effects (`generators/` folder).

## Text, titles, and templates

- **+ Title** adds text at the playhead (on a free track).
- `Inspector → Text`: content, **system font** (all installed ones), size, color, left/center/right alignment, and position X/Y.
- **Templates**: save a named style and apply it to any title later.
- Everything is burned into the export with the same font and placement you see in the preview.

## Transitions

`Inspector → Transition` (on the clip to the right of the cut): **11 types** (crossfade, wipes, slides, circle, dissolve, pixelize, radial) with configurable duration. The handles extend to both sides, limited by the available material, and they also work between clips at different speeds.

---

## AI

### Transcription (Whisper)

- **T** button on a media item in the pool → transcribes word by word (the model downloads itself). `T✓` = already transcribed.
- **Model and language** are chosen in the Inspector with nothing selected (`AI · Whisper`): tiny/base/small/medium/large-v3-turbo, language auto/es/en/…

### Text-based editing

**Text** tab: the full transcript, with the current word highlighted during playback (click a word = seek).

- Mark words and **✂ Cut** — removes those pieces of the video **on all tracks** and closes the gaps (1 undo).
- **⇢ Move** — reorders a range of material to another point in the timeline (reorders spoken phrases without touching blades).

### Silences

`Inspector → Silences` (clip with audio):

- **🔇 Remove** — cuts the silences and closes the gaps (all tracks, 1 undo).
- **⏩ Speed up 4×** — instead of deleting, it speeds up the silent stretches.
- Sliders for **threshold (dB)**, **minimum duration**, and **margin** around speech.

### Automatic subtitles

**💬** button on a transcribed clip. Three modes in `Inspector → Subtitles`:

- **By phrases** — one line per segment.
- **Word by word** — one big word at a time (shorts style).
- **Karaoke** — the full phrase visible and **each word lights up as it's spoken** (configurable highlight color).

### Automatic vertical (Shorts/Reels)

**📱 Vertical** button → generates a 1080×1920 sequence with a blurred background and the video centered. The sequence selector (next to the timeline buttons) lets you switch back to the horizontal one. Each sequence exports separately.

### Reactive avatar

**🧑‍🎤** button on a transcribed clip → pick the avatar `config.json` (Youtubers-toolkit-compatible format: one looping video per emotion). The avatar appears in the corner, **changes emotion based on what you say** (offline energy/rhythm classifier, or OpenAI-compatible if you set `OPENAI_API_KEY`) and in the export it shakes to the beat of the volume. Visible when paused, playing, and in the export.

---

## Audio

- **Gain** (animatable with keyframes), **Pan** (balance law), in/out **fades** per clip.
- Per-**track** volume (drag the dB in the header).
- **RMS L/R meters** in the transport bar during playback.
- Audio is the **master clock**: the position comes from the frames served to the device (no drift).
- **Background noise removal** per clip (`Audio → Reduce background noise`): Facebook's DNS64
  neural denoiser, rendered in the background — playback and export switch to the exact same
  clean audio when ready. **Self-contained**: on first use the app provisions its own Python
  venv under its data dir (needs a system `python3`/`python` ≥ 3.9; ~200 MB one-time download).
  `UE_DENOISER_PYTHON` points to a custom interpreter (`off` disables); without any Python the
  app falls back to ffmpeg's `afftdn`.

## Export

**Export…** button:

- **Presets**: YouTube 1080p, YouTube 4K, Maximum quality, Quick draft, **Audio only (M4A)**, and **GIF**.
- Adjustable: maximum resolution, CRF quality, codec speed, audio bitrate.
- Optional **R128 normalization** (−14 LUFS, YouTube style).
- **Range I–O**: exports only the range marked with `I`/`O`.
- Live progress on the button and a **Cancel** that cleans up the half-written file.

## Projects

- **`.uep`** format (readable JSON), **portable**: media paths are stored relative to the project — move the whole folder to another disk/machine and open it.
- Media that can't be found stay **offline** (in red) with a **Relink…** button.
- **Autosave**: every minute (if there are changes) a `.uep.autosave` copy is written; if the app dies, on startup it offers to **recover**. A real save invalidates it.
- The caches (conformed audio, proxies, waveforms, thumbnails) live outside the project, indexed by content hash: they regenerate themselves on another machine.

---

## MCP server (agent-based editing)

On startup, the app brings up an MCP server at `http://127.0.0.1:4599/mcp` (loopback only) **protected with a token** (generated at startup; find it in the «MCP» pill in the header, which includes the connection command ready to copy):

```bash
claude mcp add --transport http ubereditor http://127.0.0.1:4599/mcp \
  --header "Authorization: Bearer <token>"
```

**51 tools — everything the UI can do, an agent can do**: import, relink and transcribe media, cut, trim, move and split clips, animate transforms with keyframes, apply effects and transitions, add titles, shapes and subtitles, manage tracks and sequences, remove silences, fix transcription errors, search the transcript, name clips, generate the reactive avatar, render (including multi-piece exports), and save the project. Every edit an agent makes is **one** undo entry in the UI, and a call that would break the project fails without changing anything.

Slow operations (transcribe, export, avatar) run as **background jobs**: the tool returns a `job_id` immediately and the agent polls `get_job_status`, so a client timeout is never mistaken for a failure. Three debugging tools (`debug_render_frame`, `debug_playback_frame`, `playback`) let an agent *see* what the editor is showing — the paused preview now includes titles and subtitles, composited exactly like the export — so it can verify a frame or reproduce a visual bug instead of guessing.

📖 **[Full reference: `docs/MCP.md`](docs/MCP.md)** — the five rules (µs everywhere, one call = one undo…), a typical session, every tool, and how to add one.

---

## Development

```bash
cargo test                    # the whole Rust suite (unit + pixel over real ffmpeg)
cargo clippy --workspace --all-targets

npm run dev                   # UI in the browser with the MOCK engine (http://localhost:5175)
npm run typecheck

npx tauri dev                 # the real app

npm run screenshot            # visual tests: 13 steps with functional assertions
                              #   → screenshots/<date>/*.png (starts vite only if needed)
```

### Architecture (crates)

| Crate | What it does |
|---|---|
| `ue-core` | Pure model, actions with mechanical inverses, transactional history, keyframe curves |
| `ue-media` | ffprobe, hashing, preview frames (MJPEG), proxies, thumbnails, audio conforming |
| `ue-audio` | mmap'd WAV, pure mixer (testable), cpal output (master clock), peaks |
| `ue-render` | Effect and generator packs (manifest → ffmpeg chain), transform and animation expressions |
| `ue-export` | EDL, ffmpeg graph (multi-layer, transitions, text/karaoke, avatar), progress and cancellation |
| `ue-ai` | Silence detection (hysteresis + padding), emotion classification |
| `ue-whisper` | whisper-rs (Metal/CUDA), per-word timestamps, model downloads |
| `src-tauri` | IPC commands, FrameService, MCP server, autosave |

Time convention: microseconds (`i64`) throughout the model; rational fps; ULID ids. The frontend mirrors the serde types by hand (`src/engine/types.ts`) and has two interchangeable engines: `TauriEngine` (real IPC) and `MockEngine` (browser demo).
