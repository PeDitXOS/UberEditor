import { create } from "zustand";

import type { EngineClient } from "../engine/client";
import { MockEngine, demoProject } from "../engine/mock";
import { TauriEngine, isTauri } from "../engine/tauri";
import type {
  AudioProps,
  EffectDef,
  EffectInstance,
  Id,
  Project,
  StateSnapshot,
  TimeUs,
  TextStyle,
  Transform2D,
  TransitionRef,
} from "../engine/types";
import { activeSequence } from "../engine/types";

const mockEngine = isTauri() ? null : new MockEngine(demoProject());
export const engine: EngineClient = mockEngine ?? new TauriEngine();

export interface UiState {
  ready: boolean;
  project: Project;
  version: number;
  dirty: boolean;
  canUndo: boolean;
  canRedo: boolean;
  selection: Id[];
  playheadUs: TimeUs;
  playing: boolean;
  /** true = la posición la dicta el reloj de audio del backend */
  engineClock: boolean;
  viewStartUs: TimeUs;
  pxPerSec: number;
  lastActionLabel?: string;

  init: () => Promise<void>;
  seek: (us: TimeUs) => void;
  select: (ids: Id[], additive?: boolean) => void;
  togglePlay: () => void;
  setView: (viewStartUs: TimeUs, pxPerSec: number) => void;
  splitAtPlayhead: () => Promise<void>;
  deleteSelection: (ripple: boolean) => Promise<void>;
  moveClip: (clipId: Id, toTrackId: Id, toStartUs: TimeUs) => Promise<void>;
  trimClip: (clipId: Id, left: boolean, newEdgeUs: TimeUs) => Promise<void>;
  setClipAudio: (clipId: Id, audio: AudioProps) => Promise<void>;
  setClipTransform: (clipId: Id, transform: Transform2D) => Promise<void>;
  importMedia: () => Promise<void>;
  addClipFromAsset: (assetId: Id) => Promise<void>;
  exporting: boolean;
  exportProgress: number | null;
  exportVideo: () => Promise<void>;
  cancelExport: () => Promise<void>;
  saveProject: () => Promise<void>;
  openProject: () => Promise<void>;
  mcpPort: number | null;
  effectsCatalog: EffectDef[];
  setClipEffects: (clipId: Id, effects: EffectInstance[]) => Promise<void>;
  setClipTransition: (clipId: Id, transition: TransitionRef | null) => Promise<void>;
  reloadEffectPacks: () => Promise<void>;
  addTextClip: () => Promise<void>;
  removeSilences: (clipId: Id, mode: "delete" | "speedup") => Promise<void>;
  setClipSpeed: (clipId: Id, speed: number) => Promise<void>;
  transcribeAsset: (assetId: Id) => Promise<void>;
  addSubtitlesClip: (clipId: Id) => Promise<void>;
  generateVertical: () => Promise<void>;
  addAvatarClip: (clipId: Id) => Promise<void>;
  setActiveSequence: (sequenceId: Id) => Promise<void>;
  cutTimelineRanges: (ranges: [TimeUs, TimeUs][]) => Promise<void>;
  moveTimelineRange: (fromUs: TimeUs, toUs: TimeUs, destUs: TimeUs) => Promise<void>;
  setClipText: (clipId: Id, content: string, style: TextStyle) => Promise<void>;
  setSubtitlesProps: (
    clipId: Id,
    style: TextStyle,
    mode: "phrase" | "word" | "karaoke",
  ) => Promise<void>;
  toggleTrack: (trackId: Id, prop: "muted" | "solo" | "locked") => Promise<void>;
  undo: () => Promise<void>;
  redo: () => Promise<void>;
}

/** El proyecto vacío inicial se sustituye en init() con el estado del engine. */
const emptyProject: Project = {
  schema_version: 1,
  id: "pending",
  name: "Cargando…",
  created_at: "",
  settings: { whisper_language: "auto", autosave_secs: 60 },
  assets: [],
  transcripts: [],
  sequences: [
    {
      id: "seq_pending",
      name: "Principal",
      resolution: [1920, 1080],
      fps: [30, 1],
      sample_rate: 48000,
      tracks: [],
      markers: [],
    },
  ],
  active_sequence: "seq_pending",
};

