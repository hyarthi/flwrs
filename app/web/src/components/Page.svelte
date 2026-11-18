<script lang="ts">
  // A page's body: padding and an optional row of actions.
  import type { Snippet } from 'svelte';

  let { actions, children, full = false }: { actions?: Snippet; children: Snippet; full?: boolean } = $props();
</script>

<div class="page" class:full>
  {#if actions}
    <div class="actions">{@render actions()}</div>
  {/if}
  {@render children()}
</div>

<style>
  .page {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-5);
  }

  .page.full {
    height: 100%;
  }

  .actions {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
  }
</style>
