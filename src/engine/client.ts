import type {
  AudioProps,
  EffectDef,
  EffectInstance,
  Id,
  AvatarConfig,
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

/**
 * Editing engine contract. Two implementations:
 * - TauriEngine: the real desktop app (ue-core via IPC).
 * - MockEngine: browser, for UI development and visual checks.
 */
/** Export options chosen in the dialog (a subset of ExportSettings). */
export interface ExportUiSettings {
  /** Output container. */
  format: "mp4" | "m4a" | "gif";
  maxHeight: number | null;
  crf: number;
  preset: string;
  audioBitrateK: number;
  loudnorm: boolean;
  /** I-O range in µs, or null to export everything. */
  rangeInUs: number | null;
  rangeOutUs: number | null;
  /** Several pieces of the master concatenated in order (overrides the range). */
  ranges?: [number, number][];
}

export interface EngineClient {
  readonly kind: "tauri" | "mock";

  getState(): Promise<StateSnapshot>;
  splitClip(clipId: Id, tUs: TimeUs): Promise<StateSnapshot>;
  deleteClips(ids: Id[], ripple: boolean): Promise<StateSnapshot>;
  moveClip(clipId: Id, toTrack: Id, toStartUs: TimeUs): Promise<StateSnapshot>;
  trimClip(clipId: Id, left: boolean, newEdgeUs: TimeUs): Promise<StateSnapshot>;
  undo(): Promise<StateSnapshot>;
  redo(): Promise<StateSnapshot>;
  setClipAudio(clipId: Id, audio: AudioProps): Promise<StateSnapshot>;
  setClipTransform(clipId: Id, transform: Transform2D): Promise<StateSnapshot>;

  /** Native file picker dialog (null if not available). */
  pickMediaFiles(): Promise<string[] | null>;
  importMedia(paths: string[]): Promise<StateSnapshot>;
  /** Adds a clip from the asset to the timeline (playhead or end of track). */
  addClip(assetId: Id, atUs: TimeUs): Promise<StateSnapshot>;

  /** Real JPEG frame at the given time, or null if there is no signal / unsupported. */
  renderFrame(tUs: TimeUs, maxWidth: number): Promise<Uint8Array | null>;

  /**
   * A URL the webview can load this asset's media from (asset protocol on
   * desktop). Used by the frontend preview compositor to decode video/image
   * frames natively. Returns null when unavailable (mock/browser).
   */
  resolveAssetUrl(assetId: Id): Promise<string | null>;

  /**
   * One decoded source frame of an asset at `srcUs` as PNG bytes (alpha
   * preserved), rendered by the backend with ffmpeg. Fallback for codecs the
   * webview cannot decode (e.g. the generated avatar's qtrle .mov).
   */
  renderAssetFrame(assetId: Id, srcUs: TimeUs, maxWidth: number): Promise<Uint8Array | null>;

  /** Native "save as" dialog (null if the user cancels or it's unsupported). */
  pickSavePath(defaultName: string, extension?: string): Promise<string | null>;
  /** Exports the active sequence to MP4. Returns the written path. */
  exportVideo(path: string, settings?: ExportUiSettings): Promise<string>;

  // -- transport with audio as the master clock (desktop only) --
  /** Starts audio playback from `fromUs`. Rejects if there is no device. */
  playbackPlay(fromUs: TimeUs): Promise<void>;
  /** Pauses and returns the exact position of the audio clock. */
  playbackPause(): Promise<TimeUs>;
  playbackSeek(tUs: TimeUs): Promise<void>;
  /** [position µs, playing] according to the audio clock. */
  /** Path of the autosave newer than the project, or null. */
  /** UI log bridge: errors land in the dev terminal (no-op in the browser). */
  uiLog(level: "error" | "warn" | "info", message: string): void;
  checkRecovery(): Promise<string | null>;
  /** Loads the recovery copy (keeps the real project's path). */
  recoverProject(autosave: string, original: string | null): Promise<StateSnapshot>;
  /** Discards the active recovery copy. */
  discardRecovery(): Promise<void>;
  /** Real audio peaks (25 bins/s) for the asset, or null if not applicable. */
  getAudioPeaks(assetId: Id): Promise<number[] | null>;
  /** Generates/retrieves the asset's thumbnail strip (desktop only). */
  ensureThumbs(assetId: Id): Promise<ThumbStrip | null>;
  /** JPEG bytes of the already-generated strip. */
  getThumbStrip(assetId: Id): Promise<Uint8Array | null>;
  /** JKL shuttle: signed rate; starts from fromUs if it was paused. */
  playbackSetRate(rate: number, fromUs: TimeUs): Promise<void>;
  /** (position, playing, RMS left 0..1, RMS right 0..1). */
  playbackPosition(): Promise<[TimeUs, boolean, number, number]>;

  /** Subscription to state changes originating in the backend (jobs). */
  onStateChanged(cb: () => void): Promise<() => void>;

  // -- project on disk (desktop only) --
  /** Saves to the given path (or the last one used if null). Returns the path. */
  saveProject(path: string | null): Promise<string>;
  openProject(path: string): Promise<StateSnapshot>;
  pickProjectSavePath(defaultName: string): Promise<string | null>;
  pickProjectOpenPath(): Promise<string | null>;


  // -- modular effects --
  getEffectsCatalog(): Promise<EffectDef[]>;
  /** Reloads user packs from disk; returns the updated catalog. */
  reloadEffectPacks(): Promise<{ catalog: EffectDef[]; errors: string[]; dir: string | null }>;
  setClipEffects(clipId: Id, effects: EffectInstance[]): Promise<StateSnapshot>;
  /** `out` = the EXIT transition (clip tail); default is the entrance. */
  setClipTransition(
    clipId: Id,
    transition: TransitionRef | null,
    out?: boolean,
  ): Promise<StateSnapshot>;
  /** Adds a title clip on the top video track. */
  addTextClip(content: string, atUs: TimeUs): Promise<StateSnapshot>;
  setClipText(clipId: Id, content: string, style: TextStyle): Promise<StateSnapshot>;
  setSubtitlesProps(
    clipId: Id,
    style: TextStyle,
    mode: "phrase" | "word" | "karaoke",
    maxWords: number | null,
  ): Promise<StateSnapshot>;
  /** System fonts (family, path) for the text picker. */
  listFonts(): Promise<[string, string][]>;
  listTextTemplates(): Promise<Record<string, TextStyle>>;
  saveTextTemplate(name: string, style: TextStyle): Promise<Record<string, TextStyle>>;

  /** Relinks an offline media with a new path. */
  relinkAsset(assetId: Id, newPath: string): Promise<StateSnapshot>;
  /** Creates a new empty project. */
  newProject(name: string): Promise<StateSnapshot>;

  /** Breaks the video↔audio link of the clip's group. */
  unlinkClip(clipId: Id): Promise<StateSnapshot>;
  /** Generator catalog (manifests). */
  getGenerators(): Promise<GeneratorDef[]>;
  addGeneratorClip(generatorId: string, atUs: TimeUs): Promise<StateSnapshot>;
  setClipGenerator(
    clipId: Id,
    generatorId: string,
    params: Record<string, Param>,
    colorParams: Record<string, string>,
  ): Promise<StateSnapshot>;
  addTrack(kind: "video" | "audio"): Promise<StateSnapshot>;
  /** Delete a sequence (never the last one; switches away if active). */
  removeSequence(sequenceId: Id): Promise<StateSnapshot>;
  /** Correct one transcribed word (empty = back to the original). */
  setWordText(transcriptId: Id, index: number, text: string): Promise<StateSnapshot>;
  /** Replace every whole-word occurrence in a transcript. */
  replaceWords(
    transcriptId: Id,
    from: string,
    to: string,
  ): Promise<{ replaced: number; snapshot: StateSnapshot }>;
  setSequenceProps(
    sequenceId: Id,
    width: number,
    height: number,
    fpsNum: number,
    fpsDen: number,
  ): Promise<StateSnapshot>;
  removeTrack(trackId: Id): Promise<StateSnapshot>;
  renameTrack(trackId: Id, name: string): Promise<StateSnapshot>;
  setTrackVolume(trackId: Id, db: number): Promise<StateSnapshot>;
  setTrackProp(
    trackId: Id,
    prop: "muted" | "solo" | "locked",
    value: boolean,
  ): Promise<StateSnapshot>;

  /** Moves a timeline range to another point (reorder material). */
  moveRange(
    sequenceId: Id,
    fromUs: TimeUs,
    toUs: TimeUs,
    destUs: TimeUs,
  ): Promise<StateSnapshot>;

  /** Cuts timeline ranges across all tracks (optional ripple). */
  cutRanges(
    sequenceId: Id,
    ranges: [TimeUs, TimeUs][],
    ripple: boolean,
  ): Promise<StateSnapshot>;

  /** Returns the saved id (a new draft gets one) plus the snapshot. */
  saveAvatarConfig(config: AvatarConfig): Promise<[Id, StateSnapshot]>;
  removeAvatarConfig(configId: Id): Promise<StateSnapshot>;
  pickAvatarMedia(): Promise<string[]>;
  exportAvatarConfig(configId: Id, path: string): Promise<string>;
  /** Returns the imported id plus the snapshot. */
  importAvatarConfig(path: string): Promise<[Id, StateSnapshot]>;
  /** Background render; progress arrives via onAvatarProgress. */
  generateAvatarVideo(configId: Id, driverAsset: Id): Promise<void>;
  onAvatarProgress(
    cb: (p: { stage: string; progress: number; message: string }) => void,
  ): Promise<() => void>;
  pickJsonSavePath(defaultName: string): Promise<string | null>;
  pickJsonOpenPath(): Promise<string | null>;

  // -- TTS voiceover (speech from text) --
  /** Voice catalog + engine availability for the voiceover dialog. */
  listTtsVoices(): Promise<TtsCatalog>;
  /**
   * Background synthesis (say | kokoro); progress arrives via onTtsProgress
   * and the result lands in Media. `atUs` also drops the clip there.
   * `rate`: words/min for say (90–400), speed multiplier for kokoro (0.5–2).
   */
  generateSpeech(
    text: string,
    engine: string,
    voice: string | null,
    rate: number | null,
    atUs: TimeUs | null,
  ): Promise<void>;
  onTtsProgress(
    cb: (p: { stage: string; progress: number; message: string }) => void,
  ): Promise<() => void>;
  /**
   * Whether the NEURAL denoiser (DNS64) can run, plus a human hint. The
   * Inspector disables the Denoise toggle and shows the hint when it cannot.
   */
  denoiseStatus(): Promise<[boolean, string]>;

  /** Generates the 1080x1920 vertical sequence (blurred background) and activates it. */
  generateVertical(): Promise<StateSnapshot>;
  setActiveSequence(sequenceId: Id): Promise<StateSnapshot>;

  /** Creates an auto-subtitles clip over a transcribed media clip. */
  addSubtitlesClip(clipId: Id): Promise<StateSnapshot>;

  /** Launches the Whisper transcription of an asset (background job). */
  transcribeAsset(assetId: Id): Promise<void>;

  /** Changes a clip's speed (0.05–20; export preserves the pitch). */
  setClipSpeed(clipId: Id, speed: number): Promise<StateSnapshot>;

  /** Clip silences: mode "delete" cuts, "speedup" speeds up 4x. */
  removeSilences(
    clipId: Id,
    mode: "delete" | "speedup" | "split",
    params?: { thresholdDb?: number; minSilenceMs?: number; padMs?: number },
  ): Promise<{ removed: number; removed_us: number; snapshot: StateSnapshot }>;

  /** (port, token) of the embedded MCP server (null if not active). */
  mcpStatus(): Promise<[number, string] | null>;
  /** Invoke an MCP tool by name with arguments. */
  mcpCall(toolName: string, args: Record<string, unknown>): Promise<unknown>;
  /** List available MCP tools. */
  mcpListTools(): Promise<unknown>;
  setProjectSettings(whisperLanguage: string, whisperModel: string): Promise<StateSnapshot>;

  // -- export progress --
  cancelExport(): Promise<void>;
  onExportProgress(cb: (progress: number) => void): Promise<() => void>;
}
