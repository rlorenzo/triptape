import { get } from "svelte/store";
import { exportJob, IDLE_EXPORT } from "./store";
import { exportKeepsake, type ExportCmd, type ExportSummary, type ProgressEvent } from "./tauri";

const key = (projectId: string) => `triptape-export-${projectId}`;

/** The finished keepsakes last recorded for this trip, surviving a relaunch. */
export function savedExport(projectId: string): ExportSummary | null {
  try {
    const raw = localStorage.getItem(key(projectId));
    return raw ? (JSON.parse(raw) as ExportSummary) : null;
  } catch {
    return null; // no storage, or a damaged record: treat as never exported
  }
}

// A single-keepsake export must not forget the other two, but a keepsake that
// was attempted and skipped no longer exists as far as the trip is concerned.
function remember(projectId: string, next: ExportSummary): ExportSummary {
  const prev = savedExport(projectId);
  const skippedForReal = (keepsake: string) =>
    next.skipped.some((s) => s.keepsake === keepsake && !s.reason.startsWith("not selected"));
  const merged: ExportSummary = {
    ...next,
    video: next.video ?? (skippedForReal("video") ? undefined : prev?.video),
    reels: next.reels.length > 0 || skippedForReal("reels") ? next.reels : (prev?.reels ?? []),
    bookPages: next.bookPages || (skippedForReal("book") ? 0 : prev?.bookPages || 0)
  };
  try {
    localStorage.setItem(key(projectId), JSON.stringify(merged));
  } catch {
    /* private mode: the record lasts this session only */
  }
  return merged;
}

/** Run one export; state lands in `exportJob` whichever screen is showing. */
export async function runExport(cmd: ExportCmd, label: string, projectId: string): Promise<void> {
  exportJob.set({ ...IDLE_EXPORT, cmd, label });
  try {
    const summary = remember(projectId, await exportKeepsake(cmd, projectId));
    exportJob.set({ ...IDLE_EXPORT, cmd, summary });
  } catch (e) {
    const message = String(e);
    // The backend reports a user's cancel as this exact error.
    const cancelled = message.includes("export cancelled");
    exportJob.set({
      ...IDLE_EXPORT,
      cmd,
      cancelled,
      error: cancelled ? "" : message,
      summary: savedExport(projectId)
    });
  }
}

/** Apply a non-ingest `job://progress` event to the running export. */
export function applyExportProgress(e: ProgressEvent): void {
  if (!get(exportJob).label) return;
  exportJob.update((j) => ({
    ...j,
    // The backend emits `running` before rendering, so Cancel has a target early.
    jobId: e.state === "running" ? e.id : j.jobId,
    progress: e.progress,
    step: e.currentStep ?? ""
  }));
}
