<script lang="ts" module>
  import type { Port } from '../../lib/graphModel';

  /** What an inner node on the canvas shows (`data` of a Svelte Flow node). */
  export interface GraphNodeData extends Record<string, unknown> {
    title: string;
    typeTitle: string;
    inputs: Port[];
    outputs: Port[];
    portTitle: (type: string) => string;
    problems: string[];
    /** The groups it's in. */
    chips: { id: string; color: number; family: string }[];
    /** Settings filled from what its file says, not yet reviewed. */
    detected: boolean;
    /** What a running node reports; shown when there is something to show. */
    live?: { stats?: string; preview?: string };
  }
</script>

<script lang="ts">
  import { Handle, Position as Side } from '@xyflow/svelte';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';
  import Sparkles from '@lucide/svelte/icons/sparkles';
  import { useGroupFocus } from '../../lib/groupFocus.svelte';

  let { data, selected }: { data: GraphNodeData; selected?: boolean } = $props();

  const focus = useGroupFocus();
  const inFocus = $derived(!!focus?.id && data.chips.some((c) => c.id === focus.id));
  const dimmed = $derived(!!focus?.id && !inFocus);
  const rows = $derived(Math.max(data.inputs.length, data.outputs.length));
</script>

<div class="node" class:selected class:bad={data.problems.length > 0} class:dimmed class:in-focus={inFocus} data-testid="graph-node">
  <div class="head">
    <span class="text">
      <span class="title">{data.title}</span>
      <span class="subtitle">{data.typeTitle}</span>
    </span>
    {#if data.detected}
      <span class="mark detected" title="Some settings were filled from the model file: review them"><Sparkles size={13} strokeWidth={2} /></span>
    {/if}
    {#if data.problems.length}
      <span class="mark danger" title={data.problems.join('\n')}><CircleAlert size={14} strokeWidth={2} /></span>
    {/if}
  </div>

  {#if rows}
    <div class="ports" style:--rows={rows}>
      {#each data.inputs as p, i (p.name)}
        <span class="port in" style:grid-row={i + 1} title={data.portTitle(p.type)}>
          <Handle id={p.name} type="target" position={Side.Left} class={`port-handle t-${p.type}`} />
          {p.name}
        </span>
      {/each}
      {#each data.outputs as p, i (p.name)}
        <span class="port out" style:grid-row={i + 1} title={data.portTitle(p.type)}>
          {p.name}
          <Handle id={p.name} type="source" position={Side.Right} class={`port-handle t-${p.type}`} />
        </span>
      {/each}
    </div>
  {/if}

  {#if data.chips.length}
    <span class="chips">
      {#each data.chips as c (c.id)}
        <button
          class="chip"
          type="button"
          style:--chip-color={`var(--group-${c.color})`}
          title={`${c.family} group ${c.id}`}
          onpointerenter={() => focus && (focus.id = c.id)}
          onpointerleave={() => focus && (focus.id = null)}
          onfocus={() => focus && (focus.id = c.id)}
          onblur={() => focus && (focus.id = null)}
        >{c.id}</button>
      {/each}
    </span>
  {/if}

  <!-- A running node's own numbers and picture, when it reports them. -->
  {#if data.live?.stats}<div class="live-stats mono" class:unmeasured={data.live.stats === 'not measured'} data-testid="node-live-line">{data.live.stats}</div>{/if}
  {#if data.live?.preview}<img class="live-preview" src={data.live.preview} alt="" />{/if}
</div>

<style>
  .node {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    width: 220px;
    padding: var(--space-2) 0;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--text);
    box-shadow: var(--shadow-sm);
    font-size: var(--text-sm);
    transition:
      border-color var(--dur-fast) var(--ease),
      opacity var(--dur) var(--ease);
  }

  .node.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--focus-ring);
  }

  .node.bad {
    border-color: var(--danger);
  }

  .node.dimmed {
    opacity: 0.25;
  }

  .node.in-focus {
    border-color: var(--text-muted);
  }

  .head {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0 var(--space-3);
  }

  .text {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-width: 0;
  }

  .title,
  .subtitle {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .title {
    font-weight: 600;
  }

  .subtitle {
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .mark {
    display: inline-flex;
  }

  .mark.danger {
    color: var(--danger);
  }

  .mark.detected {
    color: var(--accent);
  }

  .ports {
    display: grid;
    grid-template-columns: 1fr 1fr;
    grid-template-rows: repeat(var(--rows), 20px);
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .port {
    position: relative;
    display: flex;
    align-items: center;
    padding: 0 var(--space-3);
  }

  .port.in {
    grid-column: 1;
  }

  .port.out {
    grid-column: 2;
    justify-content: flex-end;
  }

  .port :global(.port-handle) {
    width: 10px;
    height: 10px;
    border: 2px solid var(--surface);
    background: var(--text-muted);
  }

  .port :global(.port-handle.t-frame) {
    background: var(--series-1);
  }

  .port :global(.port-handle.t-regions) {
    background: var(--series-2);
  }

  .port :global(.port-handle.t-objects) {
    background: var(--series-3);
  }

  .port :global(.svelte-flow__handle.valid) {
    background: var(--ok);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
    padding: 0 var(--space-3);
  }

  .chip {
    padding: 0 var(--space-1);
    border: 1px solid var(--chip-color);
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text);
    font-size: var(--text-xs);
    cursor: default;
  }

  .live-stats {
    padding: 0 var(--space-3);
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .live-stats:not(.unmeasured) {
    color: var(--accent);
  }

  .live-stats.unmeasured {
    font-style: italic;
  }

  .live-preview {
    display: block;
    width: calc(100% - 2 * var(--space-2));
    margin: 0 var(--space-2);
    border-radius: var(--radius-sm);
  }
</style>
