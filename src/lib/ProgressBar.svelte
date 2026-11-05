<script lang="ts">
  /** Fraction done, 0 to 1. Null shows motion instead of a stuck 0%. */
  export let value: number | null;
  export let label: string;
</script>

<div
  class="progress"
  role="progressbar"
  aria-valuemin={0}
  aria-valuemax={100}
  aria-valuenow={value === null ? undefined : Math.round(value * 100)}
  aria-label={label}
>
  {#if value === null}
    <div class="indeterminate"></div>
  {:else}
    <div style="width: {(value * 100).toFixed(2)}%"></div>
  {/if}
</div>
