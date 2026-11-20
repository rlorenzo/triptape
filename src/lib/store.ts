import { derived, writable, type Writable } from "svelte/store";
import { byTime, dayKey } from "./trip";
import type { AssetView, ExportCmd, ExportSummary, ScanSummary } from "./tauri";

export type Screen = "import" | "library" | "story" | "export" | "settings";

const CONSENT_KEY = "triptape-consent";

/** A saved setting, or null when unset or storage is unavailable (SSR / private mode). */
function stored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** A store whose every value is saved under `key`, so the choice survives relaunches. */
function remembered<T>(key: string, initial: T): Writable<T> {
  const store = writable<T>(initial);
  store.subscribe((value) => {
    try {
      localStorage.setItem(key, String(value));
    } catch {
      /* private mode: the choice lasts this session */
    }
  });
  return store;
}

export const screen = writable<Screen>("import");
export const project = writable<ScanSummary | null>(null);
export const assets = writable<AssetView[]>([]);

/** Oldest first. A sorted copy, so the `assets` array is never mutated. */
export const assetsByTime = derived(assets, ($assets) => [...$assets].sort(byTime));

/** Frame numbers follow the whole trip, so filters and cuts never renumber a photo. */
export const frameNumbers = derived(
  assetsByTime,
  ($sorted) => new Map($sorted.map((a, i) => [a.id, i + 1]))
);

/** Day numbers follow the whole trip too. */
export const dayNumbers = derived(assetsByTime, ($sorted) => {
  const numbers = new Map<string, number>();
  for (const a of $sorted) {
    const key = dayKey(a.takenAt);
    if (key !== "undated" && !numbers.has(key)) numbers.set(key, numbers.size + 1);
  }
  return numbers;
});

export const consentGiven = writable<boolean>(stored(CONSENT_KEY) === "1");

/** Photo Book page cap bounds; the backend enforces the same range. */
export const BOOK_PAGES_MIN = 24;
export const BOOK_PAGES_MAX = 400;
const BOOK_PAGES_DEFAULT = 60;
const BOOK_PAGES_KEY = "triptape-book-pages";

/** A whole page count inside the allowed range, else null. */
export function validBookPages(n: unknown): number | null {
  return typeof n === "number" && Number.isInteger(n) && n >= BOOK_PAGES_MIN && n <= BOOK_PAGES_MAX ? n : null;
}

/** Most pages the Photo Book may use. Remembered across launches. */
export const bookPages = remembered<number>(
  BOOK_PAGES_KEY,
  validBookPages(Number(stored(BOOK_PAGES_KEY))) ?? BOOK_PAGES_DEFAULT
);

const LIVE_COLLAGES_KEY = "triptape-live-collages";

/** Whether Live Photos move inside the film's collages. On unless turned off. */
export const liveCollages = remembered<boolean>(LIVE_COLLAGES_KEY, stored(LIVE_COLLAGES_KEY) !== "false");

/** Background ingest job (public surface of `job://progress` for kind `ingest`). */
export interface IngestJobState {
  id: string;
  progress: number;
  step: string;
  state: string;
  error?: string;
}

export const ingestJob = writable<IngestJobState | null>(null);

/** Number of story picks, known once the Story screen has loaded them. */
export const storyPicks = writable<number | null>(null);

/**
 * The export in flight or last finished. Lives here, not in the Export screen,
 * so it survives navigating away during a long render.
 */
export interface ExportJobState {
  cmd: ExportCmd | null;
  label: string | null;
  jobId: string;
  progress: number;
  step: string;
  error: string;
  cancelled: boolean;
  summary: ExportSummary | null;
}

export const IDLE_EXPORT: ExportJobState = {
  cmd: null,
  label: null,
  jobId: "",
  progress: 0,
  step: "",
  error: "",
  cancelled: false,
  summary: null
};

export const exportJob = writable<ExportJobState>(IDLE_EXPORT);

// Browser-preview background simulation. Owned at module level (not by the
// Import component) because the app navigates to Library when the demo scan
// "returns" and Import unmounts. The chain is finite and ends in `done`;
// App.svelte clears the completion state, so no timer outlives the run.
let demoTimers: ReturnType<typeof setTimeout>[] = [];

function stopDemoSimulation() {
  for (const t of demoTimers) clearTimeout(t);
  demoTimers = [];
}

export function simulateDemoIngestBackground() {
  stopDemoSimulation();
  ingestJob.set({ id: "demo_job", progress: 0.15, step: "Hashing files… 0 of 12", state: "running" });
  const phases: [number, string][] = [
    [0.4, "Hashing files… 4 of 12"],
    [0.7, "Hashing files… 9 of 12"],
    [0.95, "Refreshing story plan…"]
  ];
  phases.forEach(([progress, step], i) => {
    demoTimers.push(
      setTimeout(() => {
        ingestJob.update((j) => (j ? { ...j, progress, step } : j));
      }, 400 * (i + 1))
    );
  });
  demoTimers.push(
    setTimeout(() => {
      ingestJob.update((j) => (j ? { ...j, progress: 1, step: "done", state: "done" } : j));
    }, 400 * (phases.length + 1))
  );
}

export function giveConsent() {
  try {
    localStorage.setItem(CONSENT_KEY, "1");
  } catch {
    /* private mode: consent lasts the session */
  }
  consentGiven.set(true);
}
