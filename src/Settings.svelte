<script lang="ts">
  import { onMount, tick } from "svelte";
  import ErrorNote from "./lib/ErrorNote.svelte";
  import { project } from "./lib/store";
  import {
    clearCache,
    getCacheInfo,
    getDiagnosticReport,
    getLicenses,
    getPrivacySettings,
    revealExport,
    saveDiagnosticReport,
    setHideHome,
    setHome,
    setSegmentCacheCap,
    tripEndAsHome,
    type CacheInfo,
    type LicenseInfo,
    type PrivacySettings
  } from "./lib/tauri";

  const GB = 1024 ** 3;

  function size(bytes: number): string {
    if (bytes < 1024 ** 2) return `${Math.round(bytes / 1024)} KB`;
    if (bytes < GB) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
    return `${(bytes / GB).toFixed(1)} GB`;
  }

  /** "Couldn't …" plus the raw reason for the Technical details fold. */
  type Failure = { message: string; detail: string };
  const failed = (message: string, e: unknown): Failure => ({ message, detail: String(e) });

  // Home on maps -------------------------------------------------------------

  let privacy: PrivacySettings | null = null;
  let latInput = "";
  let lonInput = "";
  let homeError: Failure | null = null;
  let homeSaved = "";

  function showHome(p: PrivacySettings) {
    privacy = p;
    latInput = p.home ? String(p.home.lat) : "";
    lonInput = p.home ? String(p.home.lon) : "";
  }

  function parsed(text: string, limit: number): number | null {
    const n = Number(text.trim());
    return text.trim() !== "" && Number.isFinite(n) && Math.abs(n) <= limit ? n : null;
  }

  $: lat = parsed(latInput, 90);
  $: lon = parsed(lonInput, 180);
  $: latBad = latInput.trim() !== "" && lat === null;
  $: lonBad = lonInput.trim() !== "" && lon === null;
  $: homeChanged = privacy !== null && (lat !== (privacy.home?.lat ?? null) || lon !== (privacy.home?.lon ?? null));

  async function saveHome() {
    homeError = null;
    homeSaved = "";
    if (lat === null || lon === null) return;
    try {
      showHome(await setHome({ lat, lon }));
      homeSaved = "Home saved. Maps in new exports keep the route away from it.";
    } catch (e) {
      homeError = failed("Couldn't save home. Check both numbers and try again.", e);
    }
  }

  async function forgetHome() {
    homeError = null;
    try {
      showHome(await setHome(null));
      homeSaved = "Home forgotten.";
    } catch (e) {
      homeError = failed("Couldn't forget home. Try again.", e);
    }
  }

  async function fillFromTrip(which: "first" | "last") {
    homeError = null;
    homeSaved = "";
    if (!$project) return;
    try {
      const h = await tripEndAsHome($project.projectId, which);
      latInput = h.lat.toFixed(4);
      lonInput = h.lon.toFixed(4);
    } catch (e) {
      homeError = failed("Couldn't find where this trip starts or ends.", e);
    }
  }

  async function toggleHide(e: Event) {
    const hide = (e.currentTarget as HTMLInputElement).checked;
    try {
      showHome(await setHideHome(hide));
    } catch (err) {
      homeError = failed("Couldn't change that setting. Try again.", err);
    }
  }

  // Storage ------------------------------------------------------------------

  type CacheKind = "thumbnails" | "segments";
  let cache: CacheInfo | null = null;
  let cacheError: Failure | null = null;
  let confirming: CacheKind | null = null;
  let clearing: CacheKind | null = null;
  let cleared: Partial<Record<CacheKind, string>> = {};
  const CAPS = [5, 10, 20, 50, 100];
  let capChoice = "default";
  // A limit set in settings.json or by the CLI can be any size: list it too,
  // so the menu never shows blank.
  $: capOptions = [
    ...new Set([
      ...CAPS.filter((c) => c * GB !== cache?.defaultSegmentCapBytes),
      ...(capChoice === "default" ? [] : [Number(capChoice)]),
    ]),
  ].sort((a, b) => a - b);

  async function loadCache() {
    try {
      cache = await getCacheInfo();
      const capBytes = cache.segmentCapBytes;
      capChoice = capBytes === null || capBytes === cache.defaultSegmentCapBytes ? "default" : String(capBytes / GB);
    } catch (e) {
      cacheError = failed("Couldn't read the cache sizes.", e);
    }
  }

  async function changeCap() {
    cacheError = null;
    try {
      const cap = await setSegmentCacheCap(capChoice === "default" ? null : Number(capChoice) * GB);
      if (cache) cache = { ...cache, segmentCapBytes: cap };
    } catch (e) {
      cacheError = failed("Couldn't change the limit. Try again.", e);
    }
  }

  async function askClear(kind: CacheKind) {
    confirming = kind;
    cleared = { ...cleared, [kind]: "" };
    await tick();
    document.getElementById(`confirm-${kind}`)?.focus();
  }

  async function cancelClear(kind: CacheKind) {
    confirming = null;
    await tick();
    document.getElementById(`clear-${kind}`)?.focus();
  }

  async function doClear(kind: CacheKind) {
    confirming = null;
    clearing = kind;
    cacheError = null;
    try {
      const r = await clearCache(kind);
      const kept = r.kept > 0 ? ` ${r.kept} in use by an export were kept.` : "";
      cleared = { ...cleared, [kind]: `Cleared ${size(r.freedBytes)}.${kept}` };
      await loadCache();
    } catch (e) {
      cacheError = failed("Couldn't clear that cache. Try again.", e);
    } finally {
      clearing = null;
      await tick();
      document.getElementById(`clear-${kind}`)?.focus();
    }
  }

  // Help with a problem ------------------------------------------------------

  let report = "";
  let reportNote = "";
  let reportError: Failure | null = null;

  async function copyReport() {
    reportError = null;
    reportNote = "";
    try {
      report = await getDiagnosticReport();
    } catch (e) {
      reportError = failed("Couldn't make the report.", e);
      return;
    }
    try {
      await navigator.clipboard.writeText(report);
      reportNote = "Copied. Paste it into an email or an issue. Nothing was sent.";
      return;
    } catch {
      // No clipboard here: save it as a file and show it instead.
    }
    try {
      const path = await saveDiagnosticReport();
      reportNote = "The clipboard wasn't available, so the report was saved as a file. It's shown in its folder.";
      await revealExport(path);
    } catch (e) {
      reportError = failed("Couldn't copy or save the report. You can still select it below and copy it.", e);
    }
  }

  // Licences -----------------------------------------------------------------

  let licenses: LicenseInfo[] = [];
  let licenseError: Failure | null = null;

  // The three loads are independent: none waits for another.
  onMount(() => {
    getPrivacySettings().then(showHome, (e) => (homeError = failed("Couldn't read your settings.", e)));
    void loadCache();
    getLicenses().then(
      (l) => (licenses = l),
      (e) => (licenseError = failed("Couldn't read the licences.", e))
    );
  });
