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
    {kind}
  {:then url}
    {#if url}
      <img src={url} alt="Thumbnail for {assetId}" class="w-full h-full object-cover" />
    {:else}
      {request ? "no preview" : kind}
    {/if}
  {/await}
</div>
