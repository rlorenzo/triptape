<script lang="ts">
  import { project, assets, screen, consentGiven, giveConsent } from "./lib/store";
  import { scanFolder, getAssets, isTauri, type AssetView } from "./lib/tauri";

  let folder = "";
  let busy = false;
  let error = "";
  let showConsent = false;

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

  /** Fetch every page; a short page marks the end. */
  async function loadAllAssets(projectId: string) {
    const PER_PAGE = 500;
    const all: AssetView[] = [];
    for (let page = 0; ; page++) {
      const batch = await getAssets(projectId, page, PER_PAGE);
      all.push(...batch);
      if (batch.length < PER_PAGE) return all;
    }
  }

  async function runScan() {
    busy = true;
    try {
      const summary = await scanFolder(folder || "Demo Trip");
      project.set(summary);
      assets.set(await loadAllAssets(summary.projectId));
      screen.set("library");
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function acceptConsent() {
    giveConsent();
    showConsent = false;
    runScan();
  }
</script>

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
