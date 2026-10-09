import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** German millisecond format as in the native overlay: "3,4 ms". */
export function formatMs(us: number, decimals = 0): string {
  return `${(us / 1000).toLocaleString("de-DE", {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  })} ms`;
}

/** "0,4 %", exact zero as "0". */
export function formatPercent(ratio: number): string {
  if (ratio <= 0) return "0";
  return `${(ratio * 100).toLocaleString("de-DE", {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  })} %`;
}

/** Device IDs are shown in groups of three digits: "482 913 057". */
export function formatDeviceId(id: string): string {
  return id.replace(/\D/g, "").replace(/(\d{3})(?=\d)/g, "$1 ");
}
