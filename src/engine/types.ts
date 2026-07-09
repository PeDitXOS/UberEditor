/**
 * Espejo TypeScript del modelo real de ue-core (crates/ue-core/src/model.rs),
 * con la misma forma JSON que produce serde (snake_case, payload etiquetado,
 * tuplas como arrays). Generación automática (ts-rs) pendiente; mantener en
 * sincronía a mano hasta entonces.
 */

export type Id = string;
export type TimeUs = number;

// -- keyframes ---------------------------------------------------------------

export type Interp =
  | { kind: "hold" }
  | { kind: "linear" }
  | { kind: "smooth"; tan_out?: number; tan_in?: number };

export interface Keyframe {
  t: TimeUs;
  value: number;
  interp: Interp;
}

/** número plano ⇔ Const; objeto {keys} ⇔ Curve (serde untagged) */
export type Param = number | { keys: Keyframe[] };

export function paramValue(p: Param, tUs: TimeUs = 0): number {
  if (typeof p === "number") return p;
  const keys = p.keys;
  if (!keys.length) return 0;
  if (tUs <= keys[0].t) return keys[0].value;
  const last = keys[keys.length - 1];
  if (tUs >= last.t) return last.value;
  for (let i = 0; i < keys.length - 1; i++) {
    const a = keys[i];
    const b = keys[i + 1];
    if (a.t <= tUs && tUs < b.t) {
      if (a.interp.kind === "hold") return a.value;
      const u = (tUs - a.t) / (b.t - a.t);
      return a.value + (b.value - a.value) * u; // lineal (smooth ≈ lineal en UI)
    }
  }
  return last.value;
}

export function isCurve(p: Param): boolean {
  return typeof p !== "number";
}

// -- media -------------------------------------------------------------------

export type MediaKind = "video" | "audio" | "image";
export type TrackKind = "video" | "audio";

export interface ProbeInfo {
  duration_us: TimeUs;
  fps: [number, number] | null;
  width: number;
  height: number;
  rotation: number;
  vcodec: string | null;
  acodec: string | null;
  audio_channels: number;
  vfr: boolean;
}

export interface MediaAsset {
  id: Id;
  kind: MediaKind;
  path: string;
  content_hash: string;
  probe: ProbeInfo;
  proxy: string | null;
  audio_conform: string | null;
  peaks: string | null;
  thumbnails: string | null;
  transcript: Id | null;
  offline: boolean;
}

export function assetName(asset: MediaAsset | undefined): string {
  return asset?.path.split("/").pop() ?? "(archivo)";
}

// -- clips -------------------------------------------------------------------

export type SubtitleMode = "phrase" | "word" | "karaoke";
export type TextAlign = "left" | "center" | "right";

export interface TextStyle {
  font: string;
  size: number;
  color: string;
  bg: string | null;
  stroke_color: string | null;
  stroke_width: number;
  highlight_color: string | null;
  y_offset: number;
  align: TextAlign;
}

export const DEFAULT_TEXT_STYLE: TextStyle = {
  font: "sans-serif",
  size: 60,
  color: "#ffffff",
  bg: null,
  stroke_color: null,
  stroke_width: 0,
  highlight_color: null,
  y_offset: 0,
  align: "center",
};

export type ClipPayload =
  | { type: "media"; asset_id: Id; src_in: TimeUs; src_out: TimeUs }
  | { type: "text"; content: string; style: TextStyle }
  | { type: "subtitles"; transcript_id: Id; style: TextStyle; mode: SubtitleMode }
  | { type: "solid"; color: [number, number, number, number] }
  | {
      type: "avatar";
      driver_asset: Id;
      avatars: Record<string, string>;
      shake_factor: number;
      scale: number;
    };

export interface Transform2D {
  position: [Param, Param];
  scale: [Param, Param];
  rotation: Param;
  crop: [Param, Param, Param, Param];
  opacity: Param;
  flip_h: boolean;
  flip_v: boolean;
}

