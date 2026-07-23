/**
 * MockEngine: in-memory implementation of the EngineClient contract for
 * browser development and visual checks. Replicates ue-core semantics
 * (frame-quantized split, ripple, snapshot-based history) in a
 * simplified way; the desktop uses the real backend.
 */

import { quantizeToFrame } from "../lib/time";
import type { EngineClient } from "./client";
import type {
  AudioProps,
  AvatarConfig,
  Clip,
  EffectDef,
  EffectInstance,
  GeneratorDef,
  Id,
  MediaAsset,
  Param,
  Project,
  Sequence,
  StateSnapshot,
  TimeUs,
  TextStyle,
  Track,
  Transform2D,
  TransitionRef,
  TtsCatalog,
} from "./types";
import { DEFAULT_AUDIO, DEFAULT_TEXT_STYLE, DEFAULT_TRANSFORM, activeSequence } from "./types";

let idCounter = 0;
export function newId(prefix = "id"): Id {
  idCounter += 1;
  return `${prefix}_${idCounter.toString(36).padStart(4, "0")}`;
}

const S = 1_000_000;

export class MockEngine implements EngineClient {
  readonly kind = "mock" as const;
  private project: Project;
  private version = 0;
  private dirty = false;
  private undoStack: { label: string; snapshot: Project }[] = [];
  private redoStack: { label: string; snapshot: Project }[] = [];

  constructor(project: Project) {
    this.project = project;
  }

  private get sequence(): Sequence {
    return activeSequence(this.project);
  }

  private snapshot(): StateSnapshot {
    return {
      project: structuredClone(this.project),
      version: this.version,
      dirty: this.dirty,
      can_undo: this.undoStack.length > 0,
      can_redo: this.redoStack.length > 0,
      undo_labels: this.undoStack.map((e) => e.label),
    };
  }

  private locate(id: Id): { track: Track; clip: Clip; index: number } | undefined {
    for (const track of this.sequence.tracks) {
      const index = track.clips.findIndex((c) => c.id === id);
      if (index >= 0) return { track, clip: track.clips[index], index };
    }
    return undefined;
  }

  private transaction(label: string, fn: () => void): StateSnapshot {
    const before = structuredClone(this.project);
    try {
      fn();
      this.undoStack.push({ label, snapshot: before });
      this.redoStack = [];
      this.version += 1;
      this.dirty = true;
    } catch (e) {
      this.project = before;
      throw e;
    }
    return this.snapshot();
  }

  // ---- contract ----

  async getState(): Promise<StateSnapshot> {
    return this.snapshot();
  }

  async splitClip(clipId: Id, tUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Split clip", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip not found");
      const { track, clip, index } = found;
      if (track.locked) throw new Error("track locked");
      const tq = quantizeToFrame(tUs, this.sequence.fps);
      if (tq <= clip.start || tq >= clip.start + clip.duration)
        throw new Error("cut point outside the clip");
      const offset = tq - clip.start;

