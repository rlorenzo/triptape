<script lang="ts">
  import { project, assets, screen, consentGiven, giveConsent, simulateDemoIngestBackground } from "./lib/store";
  import { scanFolder, loadAllAssets, isTauri, onProgress } from "./lib/tauri";
  import { friendlyStep } from "./lib/trip";
  import ErrorNote from "./lib/ErrorNote.svelte";
  import ProgressBar from "./lib/ProgressBar.svelte";
  import { onMount, onDestroy, tick } from "svelte";

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
  // Sub-percent movement matters during hashing (1 file of 2119 is 0.02%):
  // one decimal under 10%, integer at/above.
  $: percentLabel = progress * 100 < 10 ? (progress * 100).toFixed(1) : String(Math.round(progress * 100));

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

  // Tauri delivers dropped folders as paths; the browser preview has no drop.
  let dropping = false;
  onMount(() => {
    if (!isTauri()) return;
    let stop: (() => void) | undefined;
    let gone = false;
    (async () => {
      const { getCurrentWebview } = await import("@tauri-apps/api/webview");
      const off = await getCurrentWebview().onDragDropEvent((e) => {
        if (e.payload.type === "enter" || e.payload.type === "over") dropping = true;
        else if (e.payload.type === "leave") dropping = false;
        else {
          dropping = false;
          const path = e.payload.paths[0];
          if (path && !busy) {
            folder = path;
            startWithConsent();
          }
        }
      });
      if (gone) off();
      else stop = off;
    })().catch(() => {
      // Choosing a folder still works if drop can't be set up.
    });
    return () => {
      gone = true;
      stop?.();
    };
  });

  async function startWithConsent() {
    error = "";
    try {
      if (!$consentGiven) {
        showConsent = true;
        await tick();
        consentHeading?.focus();
        return;
      }
      await runScan();
    } catch (e) {
      error = String(e);
    }
  }

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
      await startWithConsent();
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

  let consentHeading: HTMLElement;
  let addButton: HTMLButtonElement;

  async function declineConsent() {
    showConsent = false;
    await tick();
    addButton?.focus();
  }

  function acceptConsent() {
    giveConsent();
    showConsent = false;
    runScan();
  }
</script>

<style>
  .import {
    display: flex;
    flex-direction: column;
    justify-content: center;
    min-height: 100%;
  }

  .headline {
    font-size: clamp(2.4rem, 5vw, 4.25rem);
  }

  .headline span {
    display: block;
  }

  .lede {
    margin: 16px 0 28px;
    font-size: 18px;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: center;
  }

  .demo-input {
    max-width: 280px;
  }

  .notice,
  .error {
    margin-bottom: 16px;
  }

  .error {
    margin-top: 16px;
    margin-bottom: 0;
  }

  .panel {
    max-width: 560px;
    margin-top: 24px;
    padding: 18px;
    border: 1px solid var(--line);
  }

  .panel h2 {
    margin-bottom: 6px;
    font-size: 17px;
    font-weight: 700;
  }

  .panel h2:focus {
    outline: none;
  }

  .panel p {
    margin-bottom: 14px;
  }

  .scan-step {
    display: flex;
    gap: 6px;
    margin: 6px 0 0;
    color: var(--ink-2);
  }

  .import-error {
    max-width: 560px;
    margin-top: 16px;
  }

  .dropping {
    box-shadow: inset 0 0 0 6px var(--yellow);
    background: var(--yellow-soft);
  }

  .again {
    max-width: 60ch;
    margin-top: 14px;
    color: var(--ink-2);
  }
</style>

<div class="screen import" class:dropping>
  <h1 class="headline" tabindex="-1"><span>Drop in the trip.</span> <span>Get the story back.</span></h1>
  <p class="lede">Drag in a folder of vacation photos and videos, or choose one. TripTape sorts it into days and places, then makes a film, reels and a photo book. Everything runs on this computer.</p>

  {#if !isTauri()}
    <div class="notice">Browser preview with a demo trip. The real pipeline runs in the desktop app.</div>
  {/if}

  <div class="actions">
    {#if !isTauri()}
      <input class="input demo-input" placeholder="Demo folder name" aria-label="Demo folder name" bind:value={folder} />
    {/if}
    <button bind:this={addButton} class="btn btn-primary" disabled={busy} on:click={pickAndScan}>
      {busy ? "Scanning…" : "Choose trip folder"}
    </button>
    {#if $project && !busy}
      <button class="btn" on:click={() => screen.set("library")}>Back to this trip</button>
    {/if}
  </div>
  {#if $project && !busy}
    <p class="again">Choosing a folder again starts a new trip, without the edits you made to this one.</p>
  {/if}
  {#if error}
    {#if error.startsWith("Browser preview")}
      <p class="error" role="alert">{error}</p>
    {:else}
      <div class="import-error"><ErrorNote message="Couldn't scan that folder. Check that it's a folder and that it's still there, then try again." detail={error} /></div>
    {/if}
  {/if}

  {#if busy}
    <div class="panel">
      <h2>Scanning {folder || "Demo Trip"}</h2>
      <ProgressBar value={determinate ? progress : null} label="Scan progress" />
      <p class="scan-step" aria-live="polite">
        {#if determinate}<span>{percentLabel}%</span>{/if}
        <span>{friendlyStep(step) || "Starting…"}</span>
      </p>
    </div>
  {/if}

  {#if showConsent}
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="panel"
      role="group"
      aria-labelledby="consent-title"
      on:keydown={(e) => e.key === "Escape" && declineConsent()}
    >
      <h2 id="consent-title" tabindex="-1" bind:this={consentHeading}>Before we start</h2>
      <p>TripTape sorts your photos by date and place on this computer. Nothing is uploaded.</p>
      <div class="actions">
        <button class="btn btn-primary" on:click={acceptConsent}>Got it</button>
        <button class="btn" on:click={declineConsent}>Not now</button>
      </div>
    </div>
  {/if}
</div>
