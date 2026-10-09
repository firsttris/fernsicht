import { STAGES, type SessionStats } from "../types";
import { cn, formatMs, formatPercent } from "../lib/utils";

/** The stats panel from the session view: glass-to-glass, stage bar, stream info. */
export function LatencyOverlay({ stats, className }: { stats: SessionStats; className?: string }) {
  const total = STAGES.reduce((sum, s) => sum + stats.stagesUs[s.key], 0);
  return (
    <section
      aria-label="Latenz"
      className={cn(
        "flex w-58 flex-col gap-2.5 rounded-[10px] border border-border bg-overlay p-3.5 backdrop-blur",
        className,
      )}
    >
      <div className="flex items-baseline justify-between">
        <span className="text-xs text-muted-foreground">Glass-to-Glass</span>
        <span className="font-mono text-[22px] font-medium tabular-nums">{formatMs(total)}</span>
      </div>
      <div className="flex h-1.5 gap-0.5 overflow-hidden rounded-full" aria-hidden>
        {STAGES.map((s) => (
          <span
            key={s.key}
            className={cn(s.color, "transition-[flex-grow] duration-500")}
            style={{ flexGrow: Math.max(stats.stagesUs[s.key], 1) }}
          />
        ))}
      </div>
      <dl className="grid grid-cols-2 gap-x-3 gap-y-1.5 text-xs">
        {STAGES.map((s) => (
          <div key={s.key} className="contents">
            <dt className="flex items-center gap-1.5 text-muted-foreground">
              <span className={cn("size-1.5 rounded-full", s.color)} aria-hidden />
              {s.label}
            </dt>
            <dd className="m-0 text-right font-mono tabular-nums">
              {formatMs(stats.stagesUs[s.key])}
            </dd>
          </div>
        ))}
      </dl>
      <dl className="grid grid-cols-2 gap-x-3 gap-y-1.5 border-t border-border pt-2.5 text-xs">
        <dt className="text-muted-foreground">Codec</dt>
        <dd className="m-0 text-right font-mono">{stats.codec}</dd>
        <dt className="text-muted-foreground">Bildrate</dt>
        <dd className="m-0 text-right font-mono tabular-nums">{Math.round(stats.fps)} fps</dd>
        <dt className="text-muted-foreground">Bitrate</dt>
        <dd className="m-0 text-right font-mono tabular-nums">
          {Math.round(stats.bitrateBps / 1e6)} Mbit/s
        </dd>
        <dt className="text-muted-foreground">Verlust (FEC)</dt>
        <dd className="m-0 text-right font-mono tabular-nums">
          {formatPercent(stats.lossBeforeFec)} → {formatPercent(stats.lossAfterFec)}
        </dd>
      </dl>
    </section>
  );
}
