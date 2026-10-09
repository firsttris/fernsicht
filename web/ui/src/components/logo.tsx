import { Eye } from "lucide-react";

import { cn } from "../lib/utils";

export function Logo({ size = 28, className }: { size?: number; className?: string }) {
  return (
    <span
      className={cn(
        "flex items-center justify-center rounded-md bg-primary text-primary-foreground",
        className,
      )}
      style={{ width: size, height: size }}
      aria-hidden
    >
      <Eye size={Math.round(size * 0.57)} strokeWidth={2.2} />
    </span>
  );
}