export const DEFAULT_TRANSFORM: Transform2D = {
  position: [0, 0],
  scale: [1, 1],
  rotation: 0,
  crop: [0, 0, 0, 0],
  opacity: 1,
  flip_h: false,
  flip_v: false,
};

export interface AudioProps {
  gain_db: Param;
  pan: Param;
  fade_in_us: TimeUs;
  fade_out_us: TimeUs;
  muted: boolean;
}

export const DEFAULT_AUDIO: AudioProps = {
  gain_db: 0,
  pan: 0,
  fade_in_us: 0,
  fade_out_us: 0,
  muted: false,
};

export interface EffectInstance {
  effect_id: string;
  enabled: boolean;
  params: Record<string, Param>;
  color_params: Record<string, string>;
}

export interface TransitionRef {
  effect_id: string;
  duration: TimeUs;
  params: Record<string, Param>;
}

export interface Clip {
  id: Id;
  payload: ClipPayload;
  start: TimeUs;
  duration: TimeUs;
  speed: number;
  effects: EffectInstance[];
  transform: Transform2D;
  audio: AudioProps;
  transition_in: TransitionRef | null;
  label_color: string | null;
}

// -- secuencia / proyecto ----------------------------------------------------

export interface Track {
  id: Id;
  kind: TrackKind;
  name: string;
  muted: boolean;
  solo: boolean;
  locked: boolean;
  volume_db: number;
  clips: Clip[];
}

export interface Marker {
  id: Id;
  t: TimeUs;
  name: string;
  color: string | null;
}

export interface Sequence {
  id: Id;
  name: string;
  resolution: [number, number];
  fps: [number, number];
  sample_rate: number;
  tracks: Track[];
  markers: Marker[];
}

export interface ProjectSettings {
  whisper_language: string;
  autosave_secs: number;
}

export interface TranscriptWord {
  text: string;
  start_us: TimeUs;
  end_us: TimeUs;
  confidence: number;
  rejected: boolean;
}

export interface TranscriptSegment {
  text: string;
  start_us: TimeUs;
  end_us: TimeUs;
  word_range: [number, number];
  emotion: string | null;
  volume_rms: number;
}

export interface TranscriptDoc {
  id: Id;
  asset_id: Id;
  language: string;
  model: string;
  words: TranscriptWord[];
  segments: TranscriptSegment[];
  global_avg_volume: number;
}

export interface Project {
  schema_version: number;
  id: Id;
  name: string;
  created_at: string;
  settings: ProjectSettings;
  assets: MediaAsset[];
  transcripts: TranscriptDoc[];
  sequences: Sequence[];
  active_sequence: Id;
}

/** Posición de una palabra del asset en el timeline (vía el primer clip que la contiene). */
export function wordTimelineRange(
  project: Project,
  assetId: Id,
  word: TranscriptWord,
): [TimeUs, TimeUs] | null {
  const seq = activeSequence(project);
  for (const track of seq.tracks) {
    for (const clip of track.clips) {
      if (clip.payload.type !== "media" || clip.payload.asset_id !== assetId) continue;
      const { src_in, src_out } = clip.payload;
      if (word.start_us >= src_in && word.start_us < src_out) {
        const s = clip.start + Math.round((word.start_us - src_in) / clip.speed);
        const e = clip.start + Math.round((Math.min(word.end_us, src_out) - src_in) / clip.speed);
        return [s, Math.max(e, s + 1000)];
      }
    }
  }
  return null;
}

/** Tiempo de asset → timeline vía el primer clip media que lo contiene. */
export function assetTimeToTimeline(
  project: Project,
  assetId: Id,
  tAsset: TimeUs,
): TimeUs | null {
  const seq = activeSequence(project);
  for (const track of seq.tracks) {
    for (const clip of track.clips) {
      if (clip.payload.type !== "media" || clip.payload.asset_id !== assetId) continue;
      const { src_in, src_out } = clip.payload;
      if (tAsset >= src_in && tAsset < src_out) return clip.start + (tAsset - src_in);
    }
  }
  return null;
}

