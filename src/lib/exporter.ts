import { get } from "svelte/store";
import { exportJob, IDLE_EXPORT, project, type ExportJobState } from "./store";
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

/** A single-keepsake export reports the other two as "not selected"; that isn't a failure. */
export function wasAttempted(skip: { reason: string }): boolean {
  return !skip.reason.startsWith("not selected");
}

// A single-keepsake export must not forget the other two, but a keepsake that
// was attempted and skipped no longer exists as far as the trip is concerned.
function mergeSummary(prev: ExportSummary | null, next: ExportSummary): ExportSummary {
  const lost = (keepsake: string) => next.skipped.some((s) => s.keepsake === keepsake && wasAttempted(s));
  return {
    ...next,
    video: next.video ?? (lost("video") ? undefined : prev?.video),
    reels: next.reels.length > 0 || lost("reels") ? next.reels : (prev?.reels ?? []),
    bookPages: next.bookPages || (lost("book") ? 0 : prev?.bookPages || 0)
  };
}

function saveExport(projectId: string, summary: ExportSummary): void {
  try {
    localStorage.setItem(key(projectId), JSON.stringify(summary));
  } catch {
    /* private mode: the record lasts this session only */
  }
}

// One entry per trip, so a render outlives switching trips. `exportJob` only
// ever mirrors the open trip's entry.
const jobs = new Map<string, ExportJobState>();

/** This trip's export state: its latest run this session, else its saved keepsakes. */
export function exportStateFor(projectId: string): ExportJobState {
  return jobs.get(projectId) ?? { ...IDLE_EXPORT, summary: savedExport(projectId) };
}

function record(projectId: string, state: ExportJobState): void {
  jobs.set(projectId, state);
  if (get(project)?.projectId === projectId) exportJob.set(state);
}

/** Run one export per trip; state lands in `exportJob` whichever screen is showing. */
export async function runExport(cmd: ExportCmd, label: string, projectId: string): Promise<void> {
  if (jobs.get(projectId)?.label) return;
  record(projectId, { ...IDLE_EXPORT, cmd, label });
  try {
    const summary = mergeSummary(savedExport(projectId), await exportKeepsake(cmd, projectId));
    saveExport(projectId, summary);
    record(projectId, { ...IDLE_EXPORT, cmd, summary });
  } catch (e) {
    const message = String(e);
    // The backend reports a user's cancel as this exact error.
    const cancelled = message.includes("export cancelled");
    record(projectId, {
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
  const j = jobs.get(e.projectId);
  if (!j?.label) return;
  // The backend emits `running` before rendering, so Cancel has a target early.
  // After that, only this run's job may report; a previous run's late event may not.
  if (j.jobId ? e.id !== j.jobId : e.state !== "running") return;
  record(e.projectId, { ...j, jobId: e.id, progress: e.progress, step: e.currentStep ?? "" });
}