      const left: Clip = structuredClone(clip);
      const right: Clip = structuredClone(clip);
      left.id = newId("clip");
      right.id = newId("clip");
      left.duration = offset;
      left.audio.fade_out_us = 0;
      right.start = tq;
      right.duration = clip.duration - offset;
      right.audio.fade_in_us = 0;
      right.transition_in = null;
      left.transition_out = null;
      if (left.payload.type === "media" && right.payload.type === "media") {
        const srcOff = Math.round(offset * clip.speed);
        right.payload.src_in = left.payload.src_in + srcOff;
        left.payload.src_out = left.payload.src_in + srcOff;
      }
      track.clips.splice(index, 1, left, right);
    });
  }

  async deleteClips(ids: Id[], ripple: boolean): Promise<StateSnapshot> {
    return this.transaction(ripple ? "Delete (ripple)" : "Delete", () => {
      const removed: { trackId: Id; start: TimeUs; end: TimeUs }[] = [];
      for (const id of ids) {
        const found = this.locate(id);
        if (!found) continue;
        if (found.track.locked) throw new Error("track locked");
        removed.push({
          trackId: found.track.id,
          start: found.clip.start,
          end: found.clip.start + found.clip.duration,
        });
        found.track.clips.splice(found.index, 1);
      }
      if (ripple) {
        for (const track of this.sequence.tracks) {
          const spans = removed.filter((r) => r.trackId === track.id);
          if (!spans.length) continue;
          for (const clip of track.clips) {
            const shift = spans
              .filter((s) => s.end <= clip.start)
              .reduce((acc, s) => acc + (s.end - s.start), 0);
            clip.start -= shift;
          }
        }
      }
    });
  }

  async moveClip(clipId: Id, toTrack: Id, toStartUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Move clip", () => {
      const found = this.locate(clipId);
      const target = this.sequence.tracks.find((t) => t.id === toTrack);
      if (!found || !target) throw new Error("clip or track not found");
      if (found.track.locked || target.locked) throw new Error("track locked");
      if (target.kind !== found.track.kind) throw new Error("incompatible track type");
      const startQ = Math.max(0, quantizeToFrame(toStartUs, this.sequence.fps));
      const dur = found.clip.duration;
      const collides = target.clips.some(
        (c) => c.id !== clipId && c.start < startQ + dur && startQ < c.start + c.duration,
      );
      if (collides) throw new Error("collision");
      found.track.clips.splice(found.index, 1);
      found.clip.start = startQ;
      target.clips.push(found.clip);
      target.clips.sort((a, b) => a.start - b.start);
    });
  }

  async trimClip(clipId: Id, left: boolean, newEdgeUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Trim clip", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip not found");
      const { clip } = found;
      const edge = quantizeToFrame(newEdgeUs, this.sequence.fps);
      if (left) {
        const delta = Math.min(Math.max(edge, 0), clip.start + clip.duration - 33_333) - clip.start;
        clip.start += delta;
        clip.duration -= delta;
        if (clip.payload.type === "media")
          clip.payload.src_in += Math.round(delta * clip.speed);
      } else {
        clip.duration = Math.max(33_333, edge - clip.start);
        if (clip.payload.type === "media")
          clip.payload.src_out = clip.payload.src_in + Math.round(clip.duration * clip.speed);
      }
    });
  }

  async undo(): Promise<StateSnapshot> {
    const entry = this.undoStack.pop();
    if (entry) {
      this.redoStack.push({ label: entry.label, snapshot: structuredClone(this.project) });
      this.project = entry.snapshot;
      this.version += 1;
    }
    return this.snapshot();
  }

  async redo(): Promise<StateSnapshot> {
    const entry = this.redoStack.pop();
    if (entry) {
      this.undoStack.push({ label: entry.label, snapshot: structuredClone(this.project) });
      this.project = entry.snapshot;
      this.version += 1;
    }
    return this.snapshot();
  }

  async setClipAudio(clipId: Id, audio: AudioProps): Promise<StateSnapshot> {
    return this.transaction("Edit audio", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip not found");
      found.clip.audio = audio;
    });
  }

  async setClipTransform(clipId: Id, transform: Transform2D): Promise<StateSnapshot> {
    return this.transaction("Edit transform", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip not found");
      found.clip.transform = transform;
    });
  }

  async pickMediaFiles(): Promise<string[] | null> {
    return null; // only available in the desktop app
  }

  async importMedia(_paths: string[]): Promise<StateSnapshot> {
    return this.snapshot();
  }

  async addClip(assetId: Id, atUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Add clip", () => {
      const asset = this.project.assets.find((a) => a.id === assetId);
      if (!asset) throw new Error("asset not found");
      const kind = asset.kind === "audio" ? "audio" : "video";
      const track = this.sequence.tracks.find((t) => t.kind === kind && !t.locked);
      if (!track) throw new Error("no compatible track");
      const duration = asset.kind === "image" ? 5 * S : asset.probe.duration_us;
      let start = Math.max(0, quantizeToFrame(atUs, this.sequence.fps));
      const collides = (s: number) =>
        track.clips.some((c) => c.start < s + duration && s < c.start + c.duration);
      if (collides(start)) {
        start = Math.max(...track.clips.map((c) => c.start + c.duration), 0);
      }
      track.clips.push(mediaClip(asset, 0, duration / S, start / S));
      track.clips.sort((a, b) => a.start - b.start);
    });
  }

  async renderFrame(): Promise<Uint8Array | null> {
    return null; // the mock draws its own preview
  }

  async resolveAssetUrl(): Promise<string | null> {
    return null; // no real files in the browser mock
  }

  async renderAssetFrame(): Promise<Uint8Array | null> {
    return null; // no backend decoder in the browser
  }

  async pickSavePath(): Promise<string | null> {
    return null; // desktop only
  }

  async exportVideo(_path?: string, _settings?: unknown): Promise<string> {
    throw new Error("Export requires the desktop app (npx tauri dev)");
  }

  // the mock does not play audio: the UI uses its local clock (rAF)
  async playbackPlay(): Promise<void> {
    throw new Error("no audio in the browser");
  }
  async playbackPause(): Promise<number> {
    throw new Error("no audio in the browser");
  }
  async playbackSeek(): Promise<void> {}
  async playbackSetRate(): Promise<void> {}
  uiLog(level: "error" | "warn" | "info", message: string): void {
    // browser demo: the devtools console is the terminal
    (level === "error" ? console.error : console.warn)(`[ui] ${message}`);
  }
  async checkRecovery(): Promise<string | null> {
    return null;
  }
  async recoverProject(): Promise<StateSnapshot> {
    throw new Error("no recovery in the browser");
  }
  async discardRecovery(): Promise<void> {}
  async getAudioPeaks(): Promise<number[] | null> {
    return null;
  }
  async ensureThumbs(): Promise<null> {
    return null;
  }
  async getThumbStrip(): Promise<Uint8Array | null> {
    return null;
  }
  async playbackPosition(): Promise<[number, boolean, number, number]> {
    throw new Error("no audio in the browser");
  }
  async onStateChanged(): Promise<() => void> {
    return () => {};
  }

  async saveProject(): Promise<string> {
    throw new Error("Saving requires the desktop app (npx tauri dev)");
  }
  async openProject(): Promise<StateSnapshot> {
    throw new Error("Opening requires the desktop app (npx tauri dev)");
  }
  async pickProjectSavePath(): Promise<string | null> {
    return null;
  }
  async pickProjectOpenPath(): Promise<string | null> {
    return null;
  }
  async getEffectsCatalog(): Promise<EffectDef[]> {
    // the same manifests the backend embeds (single source of truth)
    const manifests = await Promise.all([
      import("../../effects/core/color_correct/manifest.json"),
      import("../../effects/core/chroma_key/manifest.json"),
      import("../../effects/core/gaussian_blur/manifest.json"),
    ]);
    return manifests.map((m) => m.default as unknown as EffectDef);
  }

  async reloadEffectPacks(): Promise<{ catalog: EffectDef[]; errors: string[]; dir: string | null }> {
    return { catalog: await this.getEffectsCatalog(), errors: [], dir: null };
  }

  async setClipEffects(clipId: Id, effects: EffectInstance[]): Promise<StateSnapshot> {
    return this.transaction("Edit effects", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip not found");
      found.clip.effects = effects;
    });
  }

  async setClipTransition(
    clipId: Id,
    transition: TransitionRef | null,
    out?: boolean,
  ): Promise<StateSnapshot> {
    return this.transaction("Edit transition", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip not found");
      if (out) found.clip.transition_out = transition;
      else found.clip.transition_in = transition;
    });
  }

  /** Splits any clip crossing `t` on every unlocked track. */
  private carveAt(t: number) {
    for (const track of this.sequence.tracks) {
      if (track.locked) continue;
      const idx = track.clips.findIndex((c) => c.start < t && t < c.start + c.duration);
      if (idx < 0) continue;
      const c = track.clips[idx];
      const left = structuredClone(c);
      const right = structuredClone(c);
      right.id = newId("clip");
      left.duration = t - c.start;
      right.start = t;
      right.duration = c.start + c.duration - t;
      if (c.payload.type === "media") {
        const cut = c.payload.src_in + Math.round((t - c.start) * c.speed);
        (left.payload as { src_out: number }).src_out = cut;
        (right.payload as { src_in: number }).src_in = cut;
      }
      track.clips.splice(idx, 1, left, right);
    }
  }

  async moveRange(
    _sequenceId: Id,
    fromUs: number,
    toUs: number,
    destUs: number,
  ): Promise<StateSnapshot> {
    return this.transaction("Move range", () => {
      if (destUs >= fromUs && destUs <= toUs) throw new Error("destination inside the range");
      const len = toUs - fromUs;
      for (const t of [fromUs, toUs, destUs]) this.carveAt(t);
      for (const track of this.sequence.tracks) {
        if (track.locked) continue;
        const moved = track.clips.filter((c) => c.start >= fromUs && c.start + c.duration <= toUs);
        track.clips = track.clips.filter((c) => !moved.includes(c));
        // close the gap
        for (const c of track.clips) if (c.start >= toUs) c.start -= len;
        // open at the destination (already shifted if it was after the range)
        const dest = destUs > toUs ? destUs - len : destUs;
        for (const c of track.clips) if (c.start >= dest) c.start += len;
        for (const c of moved) c.start = dest + (c.start - fromUs);
        track.clips.push(...moved);
        track.clips.sort((a, b) => a.start - b.start);
      }
    });
  }

  async cutRanges(
    _sequenceId: Id,
    ranges: [number, number][],
    ripple: boolean,
  ): Promise<StateSnapshot> {
    return this.transaction(`Cut ${ranges.length} range(s)`, () => {
      const sorted = [...ranges].sort((a, b) => b[0] - a[0]); // right to left
      for (const [a, b] of sorted) {
        this.carveAt(a);
        this.carveAt(b);
        for (const track of this.sequence.tracks) {
          if (track.locked) continue;
          track.clips = track.clips.filter(
            (c) => !(c.start >= a && c.start + c.duration <= b),
          );
          if (ripple) {
            for (const c of track.clips) if (c.start >= b) c.start -= b - a;
          }
        }
      }
    });
  }

  async saveAvatarConfig(config: AvatarConfig): Promise<[Id, StateSnapshot]> {
    const id = config.id || `av_${Math.random().toString(36).slice(2, 8)}`;
    const snap = await this.transaction("Save avatar", () => {
      this.project.avatars = this.project.avatars ?? [];
      const idx = this.project.avatars.findIndex((c) => c.id === id);
      if (idx >= 0) this.project.avatars[idx] = { ...config, id };
      else this.project.avatars.push({ ...config, id });
    });
    return [id, snap];
  }
  async removeAvatarConfig(configId: Id): Promise<StateSnapshot> {
    return this.transaction("Delete avatar", () => {
      this.project.avatars = (this.project.avatars ?? []).filter((c) => c.id !== configId);
    });
  }
  async pickAvatarMedia(): Promise<string[]> {
    return [];
  }
  async exportAvatarConfig(): Promise<string> {
    throw new Error("Exporting the avatar setup requires the desktop app");
  }
  async importAvatarConfig(): Promise<[Id, StateSnapshot]> {
    throw new Error("Importing the avatar setup requires the desktop app");
  }
  async generateAvatarVideo(): Promise<void> {
    throw new Error("Generating the avatar requires the desktop app (npx tauri dev)");
  }
  async onAvatarProgress(): Promise<() => void> {
    return () => {};
  }
  async listTtsVoices(): Promise<TtsCatalog> {
    // fake catalog so the dialog can be developed in the browser
    return {
      engines: [
        {
          id: "say",
          name: "System voice (say)",
          available: true,
          detail: "mock",
          voices: [
            { id: "Monica", name: "Monica", lang: "es_ES" },
            { id: "Paulina", name: "Paulina", lang: "es_MX" },
            { id: "Samantha", name: "Samantha", lang: "en_US" },
          ],
          rate: { min: 90, max: 400, default: 175, step: 5, label: "words/min" },
        },
        {
          id: "kokoro",
          name: "Kokoro AI",
          available: true,
          detail: "mock",
          voices: [
            { id: "ef_dora", name: "Dora", lang: "es" },
            { id: "af_heart", name: "Heart", lang: "en-US" },
          ],
          rate: { min: 0.5, max: 2, default: 1, step: 0.05, label: "speed ×" },
        },
      ],
      engines_dir: null,
    };
  }
  async generateSpeech(): Promise<void> {
    throw new Error("Voiceover generation requires the desktop app (npx tauri dev)");
  }
  async onTtsProgress(): Promise<() => void> {
    return () => {};
  }
  async denoiseStatus(): Promise<[boolean, string]> {
    return [true, "mock"];
  }
  async pickJsonSavePath(): Promise<string | null> {
    return null;
  }
  async pickJsonOpenPath(): Promise<string | null> {
    return null;
  }

  async generateVertical(): Promise<StateSnapshot> {
    return this.transaction("Generate vertical", () => {
      const src = this.sequence;
      const copy: Sequence = structuredClone(src);
      copy.id = newId("seq");
      copy.name = `${src.name} (Vertical)`;
      copy.resolution = [1080, 1920];
      for (const track of copy.tracks) {
        track.id = newId("track");
        for (const clip of track.clips) {
          clip.id = newId("clip");
          if (track.kind === "video" && clip.payload.type === "media") {
            clip.effects.push({
              effect_id: "core.vertical_fill",
              enabled: true,
              params: {},
              color_params: {},
            });
          }
        }
      }
      for (const m of copy.markers) m.id = newId("mk");
      this.project.sequences.push(copy);
      this.project.active_sequence = copy.id;
    });
  }

  async setActiveSequence(sequenceId: Id): Promise<StateSnapshot> {
    return this.transaction("Change sequence", () => {
      if (!this.project.sequences.some((s) => s.id === sequenceId))
        throw new Error("sequence not found");
      this.project.active_sequence = sequenceId;
    });
  }

  async addSubtitlesClip(clipId: Id): Promise<StateSnapshot> {
    return this.transaction("Auto subtitles", () => {
      const found = this.locate(clipId);
      if (!found || found.clip.payload.type !== "media") throw new Error("invalid clip");
      const assetId = found.clip.payload.asset_id;
      const doc = this.project.transcripts.find((t) => t.asset_id === assetId);
      if (!doc) throw new Error("the media has no transcript");
      const track = [...this.sequence.tracks].reverse().find((t) => t.kind === "video" && !t.locked);
      if (!track) throw new Error("no video track");
      const collides = track.clips.some(
        (c) => c.start < found.clip.start + found.clip.duration && found.clip.start < c.start + c.duration,
      );
      if (collides) throw new Error("the top track is occupied in that range");
      track.clips.push({
        group: null,
        id: newId("clip"),
        payload: {
          type: "subtitles",
          transcript_id: doc.id,
          style: { ...structuredClone(DEFAULT_TEXT_STYLE), size: 48, y_offset: 380 },
          mode: "phrase",
        },
        start: found.clip.start,
        duration: found.clip.duration,
        speed: 1,
        effects: [],
        transform: structuredClone(DEFAULT_TRANSFORM),
        audio: structuredClone(DEFAULT_AUDIO),
        transition_in: null,
        transition_out: null,
        label_color: null,
      });
      track.clips.sort((a, b) => a.start - b.start);
    });
  }

  async transcribeAsset(): Promise<void> {
    throw new Error("Transcribing requires the desktop app (npx tauri dev)");
  }

  async setClipSpeed(clipId: Id, speed: number): Promise<StateSnapshot> {
    return this.transaction("Change speed", () => {
      const found = this.locate(clipId);
      if (!found || found.clip.payload.type !== "media") throw new Error("invalid clip");
      const srcLen = found.clip.payload.src_out - found.clip.payload.src_in;
      found.clip.speed = speed;
      found.clip.duration = Math.max(33_333, Math.round(srcLen / speed));
    });
  }

  async removeSilences(): Promise<{ removed: number; removed_us: number; snapshot: StateSnapshot }> {
    throw new Error("Removing silences requires the desktop app (npx tauri dev)");
  }

  async mcpStatus(): Promise<[number, string] | null> {
    return null;
  }
  async mcpCall(): Promise<unknown> {
    throw new Error("MCP requires the desktop app");
  }
  async mcpListTools(): Promise<unknown> {
    throw new Error("MCP requires the desktop app");
  }
  async setProjectSettings(lang: string, model: string): Promise<StateSnapshot> {
    return this.transaction("AI settings", () => {
      this.project.settings.whisper_language = lang;
      this.project.settings.whisper_model = model;
    });
  }

  async cancelExport(): Promise<void> {}
  async onExportProgress(): Promise<() => void> {
    return () => {};
  }

  async addTextClip(content: string, atUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Add title", () => {
      const track = [...this.sequence.tracks]
        .reverse()
        .find((t) => t.kind === "video" && !t.locked);
      if (!track) throw new Error("no video track");
      const duration = 4 * S;
      let start = Math.max(0, quantizeToFrame(atUs, this.sequence.fps));
      const collides = (st: number) =>
        track.clips.some((c) => c.start < st + duration && st < c.start + c.duration);
      if (collides(start)) start = Math.max(...track.clips.map((c) => c.start + c.duration), 0);
      const clip = textClip(content, start / S, duration / S);
      track.clips.push(clip);
      track.clips.sort((a, b) => a.start - b.start);
    });
  }

  async setSubtitlesProps(
    clipId: Id,
    style: TextStyle,
    mode: "phrase" | "word" | "karaoke",
    maxWords: number | null = null,
  ): Promise<StateSnapshot> {
    return this.transaction("Edit subtitles", () => {
      const found = this.locate(clipId);
      if (!found || found.clip.payload.type !== "subtitles")
        throw new Error("not a subtitles clip");
      found.clip.payload.style = style;
      found.clip.payload.mode = mode;
      found.clip.payload.max_words = maxWords;
    });
  }

  async setClipText(clipId: Id, content: string, style: TextStyle): Promise<StateSnapshot> {
    return this.transaction("Edit text", () => {
      const found = this.locate(clipId);
      if (!found || found.clip.payload.type !== "text") throw new Error("not a text clip");
      found.clip.payload.content = content;
      found.clip.payload.style = style;
    });
  }

  async listFonts(): Promise<[string, string][]> {
    return [
      ["Arial", ""],
      ["Helvetica", ""],
      ["Georgia", ""],
      ["Courier New", ""],
    ];
  }
  private templates: Record<string, TextStyle> = {};
  async listTextTemplates(): Promise<Record<string, TextStyle>> {
    return { ...this.templates };
  }
  async saveTextTemplate(name: string, style: TextStyle): Promise<Record<string, TextStyle>> {
    this.templates[name] = style;
    return { ...this.templates };
  }

  async relinkAsset(): Promise<StateSnapshot> {
    throw new Error("Relinking requires the desktop app");
  }
  async newProject(name: string): Promise<StateSnapshot> {
    this.project = demoProject();
    this.project.name = name;
    this.undoStack = [];
    this.redoStack = [];
    this.version += 1;
    this.dirty = false;
    return this.snapshot();
  }

  async unlinkClip(clipId: Id): Promise<StateSnapshot> {
    return this.transaction("Unlink clips", () => {
      const found = this.locate(clipId);
      if (!found?.clip.group) throw new Error("the clip is not linked");
      const group = found.clip.group;
      for (const track of this.sequence.tracks)
        for (const c of track.clips) if (c.group === group) c.group = null;
    });
  }

  async setTrackProp(
    trackId: Id,
    prop: "muted" | "solo" | "locked",
    value: boolean,
  ): Promise<StateSnapshot> {
    return this.transaction("Track", () => {
      const track = this.sequence.tracks.find((t) => t.id === trackId);
      if (!track) throw new Error("track not found");
      track[prop] = value;
    });
  }

  async getGenerators(): Promise<GeneratorDef[]> {
    return MOCK_GENERATORS;
  }
  async addGeneratorClip(generatorId: string, atUs: number): Promise<StateSnapshot> {
    return this.transaction("Add generator", () => {
      const track = this.sequence.tracks.filter((t) => t.kind === "video").at(-1);
      if (!track) throw new Error("no video track");
      track.clips.push({
        ...emptyClipDefaults(),
        id: `gen_${Math.random().toString(36).slice(2, 8)}`,
        payload: { type: "generator", generator_id: generatorId, params: {}, color_params: {} },
        start: Math.max(0, atUs),
        duration: 4_000_000,
      });
      track.clips.sort((a, b) => a.start - b.start);
    });
  }
  async setClipGenerator(
    clipId: Id,
    generatorId: string,
    params: Record<string, Param>,
    colorParams: Record<string, string>,
  ): Promise<StateSnapshot> {
    return this.transaction("Edit generator", () => {
      for (const t of this.sequence.tracks) {
        const clip = t.clips.find((c) => c.id === clipId);
        if (clip && clip.payload.type === "generator") {
          clip.payload = {
            type: "generator",
            generator_id: generatorId,
            params,
            color_params: colorParams,
          };
          return;
        }
      }
      throw new Error("generator clip not found");
    });
  }
  async setWordText(transcriptId: Id, index: number, text: string): Promise<StateSnapshot> {
    return this.transaction("Correct word", () => {
      const doc = this.project.transcripts.find((t) => t.id === transcriptId);
      if (!doc) throw new Error("transcript not found");
      doc.words[index].display = text.trim() ? text.trim() : null;
    });
  }
  async replaceWords(
    transcriptId: Id,
    from: string,
    to: string,
  ): Promise<{ replaced: number; snapshot: StateSnapshot }> {
    let replaced = 0;
    const snapshot = await this.transaction("Replace words", () => {
      const doc = this.project.transcripts.find((t) => t.id === transcriptId);
      if (!doc) throw new Error("transcript not found");
      for (const w of doc.words) {
        if ((w.display ?? w.text).trim().toLowerCase() === from.trim().toLowerCase()) {
          w.display = to.trim() ? to.trim() : null;
          replaced += 1;
        }
      }
    });
    return { replaced, snapshot };
  }
  async setSequenceProps(
    sequenceId: Id,
    width: number,
    height: number,
    fpsNum: number,
    fpsDen: number,
  ): Promise<StateSnapshot> {
    return this.transaction("Sequence settings", () => {
      const seq = this.project.sequences.find((s) => s.id === sequenceId);
      if (!seq) throw new Error("sequence not found");
      seq.resolution = [width, height];
      seq.fps = [fpsNum, fpsDen];
    });
  }
  async removeSequence(sequenceId: Id): Promise<StateSnapshot> {
    return this.transaction("Delete sequence", () => {
      if (this.project.sequences.length <= 1)
        throw new Error("cannot delete the last sequence");
      const idx = this.project.sequences.findIndex((s) => s.id === sequenceId);
      if (idx < 0) throw new Error("sequence not found");
      this.project.sequences.splice(idx, 1);
      if (this.project.active_sequence === sequenceId)
        this.project.active_sequence = this.project.sequences[0].id;
    });
  }
  async addTrack(kind: "video" | "audio"): Promise<StateSnapshot> {
    return this.transaction("Add track", () => {
      const n = this.sequence.tracks.filter((t) => t.kind === kind).length;
      this.sequence.tracks.push({
        id: `trk_${Math.random().toString(36).slice(2, 8)}`,
        kind,
        name: `${kind === "video" ? "V" : "A"}${n + 1}`,
        muted: false,
        solo: false,
        locked: false,
        volume_db: 0,
        clips: [],
      });
    });
  }
  async removeTrack(trackId: Id): Promise<StateSnapshot> {
    return this.transaction("Delete track", () => {
      const tracks = this.sequence.tracks;
      const idx = tracks.findIndex((t) => t.id === trackId);
      if (idx < 0) throw new Error("track not found");
      if (tracks.filter((t) => t.kind === tracks[idx].kind).length <= 1)
        throw new Error("cannot delete the last track of its type");
      tracks.splice(idx, 1);
    });
  }
  async renameTrack(trackId: Id, name: string): Promise<StateSnapshot> {
    return this.transaction("Rename track", () => {
      const track = this.sequence.tracks.find((t) => t.id === trackId);
      if (!track) throw new Error("track not found");
      track.name = name;
    });
  }
  async setTrackVolume(trackId: Id, db: number): Promise<StateSnapshot> {
    return this.transaction("Track volume", () => {
      const track = this.sequence.tracks.find((t) => t.id === trackId);
      if (!track) throw new Error("track not found");
      track.volume_db = db;
    });
  }
}

