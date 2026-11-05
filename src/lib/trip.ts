import type { AssetView } from "./tauri";

/** Oldest first, undated last, id as the tie-break. */
export function byTime(a: AssetView, b: AssetView): number {
  if (a.takenAt == null && b.takenAt == null) return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  if (a.takenAt == null) return 1;
  if (b.takenAt == null) return -1;
  return a.takenAt - b.takenAt || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
}

export function dayKey(takenAt?: number): string {
  if (!takenAt) return "undated";
  const d = new Date(takenAt * 1000);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

/** Backend job steps in the words a parent would use. Unknown steps pass through. */
export function friendlyStep(step: string | undefined): string {
  if (!step) return "";
  return step
    .replace(/^Hashing files/i, "Checking files")
    .replace(/^Scoring photos/i, "Finding the best photos")
    .replace(/^Grouping photos/i, "Sorting into days and places")
    .replace(/^(Building|Refreshing) story plan/i, "Building your story");
}
