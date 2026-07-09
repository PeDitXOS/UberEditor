/**
 * MockEngine: implementación en memoria del contrato EngineClient para
 * desarrollo en navegador y pruebas visuales. Replica la semántica de ue-core
 * (split cuantizado a frame, ripple, historial por snapshots) de forma
 * simplificada; el escritorio usa el backend real.
 */

import { quantizeToFrame } from "../lib/time";
import type { EngineClient } from "./client";
import type {
  AudioProps,
  Clip,
  EffectDef,
  EffectInstance,
  Id,
  MediaAsset,
  Project,
  Sequence,
  StateSnapshot,
  TimeUs,
  TextStyle,
  Track,
  Transform2D,
  TransitionRef,
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

  // ---- contrato ----

  async getState(): Promise<StateSnapshot> {
    return this.snapshot();
  }

  async splitClip(clipId: Id, tUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Dividir clip", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip no encontrado");
      const { track, clip, index } = found;
      if (track.locked) throw new Error("pista bloqueada");
      const tq = quantizeToFrame(tUs, this.sequence.fps);
      if (tq <= clip.start || tq >= clip.start + clip.duration)
        throw new Error("punto de corte fuera del clip");
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
      if (left.payload.type === "media" && right.payload.type === "media") {
        const srcOff = Math.round(offset * clip.speed);
        right.payload.src_in = left.payload.src_in + srcOff;
        left.payload.src_out = left.payload.src_in + srcOff;
      }
      track.clips.splice(index, 1, left, right);
    });
  }

  async deleteClips(ids: Id[], ripple: boolean): Promise<StateSnapshot> {
    return this.transaction(ripple ? "Eliminar (ripple)" : "Eliminar", () => {
      const removed: { trackId: Id; start: TimeUs; end: TimeUs }[] = [];
      for (const id of ids) {
        const found = this.locate(id);
        if (!found) continue;
        if (found.track.locked) throw new Error("pista bloqueada");
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

  async moveClip(
    clipId: Id,
    toTrack: Id,
    toStartUs: TimeUs,
    _overwrite: boolean,
  ): Promise<StateSnapshot> {
    return this.transaction("Mover clip", () => {
      const found = this.locate(clipId);
      const target = this.sequence.tracks.find((t) => t.id === toTrack);
      if (!found || !target) throw new Error("clip o pista no encontrados");
      if (found.track.locked || target.locked) throw new Error("pista bloqueada");
      if (target.kind !== found.track.kind) throw new Error("tipo de pista incompatible");
      const startQ = Math.max(0, quantizeToFrame(toStartUs, this.sequence.fps));
      const dur = found.clip.duration;
      const collides = target.clips.some(
        (c) => c.id !== clipId && c.start < startQ + dur && startQ < c.start + c.duration,
      );
      if (collides) throw new Error("colisión");
      found.track.clips.splice(found.index, 1);
      found.clip.start = startQ;
      target.clips.push(found.clip);
      target.clips.sort((a, b) => a.start - b.start);
    });
  }

  async trimClip(clipId: Id, left: boolean, newEdgeUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Recortar clip", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip no encontrado");
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
    return this.transaction("Editar audio", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip no encontrado");
      found.clip.audio = audio;
    });
  }

  async setClipTransform(clipId: Id, transform: Transform2D): Promise<StateSnapshot> {
    return this.transaction("Editar transformación", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip no encontrado");
      found.clip.transform = transform;
    });
  }

  async pickMediaFiles(): Promise<string[] | null> {
    return null; // solo disponible en la app de escritorio
  }

  async importMedia(_paths: string[]): Promise<StateSnapshot> {
    return this.snapshot();
  }

  async addClip(assetId: Id, atUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Añadir clip", () => {
      const asset = this.project.assets.find((a) => a.id === assetId);
      if (!asset) throw new Error("asset no encontrado");
      const kind = asset.kind === "audio" ? "audio" : "video";
      const track = this.sequence.tracks.find((t) => t.kind === kind && !t.locked);
      if (!track) throw new Error("no hay pista compatible");
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
    return null; // el mock dibuja su propio preview
  }

  async pickSavePath(): Promise<string | null> {
    return null; // solo escritorio
  }

  async exportVideo(): Promise<string> {
    throw new Error("Exportar requiere la app de escritorio (npx tauri dev)");
  }

  // el mock no reproduce audio: la UI usa su reloj local (rAF)
  async playbackPlay(): Promise<void> {
    throw new Error("sin audio en navegador");
  }
  async playbackPause(): Promise<number> {
    throw new Error("sin audio en navegador");
  }
  async playbackSeek(): Promise<void> {}
  async playbackPosition(): Promise<[number, boolean]> {
    throw new Error("sin audio en navegador");
  }
  async onStateChanged(): Promise<() => void> {
    return () => {};
  }

  async saveProject(): Promise<string> {
    throw new Error("Guardar requiere la app de escritorio (npx tauri dev)");
  }
  async openProject(): Promise<StateSnapshot> {
    throw new Error("Abrir requiere la app de escritorio (npx tauri dev)");
  }
  async pickProjectSavePath(): Promise<string | null> {
    return null;
  }
  async pickProjectOpenPath(): Promise<string | null> {
    return null;
  }
  async playbackFrame(): Promise<Uint8Array | null> {
    return null;
  }

  async getEffectsCatalog(): Promise<EffectDef[]> {
    // los mismos manifests que embebe el backend (fuente única de verdad)
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
    return this.transaction("Editar efectos", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip no encontrado");
      found.clip.effects = effects;
    });
  }

  async setClipTransition(clipId: Id, transition: TransitionRef | null): Promise<StateSnapshot> {
    return this.transaction("Editar transición", () => {
      const found = this.locate(clipId);
      if (!found) throw new Error("clip no encontrado");
      found.clip.transition_in = transition;
    });
  }

  async moveRange(): Promise<StateSnapshot> {
    throw new Error("Mover texto requiere la app de escritorio (npx tauri dev)");
  }

  async cutRanges(): Promise<StateSnapshot> {
    throw new Error("La edición por texto requiere la app de escritorio (npx tauri dev)");
  }

  async addAvatarClip(): Promise<StateSnapshot> {
    throw new Error("El avatar requiere la app de escritorio (npx tauri dev)");
  }
  async pickAvatarConfig(): Promise<string | null> {
    return null;
  }

  async generateVertical(): Promise<StateSnapshot> {
    return this.transaction("Generar vertical", () => {
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
    return this.transaction("Cambiar secuencia", () => {
      if (!this.project.sequences.some((s) => s.id === sequenceId))
        throw new Error("secuencia no encontrada");
      this.project.active_sequence = sequenceId;
    });
  }

  async addSubtitlesClip(clipId: Id): Promise<StateSnapshot> {
    return this.transaction("Subtítulos automáticos", () => {
      const found = this.locate(clipId);
      if (!found || found.clip.payload.type !== "media") throw new Error("clip inválido");
      const assetId = found.clip.payload.asset_id;
      const doc = this.project.transcripts.find((t) => t.asset_id === assetId);
      if (!doc) throw new Error("el medio no tiene transcripción");
      const track = [...this.sequence.tracks].reverse().find((t) => t.kind === "video" && !t.locked);
      if (!track) throw new Error("no hay pista de video");
      const collides = track.clips.some(
        (c) => c.start < found.clip.start + found.clip.duration && found.clip.start < c.start + c.duration,
      );
      if (collides) throw new Error("la pista superior está ocupada en ese rango");
      track.clips.push({
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
        label_color: null,
      });
      track.clips.sort((a, b) => a.start - b.start);
    });
  }

  async transcribeAsset(): Promise<void> {
    throw new Error("Transcribir requiere la app de escritorio (npx tauri dev)");
  }

  async removeSilences(): Promise<{ removed: number; removed_us: number; snapshot: StateSnapshot }> {
    throw new Error("Eliminar silencios requiere la app de escritorio (npx tauri dev)");
  }

  async mcpStatus(): Promise<number | null> {
    return null;
  }

  async cancelExport(): Promise<void> {}
  async onExportProgress(): Promise<() => void> {
    return () => {};
  }

  async addTextClip(content: string, atUs: TimeUs): Promise<StateSnapshot> {
    return this.transaction("Añadir título", () => {
      const track = [...this.sequence.tracks]
        .reverse()
        .find((t) => t.kind === "video" && !t.locked);
      if (!track) throw new Error("no hay pista de video");
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

  async setClipText(clipId: Id, content: string, style: TextStyle): Promise<StateSnapshot> {
    return this.transaction("Editar texto", () => {
      const found = this.locate(clipId);
      if (!found || found.clip.payload.type !== "text") throw new Error("no es un clip de texto");
      found.clip.payload.content = content;
      found.clip.payload.style = style;
    });
  }

  async setTrackProp(
    trackId: Id,
    prop: "muted" | "solo" | "locked",
    value: boolean,
  ): Promise<StateSnapshot> {
    return this.transaction("Pista", () => {
      const track = this.sequence.tracks.find((t) => t.id === trackId);
      if (!track) throw new Error("pista no encontrada");
      track[prop] = value;
    });
  }
}

// ---------------------------------------------------------------------------
// Proyecto demo (misma forma que ue-core)
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
    label_color: null,
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
    label_color: null,
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
  const cam = makeAsset("video", "media/intro_camara.mp4", 28);
  const gameplay = makeAsset("video", "media/gameplay_fisicas.mp4", 84, {
    fps: [60, 1],
    width: 2560,
    height: 1440,
    vcodec: "hevc",
  });
  const screen = makeAsset("video", "media/pantalla_codigo.mp4", 152, {
    audio_channels: 0,
    acodec: null,
  });
  const voz = makeAsset("audio", "media/voz_off.wav", 58, { acodec: "pcm_s16le", audio_channels: 1 });
  const musica = makeAsset("audio", "media/musica_lofi.mp3", 130, { acodec: "mp3" });
  const logo = makeAsset("image", "media/logo_canal.png", 0);

  const seq: Sequence = {
    id: newId("seq"),
    name: "Principal",
    resolution: [1920, 1080],
    fps: [30, 1],
    sample_rate: 48000,
    markers: [
      { id: newId("mk"), t: 8.5 * S, name: "Demo código", color: "#6fa3b5" },
      { id: newId("mk"), t: 22.5 * S, name: "Gameplay", color: "#8fb573" },
    ],
    tracks: [
      track(
        "audio",
        "A2",
        [mediaClip(musica, 0, 46, 0, { audio: { gain_db: -14, fade_in_us: 1.5 * S, fade_out_us: 3 * S } })],
        -12,
      ),
      track("audio", "A1", [
        mediaClip(voz, 0, 7.5, 0.8),
        mediaClip(voz, 8.1, 16.4, 8.3),
        mediaClip(voz, 17.0, 29.2, 16.7),
        mediaClip(voz, 30.1, 41.0, 29.0),
      ]),
      track("video", "V1", [
        mediaClip(cam, 2, 10.5, 0),
        mediaClip(screen, 12, 26, 8.5),
        mediaClip(gameplay, 5, 19.5, 22.5),
        mediaClip(cam, 14, 22, 37),
      ]),
      track("video", "V2", [
        textClip("CÓMO HICE UN MOTOR DE FÍSICAS", 1.2, 4.4),
        textClip("suscríbete →", 30, 3.5),
      ]),
    ],
  };

  // transcripción demo de la voz en off (para el panel de Texto)
  const frases = [
    "hola a todos bienvenidos a un nuevo devlog",
    "hoy vamos a construir un motor de físicas desde cero",
    "eee bueno primero lo primero las colisiones",
  ];
  const words: Project["transcripts"][number]["words"] = [];
  const segments: Project["transcripts"][number]["segments"] = [];
  let t = 300_000;
  for (const frase of frases) {
    const from = words.length;
    for (const palabra of frase.split(" ")) {
      const dur = 180_000 + palabra.length * 30_000;
      words.push({ text: palabra, start_us: t, end_us: t + dur, confidence: 0.95, rejected: false });
      t += dur + 60_000;
    }
    segments.push({
      text: frase,
      start_us: words[from].start_us,
      end_us: words[words.length - 1].end_us,
      word_range: [from, words.length],
      emotion: null,
      volume_rms: 0,
    });
    t += 1_200_000; // pausa entre frases
  }
  const vozTranscript: Project["transcripts"][number] = {
    id: newId("tr"),
    asset_id: voz.id,
    language: "es",
    model: "base",
    words,
    segments,
    global_avg_volume: 0,
  };
  voz.transcript = vozTranscript.id;

  return {
    schema_version: 1,
    id: newId("proj"),
    name: "Devlog 12 — Motor de físicas",
    created_at: "",
    settings: { whisper_language: "auto", autosave_secs: 60 },
    assets: [cam, gameplay, screen, voz, musica, logo],
    transcripts: [vozTranscript],
    sequences: [seq],
    active_sequence: seq.id,
  };
}