const MOCK_GENERATORS: GeneratorDef[] = [
  {
    id: "core.solid",
    name: "Solid rectangle",
    params: [
      { key: "color", label: "Color", type: "color", default: "#ff3355" },
      { key: "width", label: "Width", type: "float", default: 640, min: 16, max: 4096 },
      { key: "height", label: "Height", type: "float", default: 360, min: 16, max: 4096 },
    ],
    source: "color",
  },
  {
    id: "core.gradient",
    name: "Gradient",
    params: [
      { key: "color_a", label: "Color A", type: "color", default: "#ffb224" },
      { key: "color_b", label: "Color B", type: "color", default: "#16130f" },
      { key: "width", label: "Width", type: "float", default: 1920, min: 16, max: 4096 },
      { key: "height", label: "Height", type: "float", default: 1080, min: 16, max: 4096 },
    ],
    source: "gradients",
  },
];

/** Common fields for a new mock clip. */
function emptyClipDefaults() {
  return {
    speed: 1,
    effects: [],
    transform: structuredClone(DEFAULT_TRANSFORM),
    audio: { ...DEFAULT_AUDIO, muted: true },
    transition_in: null,
    transition_out: null,
    label_color: null,
    group: null,
  };
}

// ---------------------------------------------------------------------------
// Demo project (same shape as ue-core)
// ---------------------------------------------------------------------------

