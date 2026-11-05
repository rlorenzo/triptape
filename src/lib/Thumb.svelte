<script module lang="ts">
  // A photo develops once per session; revisits show it true at once.
  const developed = new Set<string>();
</script>

<script lang="ts">
  import { onMount } from "svelte";
  import { getThumbnail } from "./tauri";

  export let projectId: string | undefined;
  export let assetId: string;
  export let kind: string;
  export let alt = "";
  export let small = false;
  /** "cover" crops to the frame, "contain" fits inside it, "full" keeps the photo's own shape. */
  export let fit: "cover" | "contain" | "full" = "cover";

  let box: HTMLElement;
  let visible = false;
  let loaded = false;
  const instant = developed.has(assetId);

  function onLoad() {
    loaded = true;
    developed.add(assetId);
  }

  // Only request thumbnails near the viewport: a cold one spawns a decoder.
  onMount(() => {
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          visible = true;
          observer.disconnect();
        }
      },
      { rootMargin: "200px" },
    );
    observer.observe(box);
    return () => observer.disconnect();
  });

  $: request = visible && projectId ? getThumbnail(projectId, assetId) : undefined;
</script>

<div bind:this={box} class="thumb {fit}" class:small>
  {#await request}
    <svg
      class="h-5 w-5 animate-spin"
      viewBox="0 0 24 24"
      fill="none"
      role="status"
      aria-label="Loading thumbnail"
    >
      <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4" />
      <path
        class="opacity-90"
        fill="currentColor"
        d="M4 12a8 8 0 0 1 8-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 0 1 4 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
      />
    </svg>
  {:then url}
    {#if url}
      <img src={url} {alt} class:loaded class:instant on:load={onLoad} />
    {:else}
      <span>{request ? "No preview" : kind}</span>
    {/if}
  {/await}
</div>

<style>
  .thumb {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 100%;
    aspect-ratio: 3 / 2;
    overflow: hidden;
    background: var(--wash);
    color: var(--ink-2);
    font-size: 13px;
  }

  .thumb.small {
    flex: none;
    width: 72px;
    font-size: 12px;
  }

  img {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: cover;
    /* Faces sit high in most photos, so crop from below. */
    object-position: 50% 30%;
    /* Develops from overexposed to true the first time it arrives. */
    filter: brightness(1.7) contrast(0.6) saturate(0.4);
    opacity: 0;
    transition:
      filter 700ms var(--ease-out),
      opacity 200ms linear;
  }

  img.loaded {
    filter: none;
    opacity: 1;
  }

  img.instant {
    transition: none;
  }

  .contain {
    aspect-ratio: 4 / 3;
  }

  .contain img {
    object-fit: contain;
    object-position: center;
  }

  .full {
    aspect-ratio: auto;
    min-height: 120px;
  }

  .full img {
    height: auto;
    max-height: 46vh;
    object-fit: contain;
  }
</style>
