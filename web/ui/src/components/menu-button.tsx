import { type ReactNode, useEffect, useRef, useState } from "react";

import { Button } from "./button";

export interface MenuItem {
  label: string;
  /** Marks the current choice (a radio-style menu). */
  checked?: boolean;
  onSelect: () => void;
}

/** A toolbar icon button that opens a small menu below it. */
export function MenuButton({
  label,
  icon,
  items,
}: {
  label: string;
  icon: ReactNode;
  items: MenuItem[];
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const radio = items.some((i) => i.checked !== undefined);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    const onPointer = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("pointerdown", onPointer);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("pointerdown", onPointer);
    };
  }, [open]);

  return (
    <div ref={root} className="relative">
      <Button
        size="icon"
        variant="ghost"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        {icon}
      </Button>
      {open && (
        <div
          role="menu"
          aria-label={label}
          className="absolute top-full left-1/2 z-10 mt-2 flex min-w-44 -translate-x-1/2 flex-col rounded-lg border border-border bg-overlay p-1 backdrop-blur"
        >
          {items.map((item) => (
            <button
              key={item.label}
              type="button"
              role={radio ? "menuitemradio" : "menuitem"}
              aria-checked={radio ? Boolean(item.checked) : undefined}
              className="cursor-pointer rounded-md border-0 bg-transparent px-3 py-1.5 text-left text-sm whitespace-nowrap text-foreground hover:bg-secondary aria-checked:font-semibold"
              onClick={() => {
                item.onSelect();
                setOpen(false);
              }}
            >
              {item.checked && <span aria-hidden>✓ </span>}
              {item.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
