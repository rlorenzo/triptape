import { convertFileSrc, invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { simulateDemoIngestBackground } from "./store";

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
  place?: string | null;
  score?: number | null;
  highlight: boolean;
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

const demoPlaces = ["Kyoto, Japan", "Osaka, Japan", undefined];

// Deterministic demo scores so the browser preview demonstrates the Best
// filter: a fixed subset is marked highlight, mirroring stored scores.
const demoAssets: AssetView[] = Array.from({ length: 12 }, (_, i) => ({
  id: `demo_a${i.toString().padStart(6, "0")}`,
  kind: i % 4 === 3 ? "video" : "photo",
  width: 4000,
  height: 3000,
  // Four files a day across three days, matching the demo story's chapters.
  takenAt: 1720000000 + Math.floor(i / 4) * 86400 + (i % 4) * 3600,
  timeGuessed: false,
  hasGps: i % 3 !== 0,
  isLivePhoto: false,
  place: i % 3 === 0 ? undefined : demoPlaces[Math.floor(i / 4) % 3],
  score: 0.45 + (i % 5) * 0.1,
  highlight: i % 3 === 0
}));

// One demo Live Photo so the browser preview shows the paired treatment
// (kind string matches the backend `AssetKind::LivePhoto` serialization).
demoAssets[0] = {
  ...demoAssets[0],
  kind: "live-photo",
  durationSecs: 2.5,
  isLivePhoto: true
};

export async function scanFolder(folder: string): Promise<ScanSummary> {
  if (isTauri()) return invoke("scan_folder", { folder });
  await new Promise((r) => setTimeout(r, 400));
  return { projectId: "demo_prj", projectName: "Demo Trip", filesSeen: demoAssets.length, jobId: "demo_job" };
}

/**
 * Re-run the scan for the current trip's folder in place (same project,
 * no folder picker). The backend reuses the stored folder; the raw path
 * never crosses IPC. Demo mode returns the same demo project and drives
 * the nav indicator through the shared background simulation.
 */
export async function rescanProject(projectId: string): Promise<ScanSummary> {
  if (isTauri()) return invoke("rescan_project", { projectId });
  await new Promise((r) => setTimeout(r, 400));
  simulateDemoIngestBackground();
  return { projectId, projectName: "Demo Trip", filesSeen: demoAssets.length, jobId: "demo_job" };
}

export async function getAssets(projectId: string, page = 0, perPage = 100): Promise<AssetView[]> {
  if (isTauri()) return invoke("get_assets", { projectId, page, perPage });
  return demoAssets.slice(page * perPage, page * perPage + perPage);
}

/**
 * Reopen the last trip after a relaunch: the latest project's summary, or
 * null when the store is empty. When unfinished hashing remains, the backend
 * resumes it as a fresh ingest job and returns its id in the summary.
 */
export async function restoreLatestProject(): Promise<ScanSummary | null> {
  if (isTauri()) return invoke("restore_latest_project");
  return null;
}

/** Fetch every page; a short page marks the end. Shared by scan + restore. */
export async function loadAllAssets(projectId: string): Promise<AssetView[]> {
  const PER_PAGE = 500;
  const all: AssetView[] = [];
  for (let page = 0; ; page++) {
    const batch = await getAssets(projectId, page, PER_PAGE);
    all.push(...batch);
    if (batch.length < PER_PAGE) return all;
  }
}

/** Open a finished export in the default app. Tauri scopes this to the export folder. */
export async function openExport(path: string): Promise<void> {
  if (!isTauri()) throw new Error("Opening files works in the desktop app, not this browser preview.");
  const { openPath } = await import("@tauri-apps/plugin-opener");
  await openPath(path);
}

/** Show a finished export in Finder or Explorer. */
export async function revealExport(path: string): Promise<void> {
  if (!isTauri()) throw new Error("Showing files works in the desktop app, not this browser preview.");
  const { revealItemInDir } = await import("@tauri-apps/plugin-opener");
  await revealItemInDir(path);
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

// --- Story editor ---------------------------------------------------------

export interface StoryEntryView extends AssetView {
  caption?: string;
  inHero: boolean;
  inReels: boolean;
}

export interface ChapterView {
  key: string;
  title: string;
  place?: string;
  dayIndex?: number;
  dateLabel?: string;
  entries: StoryEntryView[];
}

export interface StoryView {
  chapters: ChapterView[];
  excluded: StoryEntryView[];
  heroCount: number;
  reelCount: number;
}

// Demo story for the browser preview: three days of the demo assets.
function buildDemoStory(): StoryView {
  const chapters: ChapterView[] = [0, 1, 2].map((c) => {
    const items = demoAssets.slice(c * 4, c * 4 + 4);
    const place = ["Kyoto, Japan", "Osaka, Japan", undefined][c];
    return {
      key: items[0]?.id ?? `demo_c${c}`,
      title: place ?? "At sea",
      place,
      dayIndex: c + 1,
      dateLabel: new Date((items[0]?.takenAt ?? 0) * 1000).toLocaleDateString(undefined, {
        weekday: "short",
        month: "short",
        day: "numeric"
      }),
      entries: items.map((a, i) => ({ ...a, caption: place, inHero: i < 3, inReels: i === 0 }))
    };
  }).filter((c) => c.entries.length > 0);
  return { chapters, excluded: [], heroCount: 9, reelCount: 3 };
}
const demoStory: StoryView = buildDemoStory();

export async function getStory(projectId: string): Promise<StoryView> {
  if (isTauri()) return invoke("get_story", { projectId });
  return structuredClone(demoStory);
}

export async function getAlternatives(projectId: string, assetId: string): Promise<StoryEntryView[]> {
  if (isTauri()) return invoke("get_alternatives", { projectId, assetId });
  const ch = demoStory.chapters.find((c) => c.entries.some((e) => e.id === assetId));
  return structuredClone(ch?.entries ?? []);
}

async function demoEdit(mutate: (s: StoryView) => void): Promise<StoryView> {
  mutate(demoStory);
  return structuredClone(demoStory);
}

export async function setChapterPlace(projectId: string, chapterKey: string, place: string | null): Promise<StoryView> {
  if (isTauri()) return invoke("set_chapter_place", { projectId, chapterKey, place });
  return demoEdit((s) => {
    const ch = s.chapters.find((c) => c.key === chapterKey);
    if (ch && place) {
      const old = ch.place;
      ch.place = place;
      ch.title = place;
      for (const e of ch.entries) if (e.caption === old) e.caption = place;
    }
  });
}

export async function setAssetCaption(projectId: string, assetId: string, caption: string | null): Promise<StoryView> {
  if (isTauri()) return invoke("set_asset_caption", { projectId, assetId, caption });
  return demoEdit((s) => {
    for (const ch of s.chapters) for (const e of ch.entries) if (e.id === assetId) e.caption = caption ?? undefined;
  });
}

export async function setAssetExcluded(projectId: string, assetId: string, excluded: boolean): Promise<StoryView> {
  if (isTauri()) return invoke("set_asset_excluded", { projectId, assetId, excluded });
  return demoEdit((s) => {
    if (excluded) {
      for (const ch of s.chapters) {
        const i = ch.entries.findIndex((e) => e.id === assetId);
        if (i >= 0) s.excluded.push(...ch.entries.splice(i, 1));
      }
    } else {
      const i = s.excluded.findIndex((e) => e.id === assetId);
      if (i >= 0 && s.chapters[0]) s.chapters[0].entries.push(...s.excluded.splice(i, 1));
    }
  });
}

export async function swapAsset(projectId: string, fromAssetId: string, toAssetId: string): Promise<StoryView> {
  if (isTauri()) return invoke("swap_asset", { projectId, fromAssetId, toAssetId });
  return demoEdit((s) => {
    for (const ch of s.chapters) {
      const from = ch.entries.find((e) => e.id === fromAssetId);
      const to = ch.entries.find((e) => e.id === toAssetId);
      if (from && to) {
        const wasHero = from.inHero;
        const wasReels = from.inReels;
        from.inHero = false;
        from.inReels = false;
        to.inHero = wasHero;
        to.inReels = wasReels;
      }
    }
  });
}

// Browser preview only: flat synthetic scenes stand in for real thumbnails.
function demoThumbnail(assetId: string): string {
  const scenes = [
    ["#f7b267", "#f4845f", "#22577a"],
    ["#9bd1e5", "#3a86a8", "#e9d8a6"],
    ["#264d36", "#5f9e5a", "#d7c98a"],
    ["#ef476f", "#ffd166", "#118ab2"],
    ["#1b3a5c", "#5c7fa3", "#2c2c34"],
    ["#ffe8c7", "#e07a5f", "#3d405b"]
  ];
  const n = [...assetId].reduce((sum, c) => sum + c.charCodeAt(0), 0);
  const [a, b, c] = scenes[n % scenes.length];
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="320" height="240"><rect width="320" height="240" fill="${a}"/><rect y="110" width="320" height="70" fill="${b}"/><rect y="180" width="320" height="60" fill="${c}"/></svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

const MAX_THUMB_JOBS = 4;
let activeThumbJobs = 0;
const thumbQueue: (() => void)[] = [];

/**
 * Run `job` once a slot is free: each cold thumbnail spawns a decoder.
 * Queued requests run newest-first, so tiles scrolled into view jump ahead
 * of stale requests queued while scrolling past. Running jobs are never
 * preempted; every queued request still runs once scrolling settles.
 */
async function withThumbSlot<T>(job: () => Promise<T>): Promise<T> {
  if (activeThumbJobs < MAX_THUMB_JOBS) activeThumbJobs++;
  else await new Promise<void>((resolve) => thumbQueue.push(resolve));
  try {
    return await job();
  } finally {
    const next = thumbQueue.pop();
    if (next) next(); // hands this slot over
    else activeThumbJobs--;
  }
}

const thumbsInFlight = new Map<string, Promise<string | null>>();

/**
 * 320 px editor thumbnail as an `asset://` URL, or null. Only in-flight requests
 * are shared: the backend re-checks the source each time, so edits show up.
 */
export function getThumbnail(projectId: string, assetId: string): Promise<string | null> {
  if (!isTauri()) return Promise.resolve(demoThumbnail(assetId));
  const key = `${projectId}/${assetId}`;
  let hit = thumbsInFlight.get(key);
  if (!hit) {
    hit = withThumbSlot(() => invoke<string | null>("get_thumbnail", { projectId, assetId }))
      .then((path) => (path ? convertFileSrc(path) : null))
      .catch(() => null)
      .finally(() => thumbsInFlight.delete(key));
    thumbsInFlight.set(key, hit);
  }
  return hit;
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
