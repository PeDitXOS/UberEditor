import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { MEDIA_EXTENSIONS } from "../lib/media";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";

import type { EngineClient, ExportUiSettings } from "./client";
import type {
  AudioProps,
  AvatarConfig,
  EffectDef,
  EffectInstance,
  Id,
  GeneratorDef,
  Param,
  StateSnapshot,
  TextStyle,
  ThumbStrip,
  TimeUs,
  Transform2D,
  TransitionRef,
  TtsCatalog,
} from "./types";

export function isTauri(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

/** The backend takes integer µs (i64); UI math (px→time) produces floats. */
const us = (t: TimeUs): TimeUs => Math.round(t);

export class TauriEngine implements EngineClient {
  readonly kind = "tauri" as const;

  getState(): Promise<StateSnapshot> {
    return invoke("get_state");
  }
  splitClip(clipId: Id, tUs: TimeUs): Promise<StateSnapshot> {
    return invoke("split_clip", { clipId, tUs: us(tUs) });
  }
  deleteClips(ids: Id[], ripple: boolean): Promise<StateSnapshot> {
    return invoke("delete_clips", { ids, ripple });
  }
  moveClip(clipId: Id, toTrack: Id, toStartUs: TimeUs): Promise<StateSnapshot> {
    // overwrite-move exists only as an MCP capability; the UI always collision-checks
    return invoke("move_clip", { clipId, toTrack, toStartUs: us(toStartUs), overwrite: false });
  }
  trimClip(clipId: Id, left: boolean, newEdgeUs: TimeUs): Promise<StateSnapshot> {
    return invoke("trim_clip", { clipId, left, newEdgeUs: us(newEdgeUs) });
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
      title: "Import media",
      filters: [
        {
          name: "Media",
          extensions: [...MEDIA_EXTENSIONS],
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
    return invoke("add_clip", { assetId, atUs: us(atUs) });
  }

  async renderFrame(tUs: TimeUs, maxWidth: number): Promise<Uint8Array | null> {
    const buf = await invoke<ArrayBuffer>("render_frame", { tUs: us(tUs), maxWidth });
    const bytes = new Uint8Array(buf);
    return bytes.length > 0 ? bytes : null;
  }

  async resolveAssetUrl(assetId: Id): Promise<string | null> {
    const path = await invoke<string | null>("resolve_asset_path", { assetId });
    return path ? convertFileSrc(path) : null;
  }

  async renderAssetFrame(assetId: Id, srcUs: TimeUs, maxWidth: number): Promise<Uint8Array | null> {
    const buf = await invoke<ArrayBuffer>("render_asset_frame", {
      assetId,
      srcUs: us(srcUs),
      maxWidth,
    });
    const bytes = new Uint8Array(buf);
    return bytes.length > 0 ? bytes : null;
  }

  async pickSavePath(defaultName: string, extension = "mp4"): Promise<string | null> {
    return save({
      title: "Export",
      defaultPath: defaultName,
      filters: [{ name: extension.toUpperCase(), extensions: [extension] }],
    });
  }

  exportVideo(path: string, settings?: ExportUiSettings): Promise<string> {
    return invoke("export_video", {
      path,
      maxHeight: settings?.maxHeight ?? null,
      crf: settings?.crf ?? null,
      preset: settings?.preset ?? null,
      audioBitrateK: settings?.audioBitrateK ?? null,
      loudnorm: settings?.loudnorm ?? null,
      rangeInUs: settings?.rangeInUs != null ? us(settings.rangeInUs) : null,
      rangeOutUs: settings?.rangeOutUs != null ? us(settings.rangeOutUs) : null,
      ranges: settings?.ranges?.map(([a, b]) => [us(a), us(b)]) ?? null,
      format: settings?.format ?? null,
    });
  }

  playbackPlay(fromUs: TimeUs): Promise<void> {
    return invoke("playback_play", { fromUs: us(fromUs) });
  }
  playbackPause(): Promise<TimeUs> {
    return invoke("playback_pause");
  }
  playbackSeek(tUs: TimeUs): Promise<void> {
    return invoke("playback_seek", { tUs: us(tUs) });
  }
  playbackSetRate(rate: number, fromUs: TimeUs): Promise<void> {
    return invoke("playback_set_rate", { rate, fromUs: us(fromUs) });
  }
  uiLog(level: "error" | "warn" | "info", message: string): void {
    void invoke("ui_log", { level, message }).catch(() => {});
  }
  checkRecovery(): Promise<string | null> {
    return invoke("check_recovery", { path: null });
  }
  recoverProject(autosave: string, original: string | null): Promise<StateSnapshot> {
    return invoke("recover_project", { autosave, original });
  }
  discardRecovery(): Promise<void> {
    return invoke("discard_recovery");
  }
  getAudioPeaks(assetId: Id): Promise<number[] | null> {
    return invoke("get_audio_peaks", { assetId });
  }
  async ensureThumbs(assetId: Id): Promise<ThumbStrip | null> {
    return invoke("ensure_thumbs", { assetId });
  }
  async getThumbStrip(assetId: Id): Promise<Uint8Array | null> {
    const buf = await invoke<ArrayBuffer>("get_thumb_strip", { assetId });
    const bytes = new Uint8Array(buf);
    return bytes.length > 0 ? bytes : null;
  }
  playbackPosition(): Promise<[TimeUs, boolean, number, number]> {
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
      title: "Save project",
      defaultPath: defaultName,
      filters: [{ name: "OpenInCut project", extensions: ["uep"] }],
    });
  }
  async pickProjectOpenPath(): Promise<string | null> {
    const picked = await open({
      title: "Open project",
      multiple: false,
      filters: [{ name: "OpenInCut project", extensions: ["uep"] }],
    });
    return typeof picked === "string" ? picked : null;
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
  setClipTransition(
    clipId: Id,
    transition: TransitionRef | null,
    out?: boolean,
  ): Promise<StateSnapshot> {
    return invoke("set_clip_transition", { clipId, transition, out: out ?? false });
  }
  addTextClip(content: string, atUs: TimeUs): Promise<StateSnapshot> {
    return invoke("add_text_clip", { content, atUs: us(atUs) });
  }
  setClipText(clipId: Id, content: string, style: TextStyle): Promise<StateSnapshot> {
    return invoke("set_clip_text", { clipId, content, style });
  }
  setSubtitlesProps(
    clipId: Id,
    style: TextStyle,
    mode: "phrase" | "word" | "karaoke",
    maxWords: number | null,
  ): Promise<StateSnapshot> {
    return invoke("set_subtitles_props", { clipId, style, mode, maxWords });
  }
  listFonts(): Promise<[string, string][]> {
    return invoke("list_fonts");
  }
  listTextTemplates(): Promise<Record<string, TextStyle>> {
    return invoke("list_text_templates");
  }
  saveTextTemplate(name: string, style: TextStyle): Promise<Record<string, TextStyle>> {
    return invoke("save_text_template", { name, style });
  }

  relinkAsset(assetId: Id, newPath: string): Promise<StateSnapshot> {
    return invoke("relink_asset", { assetId, newPath });
  }
  newProject(name: string): Promise<StateSnapshot> {
    return invoke("new_project", { name });
  }

  unlinkClip(clipId: Id): Promise<StateSnapshot> {
    return invoke("unlink_clip", { clipId });
  }
  getGenerators(): Promise<GeneratorDef[]> {
    return invoke("get_generators");
  }
  addGeneratorClip(generatorId: string, atUs: TimeUs): Promise<StateSnapshot> {
    return invoke("add_generator_clip", { generatorId, atUs: us(atUs) });
  }
  setClipGenerator(
    clipId: Id,
    generatorId: string,
    params: Record<string, Param>,
    colorParams: Record<string, string>,
  ): Promise<StateSnapshot> {
    return invoke("set_clip_generator", { clipId, generatorId, params, colorParams });
  }
  removeSequence(sequenceId: Id): Promise<StateSnapshot> {
    return invoke("remove_sequence", { sequenceId });
  }
  setWordText(transcriptId: Id, index: number, text: string): Promise<StateSnapshot> {
    return invoke("set_word_text", { transcriptId, index, text });
  }
  replaceWords(
    transcriptId: Id,
    from: string,
    to: string,
  ): Promise<{ replaced: number; snapshot: StateSnapshot }> {
    return invoke("replace_words", { transcriptId, from, to });
  }
  setSequenceProps(
    sequenceId: Id,
    width: number,
    height: number,
    fpsNum: number,
    fpsDen: number,
  ): Promise<StateSnapshot> {
    return invoke("set_sequence_props", { sequenceId, width, height, fpsNum, fpsDen });
  }
  addTrack(kind: "video" | "audio"): Promise<StateSnapshot> {
    return invoke("add_track", { kind });
  }
  removeTrack(trackId: Id): Promise<StateSnapshot> {
    return invoke("remove_track", { trackId });
  }
  renameTrack(trackId: Id, name: string): Promise<StateSnapshot> {
    return invoke("rename_track", { trackId, name });
  }
  setTrackVolume(trackId: Id, db: number): Promise<StateSnapshot> {
    return invoke("set_track_volume", { trackId, db });
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
    return invoke("move_range", { sequenceId, fromUs: us(fromUs), toUs: us(toUs), destUs: us(destUs) });
  }

  cutRanges(
    sequenceId: Id,
    ranges: [TimeUs, TimeUs][],
    ripple: boolean,
  ): Promise<StateSnapshot> {
    return invoke("cut_ranges", {
      sequenceId,
      ranges: ranges.map(([a, b]) => [us(a), us(b)]),
      ripple,
    });
  }

  saveAvatarConfig(config: AvatarConfig): Promise<[Id, StateSnapshot]> {
    return invoke("save_avatar_config", { config });
  }
  removeAvatarConfig(configId: Id): Promise<StateSnapshot> {
    return invoke("remove_avatar_config", { configId });
  }
  pickAvatarMedia(): Promise<string[]> {
    return invoke("pick_avatar_media");
  }
  exportAvatarConfig(configId: Id, path: string): Promise<string> {
    return invoke("export_avatar_config", { configId, path });
  }
  importAvatarConfig(path: string): Promise<[Id, StateSnapshot]> {
    return invoke("import_avatar_config", { path });
  }
  generateAvatarVideo(configId: Id, driverAsset: Id): Promise<void> {
    return invoke("generate_avatar_video", { configId, driverAsset });
  }
  async onAvatarProgress(
    cb: (p: { stage: string; progress: number; message: string }) => void,
  ): Promise<() => void> {
    return listen<{ stage: string; progress: number; message: string }>("avatar-progress", (e) =>
      cb(e.payload),
    );
  }
  listTtsVoices(): Promise<TtsCatalog> {
    return invoke("list_tts_voices");
  }
  generateSpeech(
    text: string,
    engine: string,
    voice: string | null,
    rate: number | null,
    atUs: TimeUs | null,
  ): Promise<void> {
    return invoke("generate_speech", {
      text,
      engine,
      voice,
      rate,
      atUs: atUs === null ? null : us(atUs),
    });
  }
  async onTtsProgress(
    cb: (p: { stage: string; progress: number; message: string }) => void,
  ): Promise<() => void> {
    return listen<{ stage: string; progress: number; message: string }>("tts-progress", (e) =>
      cb(e.payload),
    );
  }
  denoiseStatus(): Promise<[boolean, string]> {
    return invoke("denoise_status");
  }
  async pickJsonSavePath(defaultName: string): Promise<string | null> {
    return save({
      title: "Export avatar setup",
      defaultPath: defaultName,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
  }
  async pickJsonOpenPath(): Promise<string | null> {
    const picked = await open({
      title: "Import avatar setup",
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

  transcribeAsset(assetId: Id): Promise<void> {
    // the whisper model comes from project settings (set_project_settings)
    return invoke("transcribe_asset", { assetId, model: null });
  }

  setClipSpeed(clipId: Id, speed: number): Promise<StateSnapshot> {
    return invoke("set_clip_speed", { clipId, speed });
  }

  removeSilences(
    clipId: Id,
    mode: "delete" | "speedup" | "split",
    params?: { thresholdDb?: number; minSilenceMs?: number; padMs?: number },
  ): Promise<{ removed: number; removed_us: number; snapshot: StateSnapshot }> {
    return invoke("remove_silences", {
      clipId,
      mode,
      thresholdDb: params?.thresholdDb ?? null,
      minSilenceMs: params?.minSilenceMs ?? null,
      padMs: params?.padMs ?? null,
    });
  }

  mcpStatus(): Promise<[number, string] | null> {
    return invoke("mcp_status");
  }
  mcpCall(toolName: string, args: Record<string, unknown>): Promise<unknown> {
    return invoke("mcp_call", { toolName, arguments: args });
  }
  mcpListTools(): Promise<unknown> {
    return invoke("mcp_list_tools");
  }
  setProjectSettings(whisperLanguage: string, whisperModel: string): Promise<StateSnapshot> {
    return invoke("set_project_settings", { whisperLanguage, whisperModel });
  }

  cancelExport(): Promise<void> {
    return invoke("cancel_export");
  }
  async onExportProgress(cb: (progress: number) => void): Promise<() => void> {
    return listen<number>("export-progress", (e) => cb(e.payload));
  }
}