</script>

<div class="screen settings">
  <div class="screen-head">
    <div>
      <h1 class="headline" tabindex="-1">Settings</h1>
      <p class="lede">TripTape sorts and renders everything on this computer. Nothing is uploaded.</p>
    </div>
  </div>

  <section aria-labelledby="exports-title">
    <h2 id="exports-title">What your keepsakes carry</h2>
    <p>
      Films, reels, book pages and PDFs are saved without the location, camera details or capture times stored in your photos, and without the
      code that links a Live Photo to its video. Dates you see on titles and pages stay.
    </p>
  </section>

  <section aria-labelledby="home-title">
    <h2 id="home-title">Hide home on maps</h2>
    <p>
      Enter where home is and the trip's route stays at least 500 m away from it on the film's map and the book's map page. Home stays on this
      computer.
    </p>
    {#if privacy}
      <div class="home-fields">
        <div>
          <label class="field-label" for="home-lat">Latitude</label>
          <input
            id="home-lat"
            class="input"
            inputmode="decimal"
            autocomplete="off"
            placeholder="37.7749"
            bind:value={latInput}
            aria-invalid={latBad}
            aria-describedby="home-hint"
          />
        </div>
        <div>
          <label class="field-label" for="home-lon">Longitude</label>
          <input
            id="home-lon"
            class="input"
            inputmode="decimal"
            autocomplete="off"
            placeholder="-122.4194"
            bind:value={lonInput}
            aria-invalid={lonBad}
            aria-describedby="home-hint"
          />
        </div>
      </div>
      <p id="home-hint" class="hint" class:bad={latBad || lonBad}>
        {#if latBad || lonBad}
          Latitude is a number from -90 to 90, longitude from -180 to 180.
        {:else}
          Decimal degrees, as a map app shows them when you hold a finger on a place.
        {/if}
      </p>
      <div class="row">
        <button class="btn" disabled={lat === null || lon === null || !homeChanged} on:click={saveHome}>Save home</button>
        {#if $project}
          <button class="btn" on:click={() => fillFromTrip("first")}>Use where this trip starts</button>
          <button class="btn" on:click={() => fillFromTrip("last")}>Use where it ends</button>
        {/if}
        {#if privacy.home}
          <button class="btn btn-danger" on:click={forgetHome}>Forget home</button>
        {/if}
      </div>
      <label class="check">
        <input type="checkbox" checked={privacy.hideHome} disabled={!privacy.home} on:change={toggleHide} aria-describedby="hide-hint" />
        Hide home on maps
      </label>
      <p id="hide-hint" class="hint">{privacy.home ? "On for every export while it's ticked." : "Save a home first."}</p>
      {#if homeSaved}<p class="done" role="status">{homeSaved}</p>{/if}
    {/if}
    {#if homeError}<ErrorNote {...homeError} />{/if}
  </section>

  <section aria-labelledby="storage-title">
    <h2 id="storage-title">Storage</h2>
    <p>TripTape keeps copies it can make again, so the app and repeat exports stay quick. Clearing them never touches your photos or keepsakes.</p>
    {#if cache}
      {@const rows = [
        {
          kind: "thumbnails" as CacheKind,
          title: "Thumbnails",
          bytes: cache.thumbnailBytes,
          what: `${size(cache.thumbnailBytes)} in ${cache.thumbnailFiles.toLocaleString()} files`,
          empty: cache.thumbnailFiles === 0
        },
        {
          kind: "segments" as CacheKind,
          title: "Film pieces",
          bytes: cache.segmentBytes,
          what: `${size(cache.segmentBytes)} in ${cache.segmentEntries.toLocaleString()} pieces${cache.segmentInUse > 0 ? ", in use by an export" : ""}`,
          empty: cache.segmentEntries === 0
        }
      ]}
      <ul class="caches">
        {#each rows as row (row.kind)}
          <li>
            <div class="cache-text">
              <h3 id={`${row.kind}-title`}>{row.title}</h3>
              <p id={`${row.kind}-size`}>{row.what}</p>
              {#if cleared[row.kind]}<p class="done" role="status">{cleared[row.kind]}</p>{/if}
            </div>
            {#if confirming === row.kind}
              <div class="confirm" role="group" aria-labelledby={`${row.kind}-title`}>
                <span>Clear {size(row.bytes)}?</span>
                <button id={`confirm-${row.kind}`} class="btn btn-danger" on:click={() => doClear(row.kind)}>Clear</button>
                <button class="btn" on:click={() => cancelClear(row.kind)}>Keep</button>
              </div>
            {:else}
              <button
                id={`clear-${row.kind}`}
                class="btn"
                disabled={row.empty || clearing !== null}
                aria-describedby={`${row.kind}-title ${row.kind}-size`}
                on:click={() => askClear(row.kind)}
              >
                {clearing === row.kind ? "Clearing…" : "Clear…"}
              </button>
            {/if}
          </li>
        {/each}
      </ul>
      <div class="cap">
        <label class="field-label" for="segment-cap">Most space for film pieces</label>
        <select id="segment-cap" bind:value={capChoice} on:change={changeCap} aria-describedby="cap-hint">
          <option value="default">{size(cache.defaultSegmentCapBytes)} (standard)</option>
          {#each capOptions as gb}
            <option value={String(gb)}>{CAPS.includes(gb) ? `${gb} GB` : size(gb * GB)}</option>
          {/each}
        </select>
        <p id="cap-hint" class="hint">Older pieces go first once an export finishes. TripTape also leaves free space on the disk.</p>
      </div>
    {/if}
    {#if cacheError}<ErrorNote {...cacheError} />{/if}
  </section>

  <section aria-labelledby="help-title">
    <h2 id="help-title">Help with a problem</h2>
    <p>
      The report lists the app and system versions, the media tools, cache sizes and recent log lines. Folder and file names become short codes,
      and places, captions and names are left out. Read it before you share it.
    </p>
    <div class="row">
      <button class="btn" on:click={copyReport}>Copy diagnostic report</button>
    </div>
    {#if reportNote}<p class="done" role="status">{reportNote}</p>{/if}
    {#if reportError}<ErrorNote {...reportError} />{/if}
    {#if report}
      <details>
        <summary>Show the report</summary>
        <!-- svelte-ignore a11y_no_noninteractive_tabindex (a scrolling box must take focus to scroll by keyboard) -->
        <pre class="text" tabindex="0" aria-label="Diagnostic report">{report}</pre>
      </details>
    {/if}
  </section>

  <section aria-labelledby="licences-title">
    <h2 id="licences-title">Licences</h2>
    <p>
      TripTape is free and open source. It runs FFmpeg and Typst as separate programs, each under its own licence. The Archivo typeface is licensed
      under the SIL Open Font License.
    </p>
    {#if licenseError}<ErrorNote {...licenseError} />{/if}
    <ul class="licences">
      {#each licenses as l (l.id)}
        <li>
          <h3>{l.title}</h3>
          <p>{l.summary}</p>
          {#if l.links.length}
            <dl class="links">
              {#each l.links as link}
                <dt>{link.label}</dt>
                <dd><code>{link.url}</code></dd>
              {/each}
            </dl>
          {/if}
          <details>
            <summary>Read the full text</summary>
            <!-- svelte-ignore a11y_no_noninteractive_tabindex (a scrolling box must take focus to scroll by keyboard) -->
            <pre class="text" tabindex="0" aria-label={`${l.title} licence`}>{l.text}</pre>
          </details>
        </li>
      {/each}
    </ul>
  </section>
</div>

<style>
  .settings section {
    max-width: 72ch;
    margin-top: 28px;
  }

  h2 {
    margin-bottom: 4px;
    font-size: 17px;
    font-weight: 700;
  }

  h3 {
    font-size: 15px;
    font-weight: 700;
  }

  p {
    color: var(--ink-2);
  }

  .home-fields {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 14rem));
    gap: 12px;
    margin-top: 12px;
  }

  .hint {
    margin-top: 4px;
    font-size: 13px;
  }

  .hint.bad {
    color: var(--red);
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 12px;
  }

  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 16px;
    font-weight: 500;
  }

  .done {
    margin-top: 8px;
    color: var(--ink);
  }

  .caches {
    margin-top: 12px;
    border-top: 1px solid var(--line);
  }

  .caches li {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 12px 0;
    border-bottom: 1px solid var(--line);
  }

  .confirm {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .cap {
    margin-top: 16px;
  }

  select {
    min-height: 36px;
    padding: 7px 10px;
    border: 1px solid #8d95a8;
    border-radius: var(--radius);
    background: var(--paper);
  }

  details {
    margin-top: 8px;
  }

  summary {
    cursor: pointer;
    font-weight: 500;
  }

  .text {
    max-height: 24rem;
    margin-top: 8px;
    padding: 12px;
    overflow: auto;
    background: var(--wash);
    font-size: 13px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .licences li {
    padding: 12px 0;
    border-bottom: 1px solid var(--line);
  }

  .links {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 2px 12px;
    margin-top: 6px;
    font-size: 13px;
  }

  .links dt {
    color: var(--ink-2);
  }

  .links code {
    user-select: all;
    overflow-wrap: anywhere;
  }
</style>