export const useStore = create<UiState>((set, get) => {
  const applySnapshot = (snap: StateSnapshot, label?: string) =>
    set({
      project: snap.project,
      version: snap.version,
      dirty: snap.dirty,
      canUndo: snap.can_undo,
      canRedo: snap.can_redo,
      lastActionLabel: label,
      ready: true,
    });

  /** Ejecuta una op del engine; los errores van a la barra de estado. */
  const run = async (label: string, op: () => Promise<StateSnapshot>) => {
    try {
      applySnapshot(await op(), label);
    } catch (e) {
      set({ lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}` });
    }
  };

  return {
    ready: false,
    project: emptyProject,
    version: -1,
    dirty: false,
    canUndo: false,
    canRedo: false,
    selection: [],
    playheadUs: 12_400_000,
    playing: false,
    engineClock: false,
    viewStartUs: 0,
    pxPerSec: 26,
    lastActionLabel: undefined,

    init: async () => {
      applySnapshot(await engine.getState());
      // refrescar cuando el backend termina jobs (conformado, etc.)
      void engine.onStateChanged(async () => {
        applySnapshot(await engine.getState());
      });
      void engine.onExportProgress((p) => set({ exportProgress: p }));
      try {
        set({ effectsCatalog: await engine.getEffectsCatalog() });
      } catch {
        /* sin catálogo: el panel de efectos queda vacío */
      }
      try {
        set({ mcpPort: await engine.mcpStatus() });
      } catch {
        /* sin MCP */
      }
    },

    seek: (us) => {
      const clamped = Math.max(0, us);
      set({ playheadUs: clamped });
      // mantener el reloj de audio alineado si está sonando
      if (engine.kind === "tauri" && get().playing) {
        void engine.playbackSeek(clamped).catch(() => {});
      }
    },

    select: (ids, additive = false) =>
      set((s) => ({ selection: additive ? [...new Set([...s.selection, ...ids])] : ids })),

    togglePlay: () => {
      const s = get();
      if (engine.kind !== "tauri") {
        set({ playing: !s.playing, engineClock: false });
        return;
      }
      if (!s.playing) {
        engine
          .playbackPlay(s.playheadUs)
          .then(() => set({ playing: true, engineClock: true }))
          .catch((e) => {
            // sin dispositivo de audio → reloj local silencioso
            set({
              playing: true,
              engineClock: false,
              lastActionLabel: `⚠ audio no disponible: ${e instanceof Error ? e.message : e}`,
            });
          });
      } else {
        set({ playing: false });
        engine
          .playbackPause()
          .then((t) => set({ playheadUs: t }))
          .catch(() => {});
      }
    },

    setView: (viewStartUs, pxPerSec) =>
      set({
        viewStartUs: Math.max(0, viewStartUs),
        pxPerSec: Math.min(600, Math.max(2, pxPerSec)),
      }),

    splitAtPlayhead: async () => {
      const { playheadUs, selection, project } = get();
      const seq = activeSequence(project);
      const candidates = seq.tracks
        .flatMap((t) => t.clips)
        .filter((c) => c.start < playheadUs && playheadUs < c.start + c.duration)
        .filter((c) => selection.length === 0 || selection.includes(c.id));
      if (!candidates.length) return;
      const newSelection: Id[] = [];
      for (const c of candidates) {
        try {
          const snap = await engine.splitClip(c.id, playheadUs);
          applySnapshot(snap, "Dividir clip");
        } catch {
          /* clip no divisible en ese punto */
        }
      }
      set({ selection: newSelection });
    },

    deleteSelection: async (ripple) => {
      const { selection } = get();
      if (!selection.length) return;
      await run(ripple ? "Eliminar (ripple)" : "Eliminar", () =>
        engine.deleteClips(selection, ripple),
      );
      set({ selection: [] });
    },

    moveClip: (clipId, toTrackId, toStartUs) =>
      run("Mover clip", () => engine.moveClip(clipId, toTrackId, toStartUs, false)),

    trimClip: (clipId, left, newEdgeUs) =>
      run("Recortar clip", () => engine.trimClip(clipId, left, newEdgeUs)),

    setClipAudio: (clipId, audio) =>
      run("Editar audio", () => engine.setClipAudio(clipId, audio)),

    setClipTransform: (clipId, transform) =>
      run("Editar transformación", () => engine.setClipTransform(clipId, transform)),

    importMedia: async () => {
      const paths = await engine.pickMediaFiles();
      if (!paths) {
        set({ lastActionLabel: "⚠ Importar requiere la app de escritorio (npx tauri dev)" });
        return;
      }
      await run(`Importar ${paths.length} archivo(s)`, () => engine.importMedia(paths));
    },

    addClipFromAsset: (assetId) =>
      run("Añadir clip", () => engine.addClip(assetId, get().playheadUs)),

    exporting: false,
    exportProgress: null,
    exportVideo: async () => {
      try {
        const name = `${get().project.name.replace(/[^\p{L}\p{N} _-]/gu, "").trim() || "export"}.mp4`;
        const path = await engine.pickSavePath(name);
        if (!path) {
          if (engine.kind === "mock")
            set({ lastActionLabel: "⚠ Exportar requiere la app de escritorio (npx tauri dev)" });
          return;
        }
        set({ exporting: true, exportProgress: 0, lastActionLabel: "Exportando…" });
        const written = await engine.exportVideo(path);
        set({ exporting: false, exportProgress: null, lastActionLabel: `Exportado a ${written}` });
      } catch (e) {
        set({
          exporting: false,
          exportProgress: null,
          lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}`,
        });
      }
    },

    cancelExport: async () => {
      try {
        await engine.cancelExport();
      } catch {
        /* sin export en curso */
      }
    },

    mcpPort: null,
    effectsCatalog: [],
    setClipEffects: (clipId, effects) =>
      run("Editar efectos", () => engine.setClipEffects(clipId, effects)),
    setClipTransition: (clipId, transition) =>
      run("Editar transición", () => engine.setClipTransition(clipId, transition)),

    moveTimelineRange: async (fromUs, toUs, destUs) => {
      const seqId = activeSequence(get().project).id;
      await run("Mover rango por texto", () =>
        engine.moveRange(seqId, fromUs, toUs, destUs),
      );
    },

    cutTimelineRanges: async (ranges) => {
      if (!ranges.length) return;
      const seqId = activeSequence(get().project).id;
      await run(`Cortar ${ranges.length} rango(s) por texto`, () =>
        engine.cutRanges(seqId, ranges, true),
      );
    },

    generateVertical: () => run("Generar vertical", () => engine.generateVertical()),

    addAvatarClip: async (clipId) => {
      const path = await engine.pickAvatarConfig();
      if (!path) {
        if (engine.kind === "mock")
          set({ lastActionLabel: "⚠ El avatar requiere la app de escritorio (npx tauri dev)" });
        return;
      }
      await run("Añadir avatar", () => engine.addAvatarClip(clipId, path));
    },
    setActiveSequence: (sequenceId) =>
      run("Cambiar secuencia", () => engine.setActiveSequence(sequenceId)),

    addSubtitlesClip: (clipId) =>
      run("Subtítulos automáticos", () => engine.addSubtitlesClip(clipId)),

    transcribeAsset: async (assetId) => {
      try {
        await engine.transcribeAsset(assetId);
        set({
          lastActionLabel:
            "Transcribiendo en segundo plano… (descarga el modelo la primera vez)",
        });
      } catch (e) {
        set({ lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}` });
      }
    },

    removeSilences: async (clipId, mode) => {
      try {
        set({ lastActionLabel: "Analizando silencios…" });
        const r = await engine.removeSilences(clipId, mode);
        const secs = (r.removed_us / 1e6).toFixed(1);
        applySnapshot(
          r.snapshot,
          r.removed > 0
            ? mode === "speedup"
              ? `Acelerados ${r.removed} silencios 4× (${secs} s)`
              : `Eliminados ${r.removed} silencios (${secs} s)`
            : "No se encontraron silencios",
        );
      } catch (e) {
        set({ lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}` });
      }
    },

    setClipSpeed: (clipId, speed) =>
      run(`Velocidad ${speed}×`, () => engine.setClipSpeed(clipId, speed)),

    addTextClip: async () => {
      await run("Añadir título", () => engine.addTextClip("Título", get().playheadUs));
    },
    setClipText: (clipId, content, style) =>
      run("Editar texto", () => engine.setClipText(clipId, content, style)),
    setSubtitlesProps: (clipId, style, mode) =>
      run("Editar subtítulos", () => engine.setSubtitlesProps(clipId, style, mode)),

    reloadEffectPacks: async () => {
      try {
        const r = await engine.reloadEffectPacks();
        set({
          effectsCatalog: r.catalog,
          lastActionLabel: r.errors.length
            ? `⚠ packs con errores: ${r.errors.join("; ")}`
            : `Packs recargados (${r.catalog.length})${r.dir ? ` desde ${r.dir}` : ""}`,
        });
      } catch (e) {
        set({ lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}` });
      }
    },

    toggleTrack: async (trackId, prop) => {
      const track = activeSequence(get().project).tracks.find((t) => t.id === trackId);
      if (!track) return;
      await run("Pista", () => engine.setTrackProp(trackId, prop, !track[prop]));
    },

    saveProject: async () => {
      try {
        let written: string;
        try {
          written = await engine.saveProject(null);
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e);
          if (!msg.includes("no hay ruta")) throw e;
          const name = `${get().project.name.replace(/[^\p{L}\p{N} _-]/gu, "").trim() || "proyecto"}.uep`;
          const path = await engine.pickProjectSavePath(name);
          if (!path) return;
          written = await engine.saveProject(path);
        }
        applySnapshot(await engine.getState(), `Guardado en ${written}`);
      } catch (e) {
        set({ lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}` });
      }
    },

    openProject: async () => {
      try {
        const path = await engine.pickProjectOpenPath();
        if (!path) {
          if (engine.kind === "mock")
            set({ lastActionLabel: "⚠ Abrir requiere la app de escritorio (npx tauri dev)" });
          return;
        }
        const snap = await engine.openProject(path);
        set({ selection: [], playheadUs: 0, playing: false });
        applySnapshot(snap, `Abierto ${path}`);
      } catch (e) {
        set({ lastActionLabel: `⚠ ${e instanceof Error ? e.message : String(e)}` });
      }
    },

    undo: async () => {
      const snap = await engine.undo();
      applySnapshot(snap, "Deshacer");
    },

    redo: async () => {
      const snap = await engine.redo();
      applySnapshot(snap, "Rehacer");
    },
  };
});
