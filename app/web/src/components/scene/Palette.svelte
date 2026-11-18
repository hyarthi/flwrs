<script lang="ts" module>
  /** The drag-and-drop type of a palette entry (its data: the plugin id). */
  export const PLUGIN_DRAG_TYPE = 'application/x-flwrs-plugin';
</script>

<script lang="ts">
  // The installed plugins, grouped by their first `ui.categories` entry:
  // drag one onto the canvas (or click it) to add a node.
  import Zap from '@lucide/svelte/icons/zap';
  import Shuffle from '@lucide/svelte/icons/shuffle';
  import Inbox from '@lucide/svelte/icons/inbox';
  import { SvelteSet } from 'svelte/reactivity';
  import { pluginIconUrl, type Plugin } from '../../lib/sceneView';

  let {
    plugins,
    error,
    disabled = false,
    onadd,
  }: { plugins: Plugin[] | null; error: string | null; disabled?: boolean; onadd: (plugin: Plugin) => void } = $props();

  let filter = $state('');
  const failedIcons = new SvelteSet<string>();

  const TYPE_ORDER = { SOURCE: 0, TRANSFORM: 1, SINK: 2 } as const;

  const groups = $derived.by(() => {
    const q = filter.trim().toLowerCase();
    const match = (p: Plugin) =>
      !q ||
      p.name.toLowerCase().includes(q) ||
      p.id.toLowerCase().includes(q) ||
      (p.ui?.categories ?? []).some((c) => c.toLowerCase().includes(q));
    const byCategory = new Map<string, Plugin[]>();
    for (const p of plugins ?? []) {
      if (!match(p)) continue;
      const category = p.ui?.categories[0] ?? 'Other';
      byCategory.set(category, [...(byCategory.get(category) ?? []), p]);
    }
    return [...byCategory.entries()]
      .sort(([a], [b]) => (a === 'Other' ? 1 : b === 'Other' ? -1 : a.localeCompare(b)))
      .map(([category, list]) => ({
        category,
        plugins: list.sort((a, b) => TYPE_ORDER[a.type] - TYPE_ORDER[b.type] || a.name.localeCompare(b.name)),
      }));
  });
</script>

<aside class="palette" aria-label="Plugins">
  <input class="input" type="search" placeholder="Filter plugins" aria-label="Filter plugins" bind:value={filter} />
  <div class="list">
    {#if error}
      <p class="muted small">{error}</p>
    {:else if plugins === null}
      <p class="muted small">Loading plugins…</p>
    {:else if plugins.length === 0}
      <p class="muted small">No plugins installed yet. Install some on the <a href="/plugins">Plugins</a> page.</p>
    {:else if groups.length === 0}
      <p class="muted small">No plugin matches “{filter}”.</p>
    {/if}
    {#each groups as group (group.category)}
      <section>
        <h3>{group.category}</h3>
        {#each group.plugins as plugin (plugin.id)}
          {@const Icon = plugin.type === 'SOURCE' ? Zap : plugin.type === 'SINK' ? Inbox : Shuffle}
          <button
            class="entry"
            type="button"
            draggable={!disabled && plugin.installed}
            disabled={disabled || !plugin.installed}
            style:--node-color={plugin.ui?.color ?? undefined}
            title={plugin.installed
              ? `${plugin.description ?? plugin.name}\n${plugin.id} ${plugin.version} · ${plugin.type.toLowerCase()}\nDrag onto the canvas, or click to add`
              : `${plugin.id}: its files are missing on the server`}
            ondragstart={(e) => {
              e.dataTransfer?.setData(PLUGIN_DRAG_TYPE, plugin.id);
              if (e.dataTransfer) e.dataTransfer.effectAllowed = 'copy';
            }}
            onclick={() => onadd(plugin)}
          >
            <span class="icon" aria-hidden="true">
              {#if plugin.ui?.icon && !failedIcons.has(plugin.id)}
                <img src={pluginIconUrl(plugin.id)} alt="" width="16" height="16" onerror={() => failedIcons.add(plugin.id)} />
              {:else}
                <Icon size={14} strokeWidth={1.75} />
              {/if}
            </span>
            <span class="name">{plugin.name}</span>
            <span class="type">{plugin.type.slice(0, 1)}</span>
          </button>
        {/each}
      </section>
    {/each}
  </div>
</aside>

<style>
  .palette {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    width: 220px;
    min-height: 0;
    padding: var(--space-3);
    border-right: 1px solid var(--border);
    background: var(--surface);
  }

  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-height: 0;
    overflow: auto;
  }

  .small {
    font-size: var(--text-sm);
  }

  h3 {
    margin: 0 0 var(--space-1);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .entry {
    --node-color: var(--accent);
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    margin-bottom: 2px;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
    border-left: 3px solid var(--node-color);
    border-radius: var(--radius-sm);
    background: var(--surface);
    color: var(--text);
    font: inherit;
    font-size: var(--text-sm);
    text-align: left;
    cursor: grab;
  }

  .entry:hover:not(:disabled) {
    background: var(--surface-hover);
  }

  .entry:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .icon {
    display: grid;
    place-items: center;
    flex: none;
    width: 20px;
    height: 20px;
    color: var(--node-color);
  }

  .icon img {
    object-fit: contain;
  }

  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .type {
    color: var(--text-faint);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
</style>