function makeAsset(
  kind: MediaAsset["kind"],
  path: string,
  durationS: number,
  extra?: Partial<MediaAsset["probe"]>,
): MediaAsset {
  return {
    id: newId("asset"),
    kind,
    path,
    content_hash: `xxh3:${path.length.toString(16).padStart(16, "0")}`,
    probe: {
      duration_us: durationS * S,
      fps: kind === "video" ? [30, 1] : null,
      width: kind === "video" ? 1920 : kind === "image" ? 1024 : 0,
      height: kind === "video" ? 1080 : kind === "image" ? 1024 : 0,
      rotation: 0,
      vcodec: kind === "video" ? "h264" : null,
      acodec: kind === "audio" ? "aac" : kind === "video" ? "aac" : null,
      audio_channels: kind === "audio" ? 2 : kind === "video" ? 2 : 0,
      vfr: false,
      ...extra,
    },
    proxy: null,
    audio_conform: null,
    peaks: null,
    thumbnails: null,
    transcript: null,
    offline: false,
  };
}

function mediaClip(
  asset: MediaAsset,
  srcInS: number,
  srcOutS: number,
  startS: number,
  extra?: Omit<Partial<Clip>, "audio"> & { audio?: Partial<AudioProps> },
): Clip {
  const { audio: audioExtra, ...clipExtra } = extra ?? {};
  return {
    id: newId("clip"),
    payload: { type: "media", asset_id: asset.id, src_in: srcInS * S, src_out: srcOutS * S },
    start: startS * S,
    duration: (srcOutS - srcInS) * S,
    speed: 1,
    effects: [],
    transform: structuredClone(DEFAULT_TRANSFORM),
    audio: { ...structuredClone(DEFAULT_AUDIO), ...audioExtra },
    transition_in: null,
    transition_out: null,
    label_color: null,
    group: null,
    ...clipExtra,
  };
}