/** Texto del subtítulo activo de un clip Subtitles en el tiempo dado
 *  (respeta el modo: frase completa o palabra suelta más grande). */
export function activeSubtitleText(
  project: Project,
  clip: Clip,
  playheadUs: TimeUs,
): { content: string; style: TextStyle } | null {
  if (clip.payload.type !== "subtitles") return null;
  const { transcript_id, style, mode } = clip.payload;
  const doc = project.transcripts.find((t) => t.id === transcript_id);
  if (!doc) return null;
  const items =
    mode === "phrase"
      ? doc.segments.map((s) => ({ text: s.text, s: s.start_us, e: s.end_us }))
      : doc.words
          .filter((w) => !w.rejected)
          .map((w) => ({ text: w.text, s: w.start_us, e: w.end_us }));
  const effStyle = mode === "phrase" ? style : { ...style, size: style.size * 1.6 };
  for (const item of items) {
    const tlStart = assetTimeToTimeline(project, doc.asset_id, item.s);
    if (tlStart === null) continue;
    const tlEnd = tlStart + (item.e - item.s);
    const from = Math.max(tlStart, clip.start);
    const to = Math.min(tlEnd, clip.start + clip.duration);
    if (playheadUs >= from && playheadUs < to) return { content: item.text, style: effStyle };
  }
  return null;
}

/** Rangos de timeline de un conjunto de palabras, con padding y fusión. */
export function wordsToCutRanges(
  project: Project,
  assetId: Id,
  words: TranscriptWord[],
  padUs = 80_000,
  mergeGapUs = 120_000,
): [TimeUs, TimeUs][] {
  const raw: [TimeUs, TimeUs][] = [];
  for (const w of words) {
    const r = wordTimelineRange(project, assetId, w);
    if (r) raw.push([Math.max(0, r[0] - padUs), r[1] + padUs]);
  }
  raw.sort((a, b) => a[0] - b[0]);
  const merged: [TimeUs, TimeUs][] = [];
  for (const r of raw) {
    const last = merged[merged.length - 1];
    if (last && r[0] <= last[1] + mergeGapUs) last[1] = Math.max(last[1], r[1]);
    else merged.push([...r]);
  }
  return merged;
}

export function activeSequence(project: Project): Sequence {
  return (
    project.sequences.find((s) => s.id === project.active_sequence) ??
    project.sequences[0]
  );
}

export function clipDisplayName(clip: Clip, project: Project): string {
  switch (clip.payload.type) {
    case "media":
      return assetName(project.assets.find((a) => a.id === (clip.payload as { asset_id: Id }).asset_id));
    case "text":
      return clip.payload.content.length > 24
        ? "Título"
        : clip.payload.content || "Texto";
    case "subtitles":
      return "Subtítulos";
    case "solid":
      return "Color";
    case "avatar":
      return "Avatar";
  }
}

// -- catálogo de efectos (packs) ----------------------------------------------

export interface EffectParamDef {
  key: string;
  label?: string | null;
  type: "float" | "color";
  default: number | string;
  min?: number;
  max?: number;
}

export interface EffectDef {
  id: string;
  name: string;
  category: string;
  params: EffectParamDef[];
  ffmpeg: string;
  notes?: string | null;
}

/** Instancia nueva de un efecto con los defaults del manifest. */
export function instantiateEffect(def: EffectDef): EffectInstance {
  const params: Record<string, Param> = {};
  const color_params: Record<string, string> = {};
  for (const p of def.params) {
    if (p.type === "float") params[p.key] = p.default as number;
    else color_params[p.key] = p.default as string;
  }
  return { effect_id: def.id, enabled: true, params, color_params };
}

// -- snapshot del engine -----------------------------------------------------

export interface StateSnapshot {
  project: Project;
  version: number;
  dirty: boolean;
  can_undo: boolean;
  can_redo: boolean;
  undo_labels: string[];
}
