<script lang="ts">
  import { project } from "./lib/store";
  import {
    getAlternatives,
    getStory,
    setAssetCaption,
    setAssetExcluded,
    setChapterPlace,
    swapAsset
  } from "./lib/tauri";
  import type { StoryEntryView, StoryView } from "./lib/tauri";

  let story: StoryView | null = null;
  let error = "";
  let pendingEdits = 0;
  let alternativesFor: string | null = null;
  let alternatives: StoryEntryView[] = [];
  let editQueue: Promise<void> = Promise.resolve();
  // Bumped on every project change; a response from an older one is stale.
  let generation = 0;

  $: pid = $project?.projectId;
  $: switchProject(pid);
  $: busy = pendingEdits > 0;

  function switchProject(id: string | undefined) {
    generation += 1;
    story = null;
    error = "";
    alternativesFor = null;
    alternatives = [];
    if (id) load(id, generation);
  }

  // Behind pending edits, so reopening a project can't read a pre-edit story.
  function load(id: string, gen: number) {
    editQueue = editQueue
      .then(() => getStory(id))
      .then((loaded) => {
        if (gen === generation) story = loaded;
      })
      .catch((e) => {
        if (gen === generation) error = String(e);
      });
  }

  // Queued, not dropped: a caption committed on blur can land mid-save.
  function edit(op: (id: string) => Promise<StoryView>) {
    const id = pid;
    if (!id) return;
    const gen = generation;
    pendingEdits += 1;
    error = "";
    editQueue = editQueue
      .then(() => op(id))
      .then((updated) => {
        if (gen !== generation) return;
        story = updated;
        alternativesFor = null;
      })
      .catch((e) => {
        // Shown even after a project change: the user's edit was not saved.
        error = String(e);
      })
      .finally(() => {
        pendingEdits -= 1;
      });
  }

  function fmtWhen(e: { takenAt?: number }): string {
    if (!e.takenAt) return "";
    return new Date(e.takenAt * 1000).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit"
    });
  }

  function meta(e: StoryEntryView): string {
    const bits = [e.kind];
    if (e.width && e.height) bits.push(`${e.width}×${e.height}`);
    if (e.durationSecs) bits.push(`${e.durationSecs.toFixed(1)}s`);
    return bits.join(" · ");
  }

  // Mirrors the backend's check_swap: replacing a pick with one already in the
  // same keepsake would shorten it.
  function alreadyPicked(e: StoryEntryView, alt: StoryEntryView): boolean {
    return (e.inHero && alt.inHero) || (e.inReels && alt.inReels);
  }

  async function showAlternatives(assetId: string) {
    if (!pid) return;
    const gen = generation;
    alternativesFor = assetId;
    alternatives = [];
    error = "";
    const current = () => gen === generation && alternativesFor === assetId;
    try {
      const found = await getAlternatives(pid, assetId);
      if (current()) alternatives = found;
    } catch (e) {
      if (current()) error = String(e);
    }
  }
</script>