function textClip(content: string, startS: number, durationS: number): Clip {
  return {
    id: newId("clip"),
    payload: { type: "text", content, style: structuredClone(DEFAULT_TEXT_STYLE) },
    start: startS * S,
    duration: durationS * S,
    speed: 1,
    effects: [],
    transform: structuredClone(DEFAULT_TRANSFORM),
    audio: structuredClone(DEFAULT_AUDIO),
    transition_in: null,
    transition_out: null,
    label_color: null,
    group: null,
  };
}

function track(kind: Track["kind"], name: string, clips: Clip[], volumeDb = 0): Track {
  return {
    id: newId("track"),
    kind,
    name,
    muted: false,
    solo: false,
    locked: false,
    volume_db: volumeDb,
    clips,
  };
}

export function demoProject(): Project {
  const cam = makeAsset("video", "media/intro_camera.mp4", 28);
  const gameplay = makeAsset("video", "media/gameplay_physics.mp4", 84, {
    fps: [60, 1],
    width: 2560,
    height: 1440,
    vcodec: "hevc",
  });
  const screen = makeAsset("video", "media/code_screen.mp4", 152, {
    audio_channels: 0,
    acodec: null,
  });
  const voice = makeAsset("audio", "media/voiceover.wav", 58, { acodec: "pcm_s16le", audio_channels: 1 });
  const music = makeAsset("audio", "media/lofi_music.mp3", 130, { acodec: "mp3" });
  const logo = makeAsset("image", "media/channel_logo.png", 0);

  const seq: Sequence = {
    id: newId("seq"),
    name: "Main",
    resolution: [1920, 1080],
    fps: [30, 1],
    sample_rate: 48000,
    markers: [
      { id: newId("mk"), t: 8.5 * S, name: "Code demo", color: "#6fa3b5" },
      { id: newId("mk"), t: 22.5 * S, name: "Gameplay", color: "#8fb573" },
    ],
    tracks: [
      track(
        "audio",
        "A2",
        [mediaClip(music, 0, 46, 0, { audio: { gain_db: -14, fade_in_us: 1.5 * S, fade_out_us: 3 * S } })],
        -12,
      ),
      track("audio", "A1", [
        mediaClip(voice, 0, 7.5, 0.8),
        mediaClip(voice, 8.1, 16.4, 8.3),
        mediaClip(voice, 17.0, 29.2, 16.7),
        mediaClip(voice, 30.1, 41.0, 29.0),
      ]),
      track("video", "V1", [
        mediaClip(cam, 2, 10.5, 0),
        mediaClip(screen, 12, 26, 8.5),
        mediaClip(gameplay, 5, 19.5, 22.5),
        mediaClip(cam, 14, 22, 37),
      ]),
      track("video", "V2", [
        textClip("HOW I BUILT A PHYSICS ENGINE", 1.2, 4.4),
        textClip("subscribe →", 30, 3.5),
      ]),
    ],
  };

  // demo transcripts (for the Text panel)
  const demoTranscript = (
    asset: Project["assets"][number],
    phrases: string[],
  ): Project["transcripts"][number] => {
    const words: Project["transcripts"][number]["words"] = [];
    const segments: Project["transcripts"][number]["segments"] = [];
    let t = 300_000;
    for (const phrase of phrases) {
      const from = words.length;
      for (const word of phrase.split(" ")) {
        const dur = 180_000 + word.length * 30_000;
        words.push({ text: word, start_us: t, end_us: t + dur, confidence: 0.95, rejected: false });
        t += dur + 60_000;
      }
      segments.push({
        text: phrase,
        start_us: words[from].start_us,
        end_us: words[words.length - 1].end_us,
        word_range: [from, words.length],
        emotion: null,
        volume_rms: 0,
      });
      t += 1_200_000; // pause between phrases
    }
    const doc: Project["transcripts"][number] = {
      id: newId("tr"),
      asset_id: asset.id,
      language: "en",
      model: "base",
      words,
      segments,
      global_avg_volume: 0,
    };
    asset.transcript = doc.id;
    return doc;
  };
  const voiceTranscript = demoTranscript(voice, [
    "hey everyone welcome back to another devlog",
    "today we're building a physics engine from scratch",
    "um so first let's talk about collisions",
  ]);
  const camTranscript = demoTranscript(cam, [
    "what's up guys quick intro before we start",
    "grab a coffee this one is a long one",
  ]);

  return {
    schema_version: 1,
    id: newId("proj"),
    name: "Devlog 12 — Physics engine",
    created_at: "",
    settings: { whisper_language: "auto", whisper_model: "base", autosave_secs: 60 },
    assets: [cam, gameplay, screen, voice, music, logo],
    avatars: [
      {
        id: "av_demo",
        name: "Demo avatar",
        expressions: [
          { name: "calm", path: "/avatars/calm.png" },
          { name: "angry", path: "/avatars/angry.mp4" },
        ],
        shake_factor: 1,
        scale: 0.25,
        model: "",
        api_base: "",
        api_key: "",
      },
    ],
    transcripts: [voiceTranscript, camTranscript],
    sequences: [seq],
    active_sequence: seq.id,
  };
}
