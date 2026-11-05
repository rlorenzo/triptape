<script lang="ts">
  import { onMount } from "svelte";
  import { getThumbnail } from "./tauri";

  export let projectId: string | undefined;
  export let assetId: string;
  export let kind: string;
  export let small = false;

  let box: HTMLElement;
  let visible = false;

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

<div
  bind:this={box}
  class="rounded overflow-hidden bg-gray-200 text-gray-500 flex items-center justify-center {small
    ? 'h-14 w-20 shrink-0 text-[10px]'
    : 'w-full aspect-video text-xs'}"
>
  {#await request}
    <svg
      class="h-5 w-5 animate-spin text-gray-500"
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
      <img src={url} alt="Thumbnail for {assetId}" class="w-full h-full object-cover" />
    {:else}
      {request ? "no preview" : kind}
    {/if}
  {/await}
</div>
