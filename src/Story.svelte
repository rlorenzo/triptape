<script lang="ts">
  import { tick } from "svelte";
  import ErrorNote from "./lib/ErrorNote.svelte";
  import { assets, frameNumbers, project, screen, storyPicks } from "./lib/store";
  import Thumb from "./lib/Thumb.svelte";
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

  let dayIndex = 0;
  let selectedId: string | null = null;
  let renaming = false;
  let everSaved = false;

  $: chapters = story?.chapters ?? [];
  $: if (dayIndex >= chapters.length && chapters.length > 0) dayIndex = chapters.length - 1;
  $: day = chapters[dayIndex];
  $: picks = chapters.reduce((sum, c) => sum + c.entries.length, 0);
  $: if (story) storyPicks.set(picks);
  $: selected = day?.entries.find((e) => e.id === selectedId) ?? null;

  // Same number as the Library, so leaving a photo out never renumbers the rest.
  // Reads the store directly so the markup re-renders when numbers arrive.
  $: frameOf = (id: string): number | undefined => $frameNumbers.get(id);

  let errorMessage = "";
  let alternativesEl: HTMLElement;

  /** Editing removes the button that had focus, so hand focus to the nearest frame. */
  async function focusFrame(index: number) {
    await tick();
    const frames = document.querySelectorAll<HTMLElement>(".work .frame");
    (frames[Math.min(index, frames.length - 1)] ?? document.querySelector<HTMLElement>("main h1"))?.focus();
  }

  function selectedIndex(): number {
    return day?.entries.findIndex((e) => e.id === selectedId) ?? 0;
  }

  function goToDay(i: number) {
    dayIndex = i;
    selectedId = null;
    alternativesFor = null;
    renaming = false;
    document.querySelector("main")?.scrollTo(0, 0);
  }

  let renameButton: HTMLButtonElement;

  async function endRename() {
    renaming = false;
    await tick();
    renameButton?.focus();
  }

  // Opening the panel narrows the grid, so keep the chosen photo on screen.
  async function select(id: string, target: HTMLElement) {
    selectedId = selectedId === id ? null : id;
    alternativesFor = null;
    await tick();
    target.scrollIntoView({ block: "nearest" });
  }

  function dayName(c: { dayIndex?: number; title: string }): string {
    return c.dayIndex ? `Day ${c.dayIndex}` : c.title;
  }

  function kindName(e: { kind: string; isLivePhoto: boolean }): string {
    if (e.isLivePhoto) return "Live Photo";
    return e.kind === "video" ? "Video" : "Photo";
  }

  function usedIn(e: StoryEntryView): string {
    const uses = ["book"];
    if (e.inHero) uses.unshift("film");
    if (e.inReels) uses.splice(uses.length - 1, 0, "reels");
    if (uses.length === 1) return "In the book";
    return `In the ${uses.slice(0, -1).join(", ")} and ${uses[uses.length - 1]}`;
  }

  function frameAlt(e: StoryEntryView, frame: number | undefined): string {
    return e.caption || `${kindName(e)}${frame ? ` ${frame}` : ""}${fmtWhen(e) ? `, ${fmtWhen(e)}` : ""}`;
  }

  function switchProject(id: string | undefined) {
    generation += 1;
    story = null;
    dayIndex = 0;
    selectedId = null;
    renaming = false;
    everSaved = false;
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
        if (gen === generation) {
          error = String(e);
          errorMessage = "Couldn't open your story. Go to the Library and rescan the folder, then come back.";
        }
      });
  }

  // Queued, not dropped: a caption committed on blur can land mid-save.
  function edit(op: (id: string) => Promise<StoryView>, after?: () => void) {
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
        everSaved = true;
        after?.();
      })
      .catch((e) => {
        // Shown even after a project change: the user's edit was not saved.
        error = String(e);
        errorMessage = "That change didn't save. Try it again.";
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
      if (current()) {
        alternatives = found.filter((alt) => alt.id !== assetId);
        await tick();
        alternativesEl?.scrollIntoView({ block: "nearest" });
      }
    } catch (e) {
      if (current()) {
        error = String(e);
        errorMessage = "Couldn't load other photos from this day. Try again.";
      }
    }
  }
