<script lang="ts">
  // A list of objects (TOML's arrays of tables, e.g. `[[logging.sinks]]`)
  // as rows: each collapsed to a summary of its set values, expanded to its
  // fields; add, remove, move up / down. An emptied list is left out.
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import ArrowUp from '@lucide/svelte/icons/arrow-up';
  import ArrowDown from '@lucide/svelte/icons/arrow-down';
  import Plus from '@lucide/svelte/icons/plus';
  import Trash from '@lucide/svelte/icons/trash-2';
  import SchemaField from './SchemaField.svelte';
  import { resolve, type Schema } from '../../lib/schemaForm';

  let {
    root,
    items,
    value,
    label,
    description,
    required = false,
    disabled = false,
    onchange,
  }: {
    root: Schema;
    /** The (resolved) schema of one item. */
    items: Schema;
    value: unknown;
    label: string;
    description?: string;
    required?: boolean;
    disabled?: boolean;
    onchange: (value: unknown) => void;
  } = $props();

  type Row = Record<string, unknown>;
  const rows = $derived(Array.isArray(value) ? (value as Row[]) : []);
  /** Which rows are open (by position). */
  let open = $state<boolean[]>([]);

  const keys = $derived((items['x-order'] as string[] | undefined) ?? Object.keys(items.properties ?? {}));

  function emit(next: Row[]) {
    onchange(next.length || required ? next : undefined);
  }

  function add() {
    // Required choices start at their first option, so the row is valid sooner.
    const row: Row = {};
    for (const key of items.required ?? []) {
      const prop = resolve(root, (items.properties ?? {})[key] ?? {});
      if (prop.default !== undefined) row[key] = prop.default;
      else if (prop.enum?.length) row[key] = prop.enum[0];
    }
    open = [...rows.map((_, i) => open[i] ?? false), true];
    emit([...rows, row]);
  }

  function remove(i: number) {
    open = open.filter((_, j) => j !== i);
    emit(rows.filter((_, j) => j !== i));
  }

  function move(i: number, by: -1 | 1) {
    const j = i + by;
    const next = [...rows];
    [next[i], next[j]] = [next[j], next[i]];
    const flags = rows.map((_, k) => open[k] ?? false);
    [flags[i], flags[j]] = [flags[j], flags[i]];
    open = flags;
    emit(next);
  }

  function setField(i: number, key: string, v: unknown) {
    const row = { ...rows[i] };
    if (v === undefined) delete row[key];
    else row[key] = v;
    emit(rows.map((r, j) => (j === i ? row : r)));
  }

  /** The row's set scalar values, as a one-line summary. */
  function summary(row: Row): string {
    const parts = keys
      .filter((k) => row[k] !== undefined && row[k] !== null && typeof row[k] !== 'object')
      .slice(0, 3)
      .map((k) => `${k}: ${row[k]}`);
    return parts.join(' · ') || 'nothing set yet';
  }
</script>

<div class="field">
  <span class="label">{label}{required ? ' *' : ''} <span class="muted">({rows.length})</span></span>
  {#if description}<span class="hint">{description}</span>{/if}
  <ol class="rows">
    {#each rows as row, i (i)}
      <li class:open={open[i]}>
        <div class="head">
          <button type="button" class="toggle" aria-expanded={open[i] ?? false} onclick={() => (open[i] = !open[i])}>
            <ChevronRight size={14} strokeWidth={2} />
            <span class="mono muted">#{i + 1}</span>
            <span class="summary">{summary(row)}</span>
          </button>
          <button class="btn ghost icon" type="button" aria-label="Move up" title="Move up" disabled={disabled || i === 0} onclick={() => move(i, -1)}><ArrowUp size={14} strokeWidth={1.75} /></button>
          <button class="btn ghost icon" type="button" aria-label="Move down" title="Move down" disabled={disabled || i === rows.length - 1} onclick={() => move(i, 1)}><ArrowDown size={14} strokeWidth={1.75} /></button>
          <button class="btn ghost icon danger" type="button" aria-label={`Remove #${i + 1}`} title="Remove" {disabled} onclick={() => remove(i)}><Trash size={14} strokeWidth={1.75} /></button>
        </div>
        {#if open[i]}
          <div class="body">
            {#each keys as key (key)}
              <SchemaField
                {root}
                schema={resolve(root, (items.properties ?? {})[key] ?? {})}
                name={key}
                label={key}
                value={row[key]}
                required={items.required?.includes(key)}
                {disabled}
                onchange={(v) => setField(i, key, v)}
              />
            {/each}
          </div>
        {/if}
      </li>
    {/each}
  </ol>
  <button class="btn ghost add" type="button" {disabled} onclick={add}><Plus size={14} strokeWidth={1.75} />Add</button>
</div>

<style>
  .label {
    font-weight: 500;
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface);
  }

  .head {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding-right: var(--space-1);
  }

  .toggle {
    display: flex;
    flex: 1;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
    padding: var(--space-2);
    border: 0;
    background: none;
    color: var(--text);
    text-align: left;
    cursor: pointer;
  }

  .toggle :global(svg) {
    flex: none;
    transition: transform var(--dur-fast) var(--ease);
  }

  li.open .toggle :global(svg) {
    transform: rotate(90deg);
  }

  .summary {
    overflow: hidden;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3) var(--space-3);
    border-top: 1px solid var(--border);
  }

  .add {
    align-self: flex-start;
  }
</style>
