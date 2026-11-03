import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// Tauri IPC layer with a graceful browser-preview fallback.
//
// Inside the Tauri webview, commands go to the Rust glue (`src-tauri/`).
// In a plain browser (e.g. `npm run dev` without the shell), every call
// resolves against an in-memory demo state so the UI is still clickable
// end to end. The banner in App.svelte marks this mode explicitly.

export interface AssetView {
  id: string;
  kind: string;
  width?: number;
  height?: number;
  durationSecs?: number;
  takenAt?: number;
  timeGuessed: boolean;
  hasGps: boolean;
  isLivePhoto: boolean;
}

export interface ScanSummary {
  projectId: string;
  projectName: string;
  filesSeen: number;
  jobId: string;
}

export interface ExportSummary {
  jobId: string;
  outDir: string;
  video?: string;
  reels: string[];
  bookPages: number;
  skipped: { keepsake: string; reason: string }[];
}

export const REELS_DISCLOSURE =
  "Reels are made for sharing. Watch them first and post only what you're comfortable sharing, especially videos with kids or other people in them.";

// `isTauri` reads the runtime flag Tauri v2 always injects; `window.__TAURI__`
// only exists with `app.withGlobalTauri`, which this app does not enable.
export { isTauri };

// --- Demo state (browser fallback) -----------------------------------------

const demoAssets: AssetView[] = Array.from({ length: 12 }, (_, i) => ({
  id: `demo_a${i.toString().padStart(6, "0")}`,
  kind: i % 4 === 3 ? "video" : "photo",
  width: 4000,
  height: 3000,
  takenAt: 1720000000 + i * 3600,
  timeGuessed: false,
  hasGps: i % 3 !== 0,
  isLivePhoto: false
}));

export async function scanFolder(folder: string): Promise<ScanSummary> {
  if (isTauri()) return invoke("scan_folder", { folder });
  await new Promise((r) => setTimeout(r, 400));
  return { projectId: "demo_prj", projectName: "Demo Trip", filesSeen: demoAssets.length, jobId: "demo_job" };
}

export async function getAssets(projectId: string, page = 0, perPage = 100): Promise<AssetView[]> {
  if (isTauri()) return invoke("get_assets", { projectId, page, perPage });
  return demoAssets.slice(page * perPage, page * perPage + perPage);
}

export async function cancelJob(jobId: string): Promise<void> {
  if (isTauri()) await invoke("cancel_job", { jobId });
}

export type ExportCmd = "export_video" | "export_reels" | "export_book" | "export_all";

export async function exportKeepsake(
  cmd: ExportCmd,
  projectId: string
): Promise<ExportSummary> {
  if (isTauri()) return invoke(cmd, { projectId });
  await new Promise((r) => setTimeout(r, 600));
  return {
    jobId: "demo_job",
    outDir: `~/Movies/TripTape/Demo Trip/`,
    video: cmd === "export_video" || cmd === "export_all" ? "hero.mp4" : undefined,
    reels: cmd === "export_reels" || cmd === "export_all" ? ["reel-01.mp4", "reel-02.mp4"] : [],
    bookPages: cmd === "export_book" || cmd === "export_all" ? 24 : 0,
    skipped: []
  };
}

/** Mirrors the backend `JobView` payload of `job://progress`. */
export interface ProgressEvent {
  id: string;
  kind: string;
  state: string;
  progress: number;
  currentStep?: string;
  error?: string;
}

/** Subscribe to `job://progress`; returns a synchronous unsubscribe. */
export function onProgress(cb: (e: ProgressEvent) => void): () => void {
  if (!isTauri()) return () => {};
  const pending = listen<ProgressEvent>("job://progress", (e) => cb(e.payload));
  // Unsubscribing before `listen` resolves must still detach the listener.
  return () => void pending.then((unlisten) => unlisten());
}
