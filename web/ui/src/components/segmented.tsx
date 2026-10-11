import type { KeyboardEvent } from "react";

import { cn } from "../lib/utils";

export interface SegmentedOption<T extends string> {
  value: T;
  label: string;
}

/** Tab-like single choice (shadcn "Tabs" list look). */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
  className,
}: {
  value: T;
  options: SegmentedOption<T>[];
  onChange: (value: T) => void;
  label: string;
  className?: string;
}) {
  // A radio group is one tab stop; the arrow keys choose within it.
  const step = (e: KeyboardEvent<HTMLDivElement>) => {
    const by = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[e.key];
    if (by === undefined) return;
    e.preventDefault();
    const now = options.findIndex((o) => o.value === value);
    const next = (now + by + options.length) % options.length;
    onChange(options[next]!.value);
    const buttons = e.currentTarget.querySelectorAll<HTMLElement>("[role=radio]");
    buttons[next]?.focus();
  };
  return (
    <div
      role="radiogroup"
      aria-label={label}
      onKeyDown={step}
      className={cn("flex gap-0.5 rounded-md border border-border bg-muted p-[3px]", className)}
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={o.value === value}
          tabIndex={o.value === value ? 0 : -1}
          onClick={() => onChange(o.value)}
          className={cn(
            "h-7 cursor-pointer rounded px-3 text-[13px] text-muted-foreground transition-colors hover:text-foreground",
            o.value === value && "bg-background font-medium text-foreground",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
