<script lang="ts">
  import { project, assets, screen, consentGiven, giveConsent, simulateDemoIngestBackground } from "./lib/store";
  import { scanFolder, loadAllAssets, isTauri, onProgress } from "./lib/tauri";
  import { onMount, onDestroy } from "svelte";

  let folder = "";
  let busy = false;
  let error = "";
  let showConsent = false;
  let progress = 0;
  let step = "";
  let seenAny = false;
  // Determinate once an event (or the demo simulation) reports progress > 0;
  // the walk phase stays at 0 and renders the indeterminate bar instead.
  $: determinate = seenAny && progress > 0;
  $: percent = Math.round(progress * 100);
  // Sub-percent movement matters during hashing (1 file of 2119 is 0.02%):
  // one decimal under 10%, integer at/above. `percent` stays the integer
  // for aria-valuenow and the bar reads the raw value.
  $: percentLabel = progress * 100 < 10 ? (progress * 100).toFixed(1) : String(Math.round(progress * 100));
  $: barWidth = (progress * 100).toFixed(2);

  // Stray export jobs must not drive the scan bar: only `ingest` events
  // arriving while a scan is in flight count. Svelte runs the returned
  // unsubscribe on destroy (same pattern as Export.svelte).
  onMount(() =>
    onProgress((e) => {
      if (e.kind !== "ingest" || !busy) return;
      progress = e.progress;
      step = e.currentStep ?? "";
      seenAny = true;
    })
  );

  // Browser preview: `onProgress` is a no-op there, so simulate the same
  // phase steps locally to keep the indicator verifiable without the shell.
  let demoTimer: ReturnType<typeof setInterval> | null = null;
  function startDemoProgress() {
    stopDemoProgress();
    progress = 0;
    step = "Finding files…";
    seenAny = true;
    const steps: [number, string][] = [
      [0.15, "Hashing files…"],
      [0.35, "Hashing files…"],
      [0.5, "Grouping photos…"],
      [0.7, "Building story plan…"],
      [0.9, "Saving…"]
    ];
    let i = 0;
    demoTimer = setInterval(() => {
      if (i < steps.length) {
        [progress, step] = steps[i];
        i++;
      }
    }, 120);
  }
  function stopDemoProgress() {
    if (demoTimer !== null) {
      clearInterval(demoTimer);
      demoTimer = null;
    }
  }
  onDestroy(stopDemoProgress);

  async function pickAndScan() {
    error = "";
    try {
      if (isTauri()) {
        const { open } = await import("@tauri-apps/plugin-dialog");
        const picked = await open({ directory: true, multiple: false, title: "Add a trip folder" });
        if (!picked || Array.isArray(picked)) return;
        folder = picked;
      } else if (!folder) {
        error = "Browser preview: type any folder name to load the demo trip.";
        return;
      }
      if (!$consentGiven) {
        showConsent = true;
        return;
      }
      await runScan();
    } catch (e) {
      error = String(e);
    }
  }

  async function runScan() {
    busy = true;
    progress = 0;
    step = isTauri() ? "Starting…" : "Finding files…";
    seenAny = !isTauri();
    error = "";
    if (!isTauri()) startDemoProgress();
    try {
      const summary = await scanFolder(folder || "Demo Trip");
      project.set(summary);
      assets.set(await loadAllAssets(summary.projectId));
      // Browser preview: the foreground simulation stops with this screen,
      // so the module-level store simulation carries the background phase
      // in the nav indicator after navigation.
      if (!isTauri()) simulateDemoIngestBackground();
      screen.set("library");
    } catch (e) {
      error = String(e);
    } finally {
      stopDemoProgress();
      busy = false;
    }
  }

  function acceptConsent() {
    giveConsent();
    showConsent = false;
    runScan();
  }
</script>

<style>
  @keyframes scan-slide {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(300%);
    }
  }
  .scan-indeterminate {
    animation: scan-slide 1.2s linear infinite;
  }
  @media (prefers-reduced-motion: reduce) {
    .scan-indeterminate {
      animation: none;
      width: 100%;
    }
  }
</style>

<div class="p-8 max-w-2xl mx-auto">
  <h1 class="text-3xl font-bold mb-2">Drop in the trip. Get the story back.</h1>
  <p class="text-gray-600 mb-6">Add a folder of vacation photos and videos. Everything runs on this computer.</p>

  {#if !isTauri()}
    <div class="mb-4 p-3 rounded bg-amber-100 text-amber-900 text-sm">
      Browser preview — demo state. Run inside the Tauri app for the real pipeline.
    </div>
  {/if}

  <div class="flex gap-2 mb-4">
    {#if !isTauri()}
      <input class="border rounded px-3 py-2 flex-1" placeholder="Demo folder name" bind:value={folder} />
    {/if}
    <button class="bg-blue-600 text-white rounded px-4 py-2 disabled:opacity-50" disabled={busy} on:click={pickAndScan}>
      {busy ? "Scanning…" : "Add folder"}
    </button>
  </div>
  {#if error}<p class="text-red-600 text-sm">{error}</p>{/if}

  {#if busy}
    <div class="mt-2 p-4 border rounded bg-white">
      <p class="text-sm font-medium mb-2">Scanning {folder || "Demo Trip"}…</p>
      <div
        class="h-2 rounded bg-gray-200 overflow-hidden"
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={determinate ? percent : undefined}
        aria-label="Scan progress"
      >
        {#if determinate}
          <div class="h-full bg-blue-600" style="width: {barWidth}%"></div>
        {:else}
          <div class="h-full w-1/3 bg-blue-600 scan-indeterminate"></div>
        {/if}
      </div>
      <p class="text-sm text-gray-600 mt-1 flex gap-x-1.5" aria-live="polite">
        {#if determinate}<span>{percentLabel}%</span>{/if}
        <span>{step || "Starting…"}</span>
      </p>
    </div>
  {/if}

  {#if showConsent}
    <div class="mt-6 p-4 border rounded bg-white shadow" role="dialog" aria-label="Consent">
      <h2 class="font-semibold mb-2">One-time note</h2>
      <p class="text-sm mb-4">TripTape finds faces and places on this computer. Nothing is uploaded unless you turn on Cloud Polish for a trip.</p>
      <div class="flex gap-2">
        <button class="bg-blue-600 text-white rounded px-4 py-2" on:click={acceptConsent}>Got it</button>
        <button class="border rounded px-4 py-2" on:click={() => (showConsent = false)}>Not now</button>
      </div>
    </div>
  {/if}
</div>
