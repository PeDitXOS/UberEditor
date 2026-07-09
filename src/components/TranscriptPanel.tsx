import { useState } from "react";

import type { TranscriptWord } from "../engine/types";
import { assetName, wordTimelineRange, wordsToCutRanges } from "../engine/types";
import { useStore } from "../state/store";

/**
 * Edición basada en texto (PLAN §7.B): click marca/desmarca palabras,
 * doble click hace seek. "Cortar seleccionadas" corta esos rangos del
 * timeline (con padding y fusión) en una sola acción de deshacer.
 */
export function TranscriptPanel() {
  const project = useStore((s) => s.project);
  const seek = useStore((s) => s.seek);
  const cutTimelineRanges = useStore((s) => s.cutTimelineRanges);
  const moveTimelineRange = useStore((s) => s.moveTimelineRange);
  const playheadUs = useStore((s) => s.playheadUs);
  const [assetSel, setAssetSel] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());

  const transcripts = project.transcripts;
  if (!transcripts.length) {
    return (
      <div className="px-3 py-4 text-[11px] leading-relaxed text-ink-faint">
        <p className="mb-2 font-medium text-ink-dim">Sin transcripciones todavía.</p>
        <p>
          Pulsa el botón <span className="rounded border border-line px-1">T</span> de un medio
          con audio en la pestaña Medios para transcribirlo con Whisper (palabra por palabra).
        </p>
        <p className="mt-2">
          Después podrás editar el video borrando texto: marca palabras y córtalas.
        </p>
      </div>
    );
  }

  const doc =
    transcripts.find((t) => t.asset_id === assetSel) ?? transcripts[0];
  const asset = project.assets.find((a) => a.id === doc.asset_id);

  const toggle = (i: number) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(i)) next.delete(i);
      else next.add(i);
      return next;
    });
  };

  const cutSelected = async () => {
    const words = [...selected].sort((a, b) => a - b).map((i) => doc.words[i]);
    const ranges = wordsToCutRanges(project, doc.asset_id, words);
    await cutTimelineRanges(ranges);
    setSelected(new Set());
  };

  const moveSelectedToPlayhead = async () => {
    const words = [...selected].sort((a, b) => a - b).map((i) => doc.words[i]);
    // sin padding: mover exactamente el material de las palabras
    const ranges = wordsToCutRanges(project, doc.asset_id, words, 0, 150_000);
    if (ranges.length !== 1) {
      useStore.setState({
        lastActionLabel: "⚠ para mover, selecciona palabras contiguas (un solo bloque)",
      });
      return;
    }
    await moveTimelineRange(ranges[0][0], ranges[0][1], playheadUs);
    setSelected(new Set());
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center gap-2 px-3 pb-2 pt-1">
        <select
          className="focus-ring min-w-0 flex-1 cursor-pointer rounded-md border border-line bg-bg2 px-2 py-1 text-[11px] text-ink"
          value={doc.asset_id}
          onChange={(e) => {
            setAssetSel(e.target.value);
            setSelected(new Set());
          }}
          title="Transcripción a mostrar"
        >
          {transcripts.map((t) => (
            <option key={t.id} value={t.asset_id}>
              {assetName(project.assets.find((a) => a.id === t.asset_id))}
            </option>
          ))}
        </select>
      </div>

      <div className="min-h-0 flex-1 select-text overflow-y-auto px-3 pb-2 text-[13px] leading-[1.9]">
        {doc.segments.map((seg, si) => (
          <p key={si} className="mb-2">
            {doc.words.slice(seg.word_range[0], seg.word_range[1]).map((w, k) => {
              const i = seg.word_range[0] + k;
              return (
                <WordSpan
                  key={i}
                  word={w}
                  selected={selected.has(i)}
                  underPlayhead={isUnderPlayhead(project, doc.asset_id, w, playheadUs)}
                  onToggle={() => toggle(i)}
                  onSeek={() => {
                    const r = wordTimelineRange(project, doc.asset_id, w);
                    if (r) seek(r[0]);
                  }}
                />
              );
            })}
          </p>
        ))}
        <div className="mt-2 font-[var(--font-mono)] text-[10px] text-ink-faint">
          {doc.words.length} palabras · modelo {doc.model}
          {asset && ` · ${assetName(asset)}`}
        </div>
      </div>

      <div className="border-t border-line-soft p-2">
        <div className="flex gap-2">
          <button
            className="focus-ring flex-1 rounded-md bg-danger/80 px-2 py-1.5 text-[12px] font-medium text-white enabled:hover:bg-danger disabled:opacity-40"
            disabled={selected.size === 0}
            onClick={() => void cutSelected()}
            title="Corta las palabras marcadas del video (cierra huecos; 1 deshacer)"
          >
            ✂ Cortar {selected.size > 0 ? `${selected.size} palabra(s)` : "selección"}
          </button>
          <button
            className="focus-ring rounded-md border border-line px-2 py-1.5 text-[12px] text-ink-dim enabled:hover:text-ink disabled:opacity-40"
            disabled={selected.size === 0}
            onClick={() => void moveSelectedToPlayhead()}
            title="Mueve las palabras marcadas (contiguas) al playhead"
          >
            ⇢ Mover
          </button>
          <button
            className="focus-ring rounded-md border border-line px-2 py-1.5 text-[12px] text-ink-dim enabled:hover:text-ink disabled:opacity-40"
            disabled={selected.size === 0}
            onClick={() => setSelected(new Set())}
          >
            Limpiar
          </button>
        </div>
        <p className="mt-1.5 text-[10px] leading-snug text-ink-faint">
          Click marca una palabra · doble click salta a ella en el timeline
        </p>
      </div>
    </div>
  );
}

function isUnderPlayhead(
  project: ReturnType<typeof useStore.getState>["project"],
  assetId: string,
  word: TranscriptWord,
  playheadUs: number,
): boolean {
  const r = wordTimelineRange(project, assetId, word);
  return r !== null && playheadUs >= r[0] && playheadUs < r[1];
}

function WordSpan({
  word,
  selected,
  underPlayhead,
  onToggle,
  onSeek,
}: {
  word: TranscriptWord;
  selected: boolean;
  underPlayhead: boolean;
  onToggle: () => void;
  onSeek: () => void;
}) {
  return (
    <span
      className={[
        "cursor-pointer rounded px-0.5",
        selected
          ? "bg-danger/30 text-danger line-through"
          : underPlayhead
            ? "bg-accent/25 text-ink"
            : "text-ink-dim hover:bg-bg3 hover:text-ink",
      ].join(" ")}
      onClick={onToggle}
      onDoubleClick={(e) => {
        e.preventDefault();
        onSeek();
      }}
    >
      {word.text}{" "}
    </span>
  );
}
