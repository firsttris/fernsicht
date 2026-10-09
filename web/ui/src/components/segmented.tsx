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
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className={cn("flex gap-0.5 rounded-md border border-border bg-muted p-[3px]", className)}
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={o.value === value}
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
