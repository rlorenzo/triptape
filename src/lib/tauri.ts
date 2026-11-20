import { convertFileSrc, invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { simulateDemoIngestBackground } from "./store";
import { formatDay } from "./trip";

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
  /** The book's print-check results in plain words. Absent in records saved before they existed. */
  bookNotes?: string[];
}

export const REELS_DISCLOSURE =
  "Reels are made for sharing. Watch them first and post only what you're comfortable sharing, especially videos with kids or other people in them.";

// `isTauri` reads the runtime flag Tauri v2 always injects; `window.__TAURI__`
// only exists with `app.withGlobalTauri`, which this app does not enable.
export { isTauri };

// --- Demo state (browser fallback) -----------------------------------------

/**
 * Browser preview only: `?demo=<name>` (repeatable) switches on a scenario
 * so states the default demo trip never reaches can be shown: `thin` (too few
 * photos for a book), `soft` (book pages with soft photos), `missing`
 * (originals moved or changed before a run).
 */
function demoScenario(name: string): boolean {
  try {
    return new URLSearchParams(window.location.search).getAll("demo").includes(name);
  } catch {
    return false;
  }
}

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

async function getAssets(projectId: string, page = 0, perPage = 100): Promise<AssetView[]> {
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

/** Play a finished film in the default player. The backend opens only a finished film. */
export async function playFilm(path: string): Promise<void> {
  if (!isTauri()) throw new Error("Opening files works in the desktop app, not this browser preview.");
  await invoke("open_film", { path });
}

// Browser preview: where a "picked" folder lands, so the chosen-folder state can be shown.
const DEMO_DEFAULT_FOLDER = "~/Movies/TripTape";
const DEMO_PICKED_FOLDER = "/Users/demo/Desktop/Family keepsakes";

/**
 * The folder exports go into: `folder` checked by the backend (it must be a
 * folder TripTape can write to, or can create), or the default when null.
 * Rejects with a plain-words reason when the folder can't be used.
 */
export async function checkExportFolder(folder: string | null): Promise<string> {
  if (isTauri()) return invoke("check_export_folder", { folder });
  return folder ?? DEMO_DEFAULT_FOLDER;
}

/** Ask for a folder in the OS dialog; null when the owner closes it. */
export async function pickFolder(title: string): Promise<string | null> {
  if (!isTauri()) return DEMO_PICKED_FOLDER;
  const { open } = await import("@tauri-apps/plugin-dialog");
  const picked = await open({ directory: true, multiple: false, title });
  return typeof picked === "string" ? picked : null;
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

/** The export a quit or crash stopped on this trip, as its job kind. */
export async function getInterruptedExport(projectId: string): Promise<ExportCmd | null> {
  if (!isTauri()) return null;
  const job = await invoke<ProgressEvent | null>("get_interrupted_export", { projectId });
  if (!job) return null;
  const byKind: Record<string, ExportCmd> = {
    "render-video": "export_video",
    "render-reels": "export_reels",
    "render-book": "export_book"
  };
  return byKind[job.kind] ?? "export_all";
}

export async function dismissInterruptedExport(projectId: string): Promise<void> {
  if (isTauri()) await invoke("dismiss_interrupted_export", { projectId });
}

export type ExportCmd = "export_video" | "export_reels" | "export_book" | "export_all";

export interface KeepsakeEligibility {
  enabled: boolean;
  /** Why it can't be made, or why the film will be the short version. */
  reason: string;
}

export interface Eligibility {
  video: KeepsakeEligibility;
  reels: KeepsakeEligibility;
  book: KeepsakeEligibility;
}

/** One original that failed the check before a run; only its file name is known here. */
export interface SourceProblem {
  assetId: string;
  name: string;
}

export interface SourceCheck {
  missing: SourceProblem[];
  changed: SourceProblem[];
  checked: number;
}

const KEEPSAKE_OF: Record<ExportCmd, string> = {
  export_video: "video",
  export_reels: "reels",
  export_book: "book",
  export_all: "all"
};

// Browser preview of `?demo=missing`: Relink finds the missing files once.
let demoRelinked = false;

/** Check the originals an export will read: missing ones, and ones changed since the scan. */
export async function checkSources(projectId: string, cmd: ExportCmd): Promise<SourceCheck> {
  if (isTauri()) return invoke("check_sources", { projectId, keepsake: KEEPSAKE_OF[cmd] });
  if (!demoScenario("missing")) return { missing: [], changed: [], checked: demoAssets.length };
  const missing = demoRelinked ? [] : ["IMG_0412.HEIC", "IMG_0413.HEIC", "IMG_0420.MOV"];
  return {
    missing: missing.map((name, i) => ({ assetId: `demo_m${i}`, name })),
    changed: [{ assetId: "demo_c0", name: "IMG_0398.JPG" }],
    checked: demoAssets.length
  };
}

export interface RelinkResult {
  found: number;
  notFound: number;
}

/** Look for the trip's missing originals in `folder` and use them where they are now. */
export async function relinkSources(projectId: string, folder: string): Promise<RelinkResult> {
  if (isTauri()) return invoke("relink_sources", { projectId, folder });
  await new Promise((r) => setTimeout(r, 400));
  demoRelinked = true;
  return { found: 3, notFound: 0 };
}

/** Which keepsakes this trip can make, decided from its plan before any run. */
export async function getExportEligibility(projectId: string): Promise<Eligibility> {
  if (isTauri()) return invoke("get_export_eligibility", { projectId });
  const ok = { enabled: true, reason: "" };
  return {
    video: ok,
    reels: ok,
    // The backend's reason, word for word.
    book: demoScenario("thin") ? { enabled: false, reason: "Add more photos for a printable book (24-page vendor minimum)" } : ok
  };
}

export type BookPresetId = "P1" | "P2";

/** One book shape, with the page limits the pages field follows. */
export interface BookPreset {
  id: BookPresetId;
  name: string;
  minPages: number;
  /** The page cap used when the owner doesn't set one. */
  defaultPages: number;
  maxPages: number;
}

/** Browser preview: the backend's two shapes, as `book_presets()` returns them. */
export const DEMO_BOOK_PRESETS: BookPreset[] = [
  { id: "P1", name: "Classic Square 12x12", minPages: 24, defaultPages: 60, maxPages: 400 },
  { id: "P2", name: "Travel Landscape 11x8.5", minPages: 24, defaultPages: 80, maxPages: 400 }
];

export async function getBookPresets(): Promise<BookPreset[]> {
  if (isTauri()) return invoke("get_book_presets");
  return DEMO_BOOK_PRESETS;
}

export interface ExportChoices {
  /** Caps the book's pages (book and all only). */
  bookPages?: number;
  /** The book's shape (book and all only); omitted means P1. */
  bookPreset?: BookPresetId;
  /** Live Photo motion in the film's collages (film and all only; omitted means on). */
  liveCollages?: boolean;
  /** The folder the trip's export folder goes inside; omitted uses the default. */
  destination?: string;
}

export async function exportKeepsake(cmd: ExportCmd, projectId: string, choices: ExportChoices = {}): Promise<ExportSummary> {
  const withBook = cmd === "export_book" || cmd === "export_all";
  const withFilm = cmd === "export_video" || cmd === "export_all";
  if (isTauri()) {
    return invoke(cmd, {
      projectId,
      destination: choices.destination,
      ...(withBook ? { bookPages: choices.bookPages, bookPreset: choices.bookPreset } : {}),
      ...(withFilm ? { liveCollages: choices.liveCollages } : {})
    });
  }
  await new Promise((r) => setTimeout(r, 600));
  return {
    jobId: "demo_job",
    outDir: `${choices.destination ?? DEMO_DEFAULT_FOLDER}/Demo Trip (demo_prj)`,
    video: withFilm ? "hero.mp4" : undefined,
    reels: cmd === "export_reels" || cmd === "export_all" ? ["reel-01.mp4", "reel-02.mp4"] : [],
    bookPages: withBook && !demoScenario("thin") ? 24 : 0,
    skipped:
      cmd === "export_all" && demoScenario("thin")
        ? [{ keepsake: "book", reason: "Add more photos for a printable book (24-page vendor minimum)" }]
        : [],
    // The backend's wording for a book with soft photos and a trimmed selection.
    bookNotes:
      withBook && demoScenario("soft")
        ? [
            "6 pages have a photo at 200-299 dpi and may print a little soft: pages 3, 7, 12, 15, 20, 31.",
            "2 photos were left out because they were too small to print sharply.",
            "The book uses 120 of 180 photos to stay within 60 pages. Allow more pages to fit more."
          ]
        : []
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
      dateLabel: formatDay(items[0]?.takenAt),
      entries: items.map((a, i) => ({ ...a, inHero: i < 3, inReels: i === 0 }))
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
      ch.place = place;
      ch.title = place;
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

export async function setAssetIncluded(projectId: string, assetId: string, included: boolean): Promise<StoryView> {
  if (isTauri()) return invoke("set_asset_included", { projectId, assetId, included });
  return demoEdit((s) => {
    for (const ch of s.chapters) for (const e of ch.entries) if (e.id === assetId) e.inHero = included;
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
  projectId: string;
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
