<script lang="ts">
  // A payload schema as a tree of fields. Clicking a field hands its dot
  // path to `onpick` (arrays as `[0]`, map values as `["key"]`). Fields can
  // be dragged (to the other tree, a rule or a path field), and the other
  // tree's fields dropped here (`ondropfield`).
  import SchemaTree from './SchemaTree.svelte';
  import { FIELD_DRAG_TYPE, readDraggedField, segment, type Side } from '../../lib/mapping';
  import type { SchemaField } from '../../lib/sceneView';

  let {
    fields,
    side,
    prefix = '',
    onpick,
    ondropfield,
    used = new Set<string>(),
  }: {
    fields: SchemaField[];
    side: Side;
    prefix?: string;
    onpick: (path: string) => void;
    /** A field from the other tree dropped on one of these (`path`). */
    ondropfield?: (path: string, dropped: string) => void;
    used?: Set<string>;
  } = $props();

  let over = $state<string | null>(null);

  const pathOf = (key: string) => prefix + segment(key, prefix === '');
  const short = (type: string) => type.replace('PACKED_', 'packed ').toLowerCase();
</script>

<ul class:nested={prefix !== ''}>
  {#each fields as field (field.key)}
    {@const path = pathOf(field.key)}
    <li>
      <button
        type="button"
        class="field"
        class:used={used.has(path)}
        class:over={over === path}
        draggable="true"
        title={`${path}${field.description ? `\n${field.description}` : ''}${field.content_type ? `\n${field.content_type}` : ''}\nClick or drag`}
        onclick={() => onpick(path)}
        ondragstart={(e) => {
          e.dataTransfer?.setData(FIELD_DRAG_TYPE, JSON.stringify({ side, path }));
          if (e.dataTransfer) e.dataTransfer.effectAllowed = 'link';
        }}
        ondragover={(e) => {
          if (!ondropfield || !e.dataTransfer?.types.includes(FIELD_DRAG_TYPE)) return;
          e.preventDefault();
          over = path;
        }}
        ondragleave={() => (over = over === path ? null : over)}
        ondrop={(e) => {
          over = null;
          const dropped = readDraggedField(e);
          if (!dropped || dropped.side === side || !ondropfield) return;
          e.preventDefault();
          ondropfield(path, dropped.path);
        }}
      >
        <span class="key">{field.key}</span>
        <span class="type">{short(field.type)}{field.element ? ` of ${short(field.element.type)}` : ''}</span>
      </button>
      {#if field.fields?.length}
        <SchemaTree fields={field.fields} {side} prefix={path} {onpick} {ondropfield} {used} />
      {:else if field.element?.fields?.length}
        <SchemaTree fields={field.element.fields} {side} prefix={field.type === 'MAP' ? `${path}["key"]` : `${path}[0]`} {onpick} {ondropfield} {used} />
      {/if}
    </li>
  {/each}
</ul>

<style>
  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  ul.nested {
    margin-left: var(--space-3);
    padding-left: var(--space-2);
    border-left: 1px solid var(--border);
  }

  .field {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-2);
    width: 100%;
    padding: 2px var(--space-2);
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text);
    font: inherit;
    font-size: var(--text-sm);
    text-align: left;
    cursor: pointer;
  }

  .field:hover {
    background: var(--surface-hover);
  }

  .field.over {
    outline: 2px dashed var(--accent);
    outline-offset: -2px;
  }

  .field.used .key {
    color: var(--accent);
    font-weight: 600;
  }

  .key {
    overflow: hidden;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .type {
    flex: none;
    color: var(--text-faint);
    font-size: var(--text-xs);
  }
</style>
