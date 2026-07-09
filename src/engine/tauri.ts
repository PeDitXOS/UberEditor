import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";

import type { EngineClient } from "./client";
import type {
  AudioProps,
  EffectDef,
  EffectInstance,
  Id,
  StateSnapshot,
  TextStyle,
  TimeUs,
  Transform2D,
  TransitionRef,
} from "./types";

export function isTauri(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

export class TauriEngine implements EngineClient {
  readonly kind = "tauri" as const;

  getState(): Promise<StateSnapshot> {
    return invoke("get_state");
  }
  splitClip(clipId: Id, tUs: TimeUs): Promise<StateSnapshot> {
    return invoke("split_clip", { clipId, tUs });
  }
  deleteClips(ids: Id[], ripple: boolean): Promise<StateSnapshot> {
    return invoke("delete_clips", { ids, ripple });
  }
  moveClip(
    clipId: Id,
    toTrack: Id,
    toStartUs: TimeUs,
    overwrite: boolean,
  ): Promise<StateSnapshot> {
    return invoke("move_clip", { clipId, toTrack, toStartUs, overwrite });
  }
  trimClip(clipId: Id, left: boolean, newEdgeUs: TimeUs): Promise<StateSnapshot> {
    return invoke("trim_clip", { clipId, left, newEdgeUs });
  }
  undo(): Promise<StateSnapshot> {
    return invoke("undo");
  }
  redo(): Promise<StateSnapshot> {
    return invoke("redo");
  }
  setClipAudio(clipId: Id, audio: AudioProps): Promise<StateSnapshot> {
    return invoke("set_clip_audio", { clipId, audio });
  }
  setClipTransform(clipId: Id, transform: Transform2D): Promise<StateSnapshot> {
    return invoke("set_clip_transform", { clipId, transform });
  }

  async pickMediaFiles(): Promise<string[] | null> {
    const picked = await open({
      multiple: true,
      title: "Importar medios",
      filters: [
        {
          name: "Medios",
          extensions: [
            "mp4", "mov", "mkv", "webm", "avi", "m4v", "mts", "mpg",
            "wav", "mp3", "m4a", "aac", "flac", "ogg", "aiff",
            "png", "jpg", "jpeg", "webp", "bmp", "tiff", "gif",
          ],
        },
      ],
    });
    if (!picked) return null;
    return Array.isArray(picked) ? picked : [picked];
  }

  importMedia(paths: string[]): Promise<StateSnapshot> {
    return invoke("import_media", { paths });
  }
  addClip(assetId: Id, atUs: TimeUs): Promise<StateSnapshot> {
    return invoke("add_clip", { assetId, atUs });
  }

  async renderFrame(tUs: TimeUs, maxWidth: number): Promise<Uint8Array | null> {
    const buf = await invoke<ArrayBuffer>("render_frame", { tUs, maxWidth });
    const bytes = new Uint8Array(buf);
    return bytes.length > 0 ? bytes : null;
  }

  async pickSavePath(defaultName: string): Promise<string | null> {
    return save({
      title: "Exportar video",
      defaultPath: defaultName,
      filters: [{ name: "MP4", extensions: ["mp4"] }],
    });
  }

  exportVideo(path: string): Promise<string> {
    return invoke("export_video", { path, maxHeight: null });
  }

  playbackPlay(fromUs: TimeUs): Promise<void> {
    return invoke("playback_play", { fromUs });
  }
  playbackPause(): Promise<TimeUs> {
    return invoke("playback_pause");
  }
  playbackSeek(tUs: TimeUs): Promise<void> {
    return invoke("playback_seek", { tUs });
  }
  playbackPosition(): Promise<[TimeUs, boolean]> {
    return invoke("playback_position");
  }

  async onStateChanged(cb: () => void): Promise<() => void> {
    return listen("state-changed", cb);
  }

  saveProject(path: string | null): Promise<string> {
    return invoke("save_project", { path });
  }
  openProject(path: string): Promise<StateSnapshot> {
    return invoke("open_project", { path });
  }
  async pickProjectSavePath(defaultName: string): Promise<string | null> {
    return save({
      title: "Guardar proyecto",
      defaultPath: defaultName,
      filters: [{ name: "Proyecto UberEditor", extensions: ["uep"] }],
    });
  }
  async pickProjectOpenPath(): Promise<string | null> {
    const picked = await open({
      title: "Abrir proyecto",
      multiple: false,
      filters: [{ name: "Proyecto UberEditor", extensions: ["uep"] }],
    });
    return typeof picked === "string" ? picked : null;
  }

  async playbackFrame(): Promise<Uint8Array | null> {
    const buf = await invoke<ArrayBuffer>("playback_frame");
    const bytes = new Uint8Array(buf);
    return bytes.length > 0 ? bytes : null;
  }

  getEffectsCatalog(): Promise<EffectDef[]> {
    return invoke("get_effects_catalog");
  }
  reloadEffectPacks(): Promise<{ catalog: EffectDef[]; errors: string[]; dir: string | null }> {
    return invoke("reload_effect_packs");
  }
  setClipEffects(clipId: Id, effects: EffectInstance[]): Promise<StateSnapshot> {
    return invoke("set_clip_effects", { clipId, effects });
  }
  setClipTransition(clipId: Id, transition: TransitionRef | null): Promise<StateSnapshot> {
    return invoke("set_clip_transition", { clipId, transition });
  }
  addTextClip(content: string, atUs: TimeUs): Promise<StateSnapshot> {
    return invoke("add_text_clip", { content, atUs });
  }
  setClipText(clipId: Id, content: string, style: TextStyle): Promise<StateSnapshot> {
    return invoke("set_clip_text", { clipId, content, style });
  }
  setSubtitlesProps(
    clipId: Id,
    style: TextStyle,
    mode: "phrase" | "word" | "karaoke",
  ): Promise<StateSnapshot> {
    return invoke("set_subtitles_props", { clipId, style, mode });
  }
  setTrackProp(
    trackId: Id,
    prop: "muted" | "solo" | "locked",
    value: boolean,
  ): Promise<StateSnapshot> {
    return invoke("set_track_prop", { trackId, prop, value });
  }

  moveRange(
    sequenceId: Id,
    fromUs: TimeUs,
    toUs: TimeUs,
    destUs: TimeUs,
  ): Promise<StateSnapshot> {
    return invoke("move_range", { sequenceId, fromUs, toUs, destUs });
  }

  cutRanges(
    sequenceId: Id,
    ranges: [TimeUs, TimeUs][],
    ripple: boolean,
  ): Promise<StateSnapshot> {
    return invoke("cut_ranges", { sequenceId, ranges, ripple });
  }

  addAvatarClip(clipId: Id, configPath: string): Promise<StateSnapshot> {
    return invoke("add_avatar_clip", { clipId, configPath });
  }
  async pickAvatarConfig(): Promise<string | null> {
    const picked = await open({
      title: "Config de avatares (config.json del toolkit)",
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    return typeof picked === "string" ? picked : null;
  }

  generateVertical(): Promise<StateSnapshot> {
    return invoke("generate_vertical");
  }
  setActiveSequence(sequenceId: Id): Promise<StateSnapshot> {
    return invoke("set_active_sequence", { sequenceId });
  }

  addSubtitlesClip(clipId: Id): Promise<StateSnapshot> {
    return invoke("add_subtitles_clip", { clipId });
  }

  transcribeAsset(assetId: Id, model?: string): Promise<void> {
    return invoke("transcribe_asset", { assetId, model: model ?? null });
  }

  setClipSpeed(clipId: Id, speed: number): Promise<StateSnapshot> {
    return invoke("set_clip_speed", { clipId, speed });
  }

  removeSilences(
    clipId: Id,
    mode: "delete" | "speedup",
  ): Promise<{ removed: number; removed_us: number; snapshot: StateSnapshot }> {
    return invoke("remove_silences", { clipId, mode });
  }

  mcpStatus(): Promise<number | null> {
    return invoke("mcp_status");
  }

  cancelExport(): Promise<void> {
    return invoke("cancel_export");
  }
  async onExportProgress(cb: (progress: number) => void): Promise<() => void> {
    return listen<number>("export-progress", (e) => cb(e.payload));
  }
}
