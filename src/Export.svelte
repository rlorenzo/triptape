<script lang="ts">
  import { project } from "./lib/store";
  import { exportKeepsake, cancelJob, REELS_DISCLOSURE, onProgress, type ExportCmd } from "./lib/tauri";
  import { onMount } from "svelte";

  const KEEPSAKES: { cmd: Exclude<ExportCmd, "export_all">; label: string; blurb: string; button: string; note?: string }[] = [
    { cmd: "export_video", label: "Trip Video", blurb: "1080p hero film, ~3–5 min.", button: "Export film" },
    {
      cmd: "export_reels",
      label: "Reels Pack",
      blurb: "3–5 vertical clips, 1080×1920.",
      button: "Export reels",
      note: REELS_DISCLOSURE
    },
    { cmd: "export_book", label: "Photo Book", blurb: "Proof PDF + per-page JPEGs.", button: "Export book" }
  ];

  let busy: string | null = null;
  let currentJob = "";
  let progress = 0;
  let step = "";
  let result: string[] = [];
  let error = "";
  let outDir = "";

  // `onProgress` returns its unsubscribe, which Svelte runs on destroy.
  onMount(() =>
    onProgress((e) => {
      // Ingest background events must not drive the export bar: the hash
      // phase now runs for minutes after the scan command returns.
      if (e.kind === "ingest") return;
      // The backend emits the job (state `running`) before rendering starts,
      // so Cancel can target it while the export command is still pending.
      if (busy && e.state === "running") currentJob = e.id;
      progress = e.progress;
      step = e.currentStep ?? "";
    })
  );

  /** Run one export; resolves `false` when it could not start or failed. */
  async function run(cmd: ExportCmd, label: string): Promise<boolean> {
    if (!$project) {
      error = "Scan a folder first.";
      return false;
    }
    busy = label;
    currentJob = "";
    progress = 0;
    step = "";
    error = "";
    result = [];
    try {
      const r = await exportKeepsake(cmd, $project.projectId);
      outDir = r.outDir;
      const lines: string[] = [];
      if (r.video) lines.push(`Trip Video: ${r.video}`);
      for (const reel of r.reels) lines.push(`Reel: ${reel}`);
      if (r.bookPages) lines.push(`Photo Book: ${r.bookPages}-page proof + JPEGs`);
      for (const s of r.skipped) lines.push(`Skipped ${s.keepsake}: ${s.reason}`);
      result = lines.length > 0 ? lines : ["Done — nothing eligible (see degradation rules)."];
      return true;
    } catch (e) {
      error = String(e);
      return false;
    } finally {
      busy = null;
      currentJob = "";
    }
  }

  // One backend pass renders every eligible keepsake.
  const exportAll = () => run("export_all", "All keepsakes");
</script>

<div class="p-6 max-w-3xl mx-auto">
  <h1 class="text-2xl font-bold mb-4">Export</h1>

  <button class="bg-blue-600 text-white rounded px-5 py-2 mb-4 disabled:opacity-50" disabled={busy !== null} on:click={exportAll}>
    {busy ? `${busy}… ${Math.round(progress * 100)}% ${step}` : "Export all keepsakes"}
  </button>
  {#if busy}
    <button class="ml-2 border rounded px-3 py-2 text-sm" on:click={() => currentJob && cancelJob(currentJob)}>Cancel</button>
  {/if}
  {#if error}<p class="text-red-600 text-sm mb-2">{error}</p>{/if}
  {#if outDir}<p class="text-sm text-gray-600 mb-2">Destination: {outDir}</p>{/if}
  {#if result.length > 0}
    <ul class="text-sm mb-4 list-disc ml-5">
      {#each result as line}<li>{line}</li>{/each}
    </ul>
    <button class="border rounded px-3 py-1 text-sm" on:click={() => (result = [])}>Dismiss</button>
  {/if}

  <div class="grid sm:grid-cols-3 gap-3 mt-4">
    {#each KEEPSAKES as k}
      <div class="border rounded p-4 bg-white">
        <h2 class="font-semibold">{k.label}</h2>
        <p class="text-xs text-gray-500 mb-2">{k.blurb}</p>
        {#if k.note}<p class="text-xs text-gray-700 border-l-2 border-amber-400 pl-2 my-2">{k.note}</p>{/if}
        <button class="border rounded px-3 py-1 text-sm" disabled={busy !== null} on:click={() => run(k.cmd, k.label)}>{k.button}</button>
      </div>
    {/each}
  </div>
</div>