</script>

<div class="screen">
  <div class="screen-head">
    <div>
      <h1 class="headline" tabindex="-1">Your story</h1>
      {#if story && picks > 0}
        <p class="lede">TripTape picked {picks.toLocaleString()} from {$assets.length.toLocaleString()} files, across {chapters.length} {chapters.length === 1 ? "day" : "days"}. Change anything, or take it as it is.</p>
      {/if}
    </div>
    {#if story && picks > 0}
      <button class="btn btn-primary" on:click={() => screen.set("export")}>Looks good, go to Export</button>
    {/if}
  </div>

  <p class="save-state" role="status" aria-live="polite">
    {#if busy}Saving…{:else if everSaved}Saved. Your changes apply to the next film, reels and book.{:else if story && picks > 0}Choose a photo to edit its caption, swap it, or leave it out. Changes save on their own.{/if}
  </p>

  {#if error}<ErrorNote message={errorMessage} detail={error} />{/if}

  {#if !pid}
    <div class="empty">
      <h2>No story yet</h2>
      <p>Choose a trip folder and TripTape makes the first cut for you.</p>
      <button class="btn btn-primary" on:click={() => screen.set("import")}>Choose trip folder</button>
    </div>
  {:else if !story}
    {#if !error}<p class="loading">Loading your story…</p>{/if}
  {:else if chapters.length === 0}
    <div class="empty">
      <h2>No picks yet</h2>
      <p>TripTape didn't find enough usable photos for a story. Add more files to the folder, then rescan it from the Library.</p>
      <button class="btn" on:click={() => screen.set("library")}>Go to Library</button>
    </div>
  {:else}
    <nav class="days" aria-label="Days">
      {#each chapters as c, i (c.key)}
        <button class="day-tab" class:on={i === dayIndex} aria-current={i === dayIndex ? "true" : undefined} on:click={() => goToDay(i)}>
          <b>{dayName(c)}</b>
          <span>{c.place ?? c.dateLabel ?? ""}</span>
        </button>
      {/each}
    </nav>

    {#if day}
      <div class="day-band band">
        <h2>{dayName(day)}{day.dateLabel && day.dayIndex ? ` · ${day.dateLabel}` : ""}</h2>
        {#if !renaming}
          <span class="sub">{day.place ?? "No place name"}</span>
          <button class="link" bind:this={renameButton} on:click={() => (renaming = true)}>{day.place ? "Rename" : "Add a place name"}</button>
        {:else}
          <form
            class="rename"
            on:submit|preventDefault={(ev) => {
              const place = String(new FormData(ev.currentTarget).get("place") ?? "") || null;
              const key = day.key;
              endRename();
              edit((id) => setChapterPlace(id, key, place));
            }}
          >
            <label class="sr-only" for={`place-${day.key}`}>Place name for {dayName(day)}</label>
            <!-- svelte-ignore a11y_autofocus -->
            <input
              id={`place-${day.key}`}
              name="place"
              class="input"
              autofocus
              placeholder="e.g. Grandma's house"
              value={day.place ?? ""}
              on:keydown={(e) => e.key === "Escape" && endRename()}
            />
            <button type="submit" class="btn" disabled={busy}>Save</button>
            <button type="button" class="btn" on:click={endRename}>Cancel</button>
          </form>
        {/if}
      </div>

      <div class="work" class:has-selection={selected !== null}>
        <div class="index-grid">
          {#each day.entries as e (e.id)}
            <button class="frame" class:on={selectedId === e.id} aria-pressed={selectedId === e.id} on:click={(ev) => select(e.id, ev.currentTarget)}>
              <Thumb projectId={pid} assetId={e.id} kind={kindName(e)} alt={frameAlt(e, frameOf(e.id))} fit="contain" />
              <span class="frame-line">
                <span class="frame-no">{frameOf(e.id) ?? ""}</span>
                <span class="frame-caption" class:none={!e.caption}>{e.caption || fmtWhen(e) || kindName(e)}</span>
              </span>
            </button>
          {/each}
        </div>

        {#if selected}
        <aside class="panel" aria-label="Selected photo">
            {#key selected.id}
              <Thumb projectId={pid} assetId={selected.id} kind={kindName(selected)} alt={frameAlt(selected, frameOf(selected.id))} fit="full" />
              <p class="panel-title"><span class="frame-no">{frameOf(selected.id) ?? ""}</span> {kindName(selected)}{#if selected.durationSecs}, {selected.durationSecs.toFixed(0)}s{/if}</p>
              <p class="panel-meta">{fmtWhen(selected)}{fmtWhen(selected) ? " · " : ""}{usedIn(selected)}</p>

              <label class="field-label" for="caption">Caption</label>
              <input
                id="caption"
                class="input"
                placeholder="Add a caption"
                value={selected.caption ?? ""}
                on:change={(ev) => {
                  const caption = ev.currentTarget.value || null;
                  const assetId = selected.id;
                  edit((id) => setAssetCaption(id, assetId, caption));
                }}
              />

              <div class="panel-actions">
                {#if selected.inHero || selected.inReels}
                  <button class="btn" disabled={busy} aria-expanded={alternativesFor === selected.id} on:click={() => (alternativesFor === selected.id ? (alternativesFor = null) : showAlternatives(selected.id))}>Swap photo</button>
                {/if}
                <button class="btn btn-danger" disabled={busy} on:click={() => { const assetId = selected.id; const index = selectedIndex(); edit((id) => setAssetExcluded(id, assetId, true), () => focusFrame(index)); }}>Leave out</button>
              </div>

              {#if alternativesFor === selected.id}
                <div class="alts" bind:this={alternativesEl}>
                  <p>Other photos from this day. The swap changes the {[selected.inHero && "film", selected.inReels && "reels"].filter(Boolean).join(" and ")}, and the book keeps both.</p>
                  {#each alternatives as alt (alt.id)}
                    <div class="alt">
                      <Thumb projectId={pid} assetId={alt.id} kind={kindName(alt)} alt={alt.caption || `Other ${kindName(alt).toLowerCase()}${fmtWhen(alt) ? `, ${fmtWhen(alt)}` : ""}`} small />
                      <span><span class="frame-no">{frameOf(alt.id) ?? ""}</span> {fmtWhen(alt) || kindName(alt)}{#if alreadyPicked(selected, alt)}<br /><em>Already picked</em>{/if}</span>
                      <button class="btn" disabled={busy || alreadyPicked(selected, alt)} on:click={() => { const from = selected.id; const index = selectedIndex(); edit((id) => swapAsset(id, from, alt.id), () => focusFrame(index)); }}>Use this</button>
                    </div>
                  {:else}
                    <p class="none">No other photos from this day.</p>
                  {/each}
                </div>
              {/if}
            {/key}
        </aside>
        {/if}
      </div>

      <div class="pager">
        <button class="btn" disabled={dayIndex === 0} on:click={() => goToDay(dayIndex - 1)}>Previous day</button>
        <span>{dayIndex + 1} of {chapters.length}</span>
        {#if dayIndex < chapters.length - 1}
          <button class="btn" on:click={() => goToDay(dayIndex + 1)}>Next day</button>
        {:else}
          <button class="btn btn-primary" on:click={() => screen.set("export")}>Looks good, go to Export</button>
        {/if}
      </div>
    {/if}

    {#if story.excluded.length > 0}
      <section class="left-out">
        <h2>Left out ({story.excluded.length})</h2>
        <div class="index-grid">
          {#each story.excluded as e (e.id)}
            <div>
              <Thumb projectId={pid} assetId={e.id} kind={kindName(e)} alt={e.caption || `Left out ${kindName(e).toLowerCase()}${fmtWhen(e) ? `, ${fmtWhen(e)}` : ""}`} fit="contain" />
              <p class="left-no"><span class="frame-no">{frameOf(e.id) ?? ""}</span></p>
              <button class="btn restore" disabled={busy} on:click={() => edit((id) => setAssetExcluded(id, e.id, false), () => focusFrame(0))}>Put back</button>
            </div>
          {/each}
        </div>
      </section>
    {/if}
  {/if}
</div>

<style>
  .screen-head {
    margin-bottom: 8px;
  }

  .save-state {
    min-height: 22px;
    margin-bottom: 10px;
    color: var(--ink-2);
    font-size: 13px;
  }

  .loading {
    padding: 32px 0;
    color: var(--ink-2);
  }

  .days {
    display: flex;
    gap: 2px;
    overflow-x: auto;
  }

  .day-tab {
    flex: none;
    display: flex;
    flex-direction: column;
    max-width: 180px;
    min-height: 48px;
    padding: 6px 14px;
    border-radius: var(--radius) var(--radius) 0 0;
    color: var(--ink);
    text-align: left;
    cursor: pointer;
  }

  .day-tab span {
    overflow: hidden;
    color: var(--ink-2);
    font-size: 13px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .day-tab:hover:not(.on) {
    background: var(--wash);
  }

  .day-tab.on {
    background: var(--yellow);
  }

  .day-tab.on span {
    color: #3f4556;
  }

  .day-tab:focus-visible {
    outline-offset: -2px;
  }

  .band {
    align-items: center;
    min-height: 44px;
  }

  .link {
    color: var(--ink);
    font-weight: 500;
    text-decoration: underline;
    text-underline-offset: 3px;
    cursor: pointer;
  }

  .rename {
    display: flex;
    flex: 1 1 320px;
    gap: 8px;
  }

  .work {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 20px;
    align-items: start;
    margin-top: 16px;
  }

  .work.has-selection {
    grid-template-columns: minmax(0, 1fr) 280px;
  }

  /* Large while reliving, within what a 320px thumbnail can hold. */
  .work .index-grid {
    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
  }

  .work.has-selection .index-grid {
    grid-template-columns: repeat(auto-fill, minmax(148px, 1fr));
  }

  .left-out .index-grid {
    grid-template-columns: repeat(auto-fill, minmax(148px, 1fr));
  }

  .left-no {
    padding-top: 4px;
  }

  .frame {
    display: block;
    width: 100%;
    min-width: 0;
    /* Clears the sticky day band when scrolled into view. */
    scroll-margin-top: 60px;
    text-align: left;
    cursor: pointer;
  }

  .frame:hover :global(img) {
    outline: 2px solid var(--ink-3);
    outline-offset: -2px;
  }

  .frame.on :global(.thumb) {
    outline: 3px solid var(--blue);
    outline-offset: 2px;
  }

  .frame-line {
    display: flex;
    gap: 6px;
    padding-top: 5px;
    font-size: 13px;
    line-height: 1.3;
  }

  .frame-caption {
    display: -webkit-box;
    overflow: hidden;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
  }

  .frame-caption.none {
    color: var(--ink-2);
  }

  .panel {
    position: sticky;
    top: 60px;
    max-height: calc(100vh - 76px);
    overflow-y: auto;
    padding: 14px;
    background: var(--wash);
  }

  .panel p {
    color: var(--ink-2);
  }

  .panel-title {
    margin-top: 10px;
    color: var(--ink) !important;
    font-weight: 600;
  }

  .panel-meta {
    margin-bottom: 12px;
    font-size: 13px;
  }

  .panel-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 12px;
  }

  .alts {
    margin-top: 14px;
    padding-top: 12px;
    border-top: 1px solid var(--line);
    font-size: 13px;
  }

  .alt {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 10px;
  }

  .alt span {
    flex: 1;
    min-width: 0;
  }

  .alt em {
    color: var(--ink-2);
    font-style: normal;
  }

  .pager {
    display: flex;
    gap: 12px;
    align-items: center;
    margin-top: 24px;
  }

  .pager span {
    color: var(--ink-2);
  }

  .left-out {
    margin-top: 36px;
    padding-top: 16px;
    border-top: 1px solid var(--line);
  }

  .left-out h2 {
    margin-bottom: 12px;
    font-size: 16px;
    font-weight: 700;
  }

  .restore {
    width: 100%;
    margin-top: 6px;
  }
</style>
