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

// Built once: constructing an Intl formatter per call is the slow part, and
// these run per tile and per filter keystroke.
const DAY = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric" });
const TIME = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const WHEN = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
const FULL = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  year: "numeric",
  hour: "2-digit",
  minute: "2-digit"
});

const formatWith = (format: Intl.DateTimeFormat) => (takenAt?: number) =>
  takenAt ? format.format(takenAt * 1000) : "";

/** "Tue, Nov 3" */
export const formatDay = formatWith(DAY);
/** "2:05 PM" */
export const formatTime = formatWith(TIME);
/** "Nov 3, 2:05 PM" */
export const formatWhen = formatWith(WHEN);
/** "Nov 3, 2026, 02:05 PM": everything the Library's search box can match. */
export const formatFull = formatWith(FULL);

export function kindLabel(a: { kind: string; isLivePhoto: boolean }): string {
  if (a.isLivePhoto) return "Live Photo";
  return a.kind === "video" ? "Video" : "Photo";
}

/** "1 file", "2,119 files". Regular plurals only. */
export function countOf(n: number, noun: string): string {
  return `${n.toLocaleString()} ${noun}${n === 1 ? "" : "s"}`;
}
