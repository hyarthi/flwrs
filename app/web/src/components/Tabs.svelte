<script lang="ts" module>
  // Tabs as links (route-driven, e.g. a scene's Layout / Stats) or as
  // buttons (local state, e.g. inside the node modal).
  export interface Tab {
    id: string;
    label: string;
    href?: string;
  }
</script>

<script lang="ts">
  let { tabs, active, onselect }: { tabs: Tab[]; active: string; onselect?: (id: string) => void } = $props();
</script>

<div class="tabs" role="tablist">
  {#each tabs as tab (tab.id)}
    {#if tab.href}
      <a href={tab.href} role="tab" aria-selected={active === tab.id} class:active={active === tab.id}>{tab.label}</a>
    {:else}
      <button type="button" role="tab" aria-selected={active === tab.id} class:active={active === tab.id} onclick={() => onselect?.(tab.id)}>
        {tab.label}
      </button>
    {/if}
  {/each}
</div>

<style>
  .tabs {
    display: flex;
    gap: var(--space-1);
    border-bottom: 1px solid var(--border);
  }

  a,
  button {
    position: relative;
    padding: var(--space-2) var(--space-3);
    border: 0;
    background: none;
    color: var(--text-muted);
    font: inherit;
    font-weight: 500;
    cursor: pointer;
  }

  a:hover,
  button:hover {
    color: var(--text);
  }

  .active {
    color: var(--text);
  }

  .active::after {
    content: '';
    position: absolute;
    inset: auto var(--space-2) -1px;
    height: 2px;
    border-radius: 2px;
    background: var(--accent);
  }
</style>
