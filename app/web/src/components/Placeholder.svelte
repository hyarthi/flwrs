<script lang="ts">
  // Stands in for a page that isn't built yet: what it will show.
  let { title, items }: { title: string; items: string[] } = $props();
</script>

<div class="placeholder card">
  <span class="badge accent">Coming soon</span>
  <h2>{title}</h2>
  <ul class="muted">
    {#each items as item, i (i)}
      <li>{item}</li>
    {/each}
  </ul>
</div>

<style>
  .placeholder {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-3);
    max-width: 640px;
    padding: var(--space-5);
  }

  ul {
    margin: 0;
    padding-left: var(--space-5);
  }
</style>
