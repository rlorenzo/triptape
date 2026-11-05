<script lang="ts">
  import { onMount } from "svelte";
  import Thumb from "./Thumb.svelte";
  import type { AssetView } from "./tauri";

  export let day: {
    key: string;
    number?: number;
    date: string;
    places: string[];
    items: AssetView[];
  };
  export let projectId: string | undefined;
  export let frameNumbers: Map<string, number>;

  // A big trip has thousands of files: a day's tiles exist only once it's near
  // the viewport, and a placeholder holds its height until then.
  let host: HTMLElement;
  let near = false;

  onMount(() => {
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          near = true;
          observer.disconnect();
        }
      },
      { rootMargin: "600px" },
    );
    observer.observe(host);
    return () => observer.disconnect();
  });

  function kindLabel(a: AssetView): string {
    if (a.isLivePhoto || a.kind === "live-photo" || a.kind === "livephoto") return "Live Photo";
    return a.kind === "video" ? "Video" : "Photo";
  }

  function time(a: AssetView): string {
    if (!a.takenAt) return "";
    return new Date(a.takenAt * 1000).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  }

  function detail(a: AssetView): string {
    const bits: string[] = [];
    if (a.isLivePhoto || a.kind === "video") {
      bits.push(a.durationSecs ? `${kindLabel(a)} ${a.durationSecs.toFixed(0)}s` : kindLabel(a));
    }
    if (a.timeGuessed) bits.push("date approx.");
    return bits.join(" · ");
  }

  function altText(a: AssetView): string {
    const when = time(a);
    return [`${kindLabel(a)} ${frameNumbers.get(a.id)}`, day.date, when, a.place].filter(Boolean).join(", ");
  }

  $: estimatedRows = Math.ceil(day.items.length / 5);
</script>

<section class="day" id="day-{day.key}" bind:this={host}>
  <div class="day-band">
    <h2>{day.number ? `Day ${day.number}` : "No date found"}</h2>
    {#if day.date}<span class="date">{day.date}</span>{/if}
    <span class="sub">{day.places.join(" and ")}{day.places.length ? " · " : ""}{day.items.length.toLocaleString()} {day.items.length === 1 ? "file" : "files"}</span>
  </div>

  {#if near}
    <div class="index-grid frames">
      {#each day.items as a (a.id)}
        <figure>
          <Thumb {projectId} assetId={a.id} kind={kindLabel(a)} alt={altText(a)} />
          <figcaption>
            <span class="frame-no">{frameNumbers.get(a.id)}</span>
            <span class="time">{time(a)}</span>
            {#if detail(a)}<span class="detail">{detail(a)}</span>{/if}
          </figcaption>
        </figure>
      {/each}
    </div>
  {:else}
    <div class="frames" style="height: {estimatedRows * 130}px"></div>
  {/if}
</section>

<style>
  .day {
    margin-bottom: 28px;
    scroll-margin-top: 0;
  }

  .frames {
    margin-top: 12px;
  }

  figcaption {
    display: flex;
    flex-wrap: wrap;
    gap: 0 6px;
    padding-top: 4px;
    font-size: 13px;
    line-height: 1.3;
  }

  .date {
    font-weight: 600;
  }

  .time {
    color: var(--ink-2);
  }

  .detail {
    flex-basis: 100%;
    color: var(--ink-2);
  }
</style>
