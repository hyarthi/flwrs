<script lang="ts">
  // The canvas's groups: which family's frames show, and every group by
  // family. Hovering an entry highlights its members.
  import ChevronDown from '@lucide/svelte/icons/chevron-down';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';
  import { FAMILIES, groupMessages, isEditable, policySummary, type SceneGroup } from '../../lib/groups';
  import { useGroupFocus } from '../../lib/groupFocus.svelte';

  let {
    groups,
    members,
    colors,
    problems,
    family = $bindable(),
    editable = isEditable,
  }: {
    groups: SceneGroup[];
    members: Map<string, Set<string>>;
    colors: Map<string, number>;
    problems: string[];
    /** The family whose frames show; `null` = none. */
    family: string | null;
    /** Whether a group can be edited here (a scene's families by default). */
    editable?: (g: SceneGroup) => boolean;
  } = $props();

  const focus = useGroupFocus();
  /** The list can fold away: it sits on the canvas. */
  let listOpen = $state(true);
  const count = (f: string) => groups.filter((g) => g.family === f).length;
  const known = $derived(new Set<string>(FAMILIES.map((f) => f.id)));
  const listed = $derived([...groups].sort((a, b) => Number(b.family === family) - Number(a.family === family) || a.id.localeCompare(b.id)));
</script>

<div class="legend">
  <div class="toggle" role="radiogroup" aria-label="Group frames">
    {#each FAMILIES as f (f.id)}
      <button type="button" role="radio" aria-checked={family === f.id} class:on={family === f.id} onclick={() => (family = f.id)}>
        {f.label}{count(f.id) ? ` ${count(f.id)}` : ''}
      </button>
    {/each}
    <button type="button" role="radio" aria-checked={family === null} class:on={family === null} onclick={() => (family = null)}>None</button>
    <button type="button" class="fold" aria-expanded={listOpen} aria-label={listOpen ? 'Hide the list of groups' : 'Show the list of groups'} title={listOpen ? 'Hide the list' : 'Show the list'} onclick={() => (listOpen = !listOpen)}>
      {#if listOpen}<ChevronDown size={12} strokeWidth={2} />{:else}<ChevronRight size={12} strokeWidth={2} />{/if}
    </button>
  </div>
  {#if listOpen}
  <ul>
    {#each listed as g (g.id)}
      {@const messages = groupMessages(problems, g.id)}
      <li>
        <button
          type="button"
          class="entry"
          class:other={g.family !== family}
          style:--chip-color={`var(--group-${colors.get(g.id) ?? 1})`}
          title={[`${g.family}: ${policySummary(g)}`, !editable(g) ? 'Read-only: this level doesn’t accept this family' : '', ...messages].filter(Boolean).join('\n')}
          onpointerenter={() => focus && (focus.id = g.id)}
          onpointerleave={() => focus && (focus.id = null)}
          onfocus={() => focus && (focus.id = g.id)}
          onblur={() => focus && (focus.id = null)}
        >
          <span class="swatch"></span>
          <span class="name">{g.id}</span>
          {#if !known.has(g.family) || g.family !== family}<span class="family">{g.family}</span>{/if}
          <span class="count">{members.get(g.id)?.size ?? 0}</span>
          {#if messages.length}<span class="problem"><CircleAlert size={12} strokeWidth={2} /></span>{/if}
        </button>
      </li>
    {/each}
  </ul>
  {/if}
</div>

<style>
  .legend {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    width: max-content;
    max-width: min(340px, 60vw);
    max-height: 40vh;
    padding: var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: var(--shadow-sm);
    font-size: var(--text-xs);
  }

  .toggle {
    display: flex;
    gap: 2px;
    white-space: nowrap;
  }

  .toggle button {
    padding: 2px var(--space-2);
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text-muted);
    font: inherit;
    cursor: pointer;
  }

  .toggle button:hover {
    background: var(--surface-hover);
  }

  .toggle .fold {
    display: grid;
    place-items: center;
    margin-left: auto;
    padding: 2px var(--space-1);
  }

  .toggle button.on {
    background: var(--accent-soft);
    color: var(--accent);
    font-weight: 600;
  }

  ul {
    margin: 0;
    padding: 0;
    overflow-y: auto;
    list-style: none;
  }

  .entry {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: 2px var(--space-1);
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text);
    font: inherit;
    text-align: left;
    cursor: default;
  }

  .entry:hover,
  .entry:focus-visible {
    background: var(--surface-hover);
  }

  .entry.other {
    color: var(--text-muted);
  }

  .swatch {
    flex: none;
    width: 10px;
    height: 10px;
    border-radius: 3px;
    background: var(--chip-color);
  }

  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    font-family: var(--font-mono);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .family,
  .count {
    flex: none;
    color: var(--text-faint);
  }

  .problem {
    display: grid;
    flex: none;
    place-items: center;
    color: var(--danger);
  }
</style>
