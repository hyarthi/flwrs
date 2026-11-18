<script lang="ts" module>
  import type { Change, Conflict } from '../../lib/sceneView';

  /** What an edge on the canvas shows (`data` of a Svelte Flow edge). */
  export interface FlowEdgeData extends Record<string, unknown> {
    change: Change | null;
    conflict: Conflict | null;
    parts: string[];
    /** The edge maps events (rules or a script). */
    mapped: boolean;
    /** The edge filters events. */
    filtered: boolean;
    problems: string[];
    onrestore: (id: string) => void;
  }
</script>

<script lang="ts">
  import { BaseEdge, EdgeLabel, getBezierPath, type EdgeProps } from '@xyflow/svelte';
  import Funnel from '@lucide/svelte/icons/funnel';
  import ArrowRightLeft from '@lucide/svelte/icons/arrow-right-left';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';

  let { id, data: given, selected, markerEnd, sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition }: EdgeProps & {
    data?: FlowEdgeData;
  } = $props();

  const NONE: FlowEdgeData = { change: null, conflict: null, parts: [], mapped: false, filtered: false, problems: [], onrestore: () => {} };
  const data = $derived(given ?? NONE);

  const geometry = $derived(getBezierPath({ sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition }));
  const removed = $derived(data.change === 'removed');
  const tone = $derived(
    data.conflict ? 'conflict' : removed ? 'removed' : data.change === 'added' ? 'added' : data.change === 'changed' ? 'changed' : '',
  );
  const badges = $derived(data.mapped || data.filtered || data.problems.length > 0 || removed || data.change === 'changed');
</script>

<BaseEdge path={geometry[0]} {markerEnd} class={`flow-edge ${tone} ${selected ? 'selected' : ''}`} interactionWidth={16} />

{#if badges}
  <EdgeLabel x={geometry[1]} y={geometry[2]} selectEdgeOnClick>
    <span class="badges" class:removed>
      {#if data.problems.length}
        <span class="badge-icon danger" title={data.problems.join('\n')}><CircleAlert size={12} strokeWidth={2} /></span>
      {/if}
      {#if data.filtered}
        <span class="badge-icon" title="Filters events"><Funnel size={12} strokeWidth={2} /></span>
      {/if}
      {#if data.mapped}
        <span class="badge-icon" title="Maps events"><ArrowRightLeft size={12} strokeWidth={2} /></span>
      {/if}
      {#if data.change === 'changed'}
        <span class="badge-icon accent" title={`Changed in the draft: ${data.parts.join(', ')}`}>•</span>
      {/if}
      {#if removed}
        <button class="badge-icon restore nodrag nopan" type="button" title="Restore edge" aria-label="Restore edge" onclick={() => data.onrestore(id)}>
          <RotateCcw size={12} strokeWidth={2} />
        </button>
      {/if}
    </span>
  </EdgeLabel>
{/if}

<style>
  :global(.svelte-flow__edge-path.flow-edge) {
    stroke: var(--border-strong);
    stroke-width: 1.5;
    transition: stroke var(--dur-fast) var(--ease);
  }

  :global(.svelte-flow__edge-path.flow-edge.selected) {
    stroke: var(--accent);
    stroke-width: 2;
  }

  :global(.svelte-flow__edge-path.flow-edge.added) {
    stroke: var(--accent);
  }

  :global(.svelte-flow__edge-path.flow-edge.conflict) {
    stroke: var(--warn);
  }

  :global(.svelte-flow__edge-path.flow-edge.removed) {
    stroke: var(--text-faint);
    stroke-dasharray: 5 5;
    opacity: 0.6;
  }

  .badges {
    display: inline-flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: var(--surface);
    box-shadow: var(--shadow-sm);
  }

  .badge-icon {
    display: grid;
    place-items: center;
    width: 18px;
    height: 18px;
    padding: 0;
    border: 0;
    border-radius: 50%;
    background: var(--surface-2);
    color: var(--text-muted);
    font-size: 14px;
    line-height: 1;
  }

  .badge-icon.danger {
    background: var(--danger-soft);
    color: var(--danger);
  }

  .badge-icon.accent {
    background: var(--accent-soft);
    color: var(--accent);
  }

  .restore {
    cursor: pointer;
  }

  .restore:hover {
    color: var(--accent);
  }
</style>
