<script lang="ts">
  import { screen, project, assets, ingestJob } from "./lib/store";
  import { onProgress, restoreLatestProject, loadAllAssets, isTauri } from "./lib/tauri";
  import Import from "./Import.svelte";
  import Library from "./Library.svelte";
  import Story from "./Story.svelte";
  import Export from "./Export.svelte";
  import Settings from "./Settings.svelte";
  import { onMount, onDestroy } from "svelte";
  import { get } from "svelte/store";

  const tabs = ["import", "library", "story", "export", "settings"] as const;

  // Global ingest indicator: App is always mounted, so one subscription
  // covers Library/Story/Export too. A new scan (running) overwrites any
  // earlier state; failures stay visible until that next scan.
  // On mount (Tauri only) the last trip reopens at once: project + assets +
  // Library, with any unfinished hashing resumed by the backend as a fresh
  // ingest job the indicator above picks up automatically.
  onMount(() => {
    const unsub = onProgress((e) => {
      if (e.kind !== "ingest") return;
      ingestJob.set({
        id: e.id,
        progress: e.progress,
        step: e.currentStep ?? "",
        state: e.state,
        error: e.error
      });
      // Background ingest just persisted scores and rebuilt the plan: reload
      // the current project's assets once so Best and Story reflect them
      // without a rescan. Skipped with no project (nothing to reload), in
      // browser demo mode (no real events), and never clobbers a project the
      // user has since switched to.
      if (e.state === "done" && isTauri()) {
        const current = get(project);
        if (current) {
          const pid = current.projectId;
          loadAllAssets(pid)
            .then((a) => {
              if (get(project)?.projectId === pid) assets.set(a);
            })
            .catch(() => {});
        }
      }
    });
    (async () => {
      if (!isTauri()) return;
      try {
        if (get(project) !== null) return;
        const summary = await restoreLatestProject();
        if (!summary) return; // empty store: start on Import as today
        // A scan raced the restore: the user's project wins, never clobbered.
        if (get(project) !== null) return;
        project.set(summary);
        assets.set(await loadAllAssets(summary.projectId));
        screen.set("library");
      } catch {
        // Restore is best-effort; a failure starts empty as today.
      }
    })();
    return unsub;
  });

  // The completion state clears itself after a few seconds, in Tauri mode
  // and in the browser-preview simulation alike.
  let clearTimer: ReturnType<typeof setTimeout> | null = null;
  $: if ($ingestJob?.state === "done" && clearTimer === null) {
    clearTimer = setTimeout(() => {
      ingestJob.set(null);
      clearTimer = null;
    }, 3000);
  }
  $: if ($ingestJob && $ingestJob.state !== "done" && clearTimer !== null) {
    clearTimeout(clearTimer);
    clearTimer = null;
  }
  onDestroy(() => {
    if (clearTimer !== null) clearTimeout(clearTimer);
  });

  $: ingestRunning = $ingestJob?.state === "running";
  $: ingestDone = $ingestJob?.state === "done";
  $: ingestFailed = $ingestJob?.state === "failed";
  $: ingestPercent = Math.round(($ingestJob?.progress ?? 0) * 100);
</script>

<nav class="flex gap-1 p-2 border-b bg-white" aria-label="Main">
  {#each tabs as t}
    <button
      class="px-4 py-2 rounded capitalize {$screen === t ? 'bg-gray-900 text-white' : 'hover:bg-gray-100'}"
      aria-current={$screen === t ? "page" : undefined}
      on:click={() => screen.set(t)}>{t}</button>
  {/each}
  {#if ingestRunning || ingestDone || ingestFailed}
    <div class="ml-auto flex items-center gap-2 text-xs text-gray-600 self-center" role="status" aria-live="polite">
      {#if ingestRunning}
        <div
          class="h-1.5 w-24 rounded bg-gray-200 overflow-hidden"
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={ingestPercent}
          aria-label="Background ingest progress"
        >
          <div class="h-full bg-blue-600" style="width: {ingestPercent}%"></div>
        </div>
        <span>{ingestPercent}%</span>
        <span class="max-w-56 truncate">{$ingestJob?.step}</span>
      {:else if ingestDone}
        <span>Library up to date</span>
      {:else}
        <span class="text-red-600 max-w-56 truncate">Background update failed{$ingestJob?.error ? `: ${$ingestJob.error}` : ""}</span>
      {/if}
    </div>
  {/if}
  {#if $project}
    <span class="{ingestRunning || ingestDone || ingestFailed ? '' : 'ml-auto '}text-sm text-gray-500 self-center">{$project.projectName}</span>
  {/if}
</nav>

<main>
  {#if $screen === "import"}<Import />
  {:else if $screen === "library"}<Library />
  {:else if $screen === "story"}<Story />
  {:else if $screen === "export"}<Export />
  {:else}<Settings />
  {/if}
</main>
