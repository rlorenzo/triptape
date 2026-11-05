import { writable } from "svelte/store";
import type { AssetView, ScanSummary } from "./tauri";

export type Screen = "import" | "library" | "story" | "export" | "settings";

const CONSENT_KEY = "triptape-consent";

function readConsent(): boolean {
  try {
    return localStorage.getItem(CONSENT_KEY) === "1";
  } catch {
    return false; // no storage (SSR / private mode)
  }
}

export const screen = writable<Screen>("import");
export const project = writable<ScanSummary | null>(null);
export const assets = writable<AssetView[]>([]);
export const consentGiven = writable<boolean>(readConsent());

/** Background ingest job (public surface of `job://progress` for kind `ingest`). */
export interface IngestJobState {
  id: string;
  progress: number;
  step: string;
  state: string;
  error?: string;
}

export const ingestJob = writable<IngestJobState | null>(null);

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
