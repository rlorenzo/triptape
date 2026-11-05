<script lang="ts">
  /** What went wrong and what to do, with the raw error kept out of the way. */
  export let message: string;
  export let detail = "";
</script>

<div class="error note" role="alert">
  <p>{message}</p>
  {#if detail}
    <details>
      <summary>Technical details</summary>
      <p class="raw">{detail}</p>
    </details>
  {/if}
</div>

<style>
  .note {
    margin-bottom: 16px;
  }

  summary {
    margin-top: 4px;
    font-size: 13px;
    cursor: pointer;
  }

  .raw {
    margin-top: 4px;
    font-size: 13px;
    overflow-wrap: anywhere;
  }
</style>
