/** A sliding window of events a minute: the recent times, and whether one
 * more is allowed now. Pure; the room keeps the times in its storage. */
export function take(
  hits: readonly number[],
  now: number,
  perMinute: number,
): { allowed: boolean; hits: number[] } {
  const recent = hits.filter((t) => now - t < 60_000);
  if (recent.length >= perMinute) return { allowed: false, hits: recent };
  return { allowed: true, hits: [...recent, now] };
}
