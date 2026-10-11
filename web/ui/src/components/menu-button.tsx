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
  // Opened from the keyboard: the focus goes into the menu (WAI-ARIA menu
  // button pattern); from the mouse it stays where it was.
  const [focusItem, setFocusItem] = useState<number | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const radio = items.some((i) => i.checked !== undefined);

  useEffect(() => {
    if (!open || focusItem === null) return;
    menu.current?.querySelectorAll<HTMLElement>("[role^=menuitem]")[focusItem]?.focus();
  }, [open, focusItem]);

  const close = (refocus: boolean) => {
    setOpen(false);
    setFocusItem(null);
    if (refocus) trigger.current?.focus();
  };

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
        ref={trigger}
        size="icon"
        variant="ghost"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          const at = { ArrowDown: 0, Enter: 0, " ": 0, ArrowUp: items.length - 1 }[e.key];
          if (at === undefined) return;
          e.preventDefault();
          setOpen(true);
          setFocusItem(at);
        }}
      >
        {icon}
      </Button>
      {open && (
        <div
          ref={menu}
          role="menu"
          aria-label={label}
          onKeyDown={(e) => {
            const all = [
              ...(menu.current?.querySelectorAll<HTMLElement>("[role^=menuitem]") ?? []),
            ];
            const now = all.indexOf(document.activeElement as HTMLElement);
            const next = {
              ArrowDown: (now + 1) % all.length,
              ArrowUp: (now - 1 + all.length) % all.length,
              Home: 0,
              End: all.length - 1,
            }[e.key];
            if (next !== undefined) {
              e.preventDefault();
              all[next]?.focus();
            } else if (e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              close(true);
            } else if (e.key === "Tab") {
              close(false);
            }
          }}
          className="absolute top-full left-1/2 z-10 mt-2 flex min-w-44 -translate-x-1/2 flex-col rounded-lg border border-border bg-overlay p-1 backdrop-blur"
        >
          {items.map((item) => (
            <button
              key={item.label}
              type="button"
              role={radio ? "menuitemradio" : "menuitem"}
              aria-checked={radio ? Boolean(item.checked) : undefined}
              className="cursor-pointer rounded-md border-0 bg-transparent px-3 py-1.5 text-left text-sm whitespace-nowrap text-foreground hover:bg-secondary aria-checked:font-semibold"
              tabIndex={-1}
              onClick={() => {
                item.onSelect();
                close(focusItem !== null);
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