<div class="p-6 max-w-5xl mx-auto">
  <h1 class="text-2xl font-bold mb-1">Story editor</h1>
  <p class="text-sm text-gray-600 mb-4">
    Fine-tune what the generator picked. Rename a place, fix a caption, remove a photo, or swap a
    pick for another photo from the same day. Every change saves automatically and applies to the
    next export (video, reels, and book). Importing the folder again starts a new project without
    these edits.
  </p>

  {#if !pid}
    <p class="text-gray-500">Import a folder first — the story appears here.</p>
  {:else if error}
    <p class="text-red-700 text-sm mb-4" role="alert">{error}</p>
  {/if}

  {#if story}
    <p class="text-sm text-gray-700 mb-4">
      {story.heroCount} moments in the trip video · {story.reelCount} reel picks ·
      {story.chapters.length} chapters
    </p>

    {#each story.chapters as ch (ch.key)}
      <section class="mb-6 border rounded p-4 bg-white">
        <div class="flex flex-wrap items-baseline gap-2 mb-1">
          <h2 class="font-semibold">
            {#if ch.dayIndex}Day {ch.dayIndex} · {/if}{ch.dateLabel ?? ch.title}
          </h2>
          {#if ch.place && ch.place !== ch.dateLabel}<span class="text-sm text-gray-500">{ch.place}</span>{/if}
        </div>
        <form
          class="flex gap-2 items-center mb-3 text-sm"
          on:submit|preventDefault={(ev) => {
            const place = String(new FormData(ev.currentTarget).get("place") ?? "") || null;
            edit((id) => setChapterPlace(id, ch.key, place));
          }}
        >
          <label class="text-gray-600" for={`place-${ch.key}`}>Location name</label>
          <input
            id={`place-${ch.key}`}
            name="place"
            class="border rounded px-2 py-1 flex-1"
            placeholder="e.g. Carnival Luminosa, Grandma's house…"
            value={ch.place ?? ""}
          />
          <button
            type="submit"
            class="px-3 py-1 rounded border bg-gray-900 text-white disabled:opacity-50"
            disabled={busy}
          >Save</button>
        </form>

        <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-2">
          {#each ch.entries as e (e.id)}
            {@const when = fmtWhen(e)}
            <div class="border rounded p-2 text-sm {e.inHero || e.inReels ? 'bg-white' : 'bg-gray-50'}">
              <div class="flex items-center justify-between gap-2">
                <span class="font-mono text-xs truncate" title={e.id}>{e.id}</span>
                <span class="flex gap-1">
                  {#if e.inHero}<span class="text-[10px] px-1 rounded bg-gray-900 text-white">video</span>{/if}
                  {#if e.inReels}<span class="text-[10px] px-1 rounded bg-gray-600 text-white">reel</span>{/if}
                </span>
              </div>
              <p class="text-xs text-gray-500">{meta(e)}{#if when} · {when}{/if}</p>
              <input
                class="border rounded px-2 py-1 w-full mt-1"
                placeholder="Caption…"
                value={e.caption ?? ""}
                on:change={(ev) => {
                  const caption = ev.currentTarget.value || null;
                  edit((id) => setAssetCaption(id, e.id, caption));
                }}
                aria-label="Caption for {e.id}"
              />
              <div class="flex gap-2 mt-2">
                {#if e.inHero || e.inReels}
                  <button
                    class="px-2 py-1 rounded border text-xs disabled:opacity-50"
                    disabled={busy}
                    on:click={() => showAlternatives(e.id)}>Replace…</button>
                {/if}
                <button
                  class="px-2 py-1 rounded border text-xs text-red-700 disabled:opacity-50"
                  disabled={busy}
                  on:click={() => edit((id) => setAssetExcluded(id, e.id, true))}>Remove</button>
              </div>
              {#if alternativesFor === e.id}
                <div class="mt-2 border-t pt-2">
                  <p class="text-xs text-gray-500 mb-1">
                    Same day — tap to use instead in the
                    {[e.inHero && "video", e.inReels && "reels"].filter(Boolean).join(" and ")}
                    (the book keeps both):
                  </p>
                  {#each alternatives as alt (alt.id)}
                    <div class="flex items-center justify-between gap-2 py-1">
                      <span class="text-xs">
                        <span class="font-mono">{alt.id}</span>
                        <span class="text-gray-500">{meta(alt)}</span>
                        {#if alt.inHero}<span class="text-[10px] px-1 rounded bg-gray-900 text-white">in video</span>{/if}
                        {#if alt.inReels}<span class="text-[10px] px-1 rounded bg-gray-600 text-white">in reels</span>{/if}
                      </span>
                      <button
                        class="px-2 py-0.5 rounded border text-xs disabled:opacity-50"
                        disabled={busy || alreadyPicked(e, alt)}
                        on:click={() => edit((id) => swapAsset(id, e.id, alt.id))}
                      >Use this</button>
                    </div>
                  {/each}
                  {#if alternatives.length === 0}<p class="text-xs text-gray-400">No alternatives.</p>{/if}
                </div>
              {/if}
            </div>
          {/each}
        </div>
      </section>
    {/each}

    {#if story.excluded.length > 0}
      <section class="mb-6 border rounded p-4 bg-gray-50">
        <h2 class="font-semibold mb-2">Removed from keepsakes</h2>
        <div class="flex flex-wrap gap-2">
          {#each story.excluded as e (e.id)}
            <span class="inline-flex items-center gap-2 border rounded px-2 py-1 text-xs bg-white">
              <span class="font-mono">{e.id}</span>
              <button
                class="underline disabled:opacity-50"
                disabled={busy}
                on:click={() => edit((id) => setAssetExcluded(id, e.id, false))}>Restore</button>
            </span>
          {/each}
        </div>
      </section>
    {/if}
  {:else if pid}
    <p class="text-gray-500">Loading story…</p>
  {/if}
</div>
