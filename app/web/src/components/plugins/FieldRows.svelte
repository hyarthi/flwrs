<script lang="ts">
  // A payload schema's fields as editable rows: key, type, description, a
  // BYTES field's content type; OBJECT fields nest, ARRAY / MAP name their
  // element's type (which may nest again).
  import Plus from '@lucide/svelte/icons/plus';
  import Trash from '@lucide/svelte/icons/trash-2';
  import ArrowUp from '@lucide/svelte/icons/arrow-up';
  import FieldRows from './FieldRows.svelte';
  import { FIELD_TYPES, newRow, type FieldRow, type FieldType } from '../../lib/remotePlugin';

  let { rows = $bindable(), depth = 0 }: { rows: FieldRow[]; depth?: number } = $props();

  const label = (t: string) => t.replace('PACKED_', 'packed ').toLowerCase();

  /** A type change keeps what still applies. */
  function retype(row: FieldRow, type: FieldType) {
    row.type = type;
    if (type === 'ARRAY' || type === 'MAP') row.element ??= newRow();
    else row.element = null;
    if (type !== 'OBJECT') row.fields = [];
    if (type !== 'BYTES') row.content_type = '';
  }

  function move(i: number) {
    const [row] = rows.splice(i, 1);
    rows.splice(i - 1, 0, row);
  }
</script>

{#snippet typeOf(row: FieldRow, keyed: boolean)}
  <select class="input type" aria-label={keyed ? `Type of ${row.key || 'the field'}` : 'Element type'} value={row.type} onchange={(e) => retype(row, e.currentTarget.value as FieldType)}>
    {#each FIELD_TYPES as t (t)}<option value={t}>{label(t)}</option>{/each}
  </select>
  {#if row.type === 'BYTES'}
    <input class="input ct" placeholder="content type, e.g. image/png" aria-label="Content type" bind:value={row.content_type} />
  {/if}
  {#if (row.type === 'ARRAY' || row.type === 'MAP') && row.element}
    <span class="muted of">{row.type === 'ARRAY' ? 'of' : 'of values'}</span>
    {@render typeOf(row.element, false)}
  {/if}
{/snippet}

{#snippet objectsOf(row: FieldRow)}
  {#if row.type === 'OBJECT'}
    <FieldRows bind:rows={row.fields} depth={depth + 1} />
  {:else if (row.type === 'ARRAY' || row.type === 'MAP') && row.element}
    {@render objectsOf(row.element)}
  {/if}
{/snippet}

<ul class="rows" class:nested={depth > 0}>
  {#each rows as row, i (row)}
    <li>
      <div class="row">
        <input class="input key mono" placeholder="key" aria-label="Key" bind:value={row.key} />
        {@render typeOf(row, true)}
        <input class="input description" placeholder="description" aria-label="Description" bind:value={row.description} />
        <button class="btn ghost icon" type="button" aria-label="Move up" title="Move up" disabled={i === 0} onclick={() => move(i)}>
          <ArrowUp size={14} strokeWidth={1.75} />
        </button>
        <button class="btn ghost icon danger" type="button" aria-label={`Remove ${row.key || 'field'}`} title="Remove" onclick={() => rows.splice(i, 1)}>
          <Trash size={14} strokeWidth={1.75} />
        </button>
      </div>
      {@render objectsOf(row)}
    </li>
  {/each}
  <li>
    <button class="btn ghost add" type="button" onclick={() => rows.push(newRow())}>
      <Plus size={14} strokeWidth={1.75} />{depth > 0 ? 'Add a nested field' : 'Add a field'}
    </button>
  </li>
</ul>

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .rows.nested {
    margin: var(--space-1) 0 var(--space-1) var(--space-4);
    padding-left: var(--space-3);
    border-left: 2px solid var(--border);
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1);
  }

  .key {
    width: 160px;
  }

  .type {
    width: 140px;
  }

  .ct {
    width: 180px;
  }

  .description {
    flex: 1;
    min-width: 140px;
  }

  .of {
    font-size: var(--text-sm);
  }

  .add {
    align-self: flex-start;
  }
</style>
