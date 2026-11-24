<script lang="ts" module>
  /** What a link on the canvas shows (`data` of a Svelte Flow edge). */
  export interface GraphEdgeData extends Record<string, unknown> {
    badges: string[];
    problems: string[];
    /** Both ends did work in the live view's last interval. */
    active?: boolean;
  }
</script>

<script lang="ts">
  import { BaseEdge, EdgeLabel, getBezierPath, type EdgeProps } from '@xyflow/svelte';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';

  let { data, selected, markerEnd, sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition }: EdgeProps & { data?: GraphEdgeData } = $props();

  const geometry = $derived(getBezierPath({ sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition }));
</script>

<BaseEdge path={geometry[0]} {markerEnd} class={`graph-link ${selected ? 'selected' : ''} ${data?.problems.length ? 'bad' : ''} ${data?.active ? 'active' : ''}`} interactionWidth={16} />
<EdgeLabel x={geometry[1]} y={geometry[2]} selectEdgeOnClick transparent>
  <span class="badges" class:selected data-testid="link-badges">
    {#if data?.problems.length}
      <span class="problem" title={data.problems.join('\n')}><CircleAlert size={12} strokeWidth={2} /></span>
    {/if}
    {#each data?.badges ?? [] as badge, i (i)}
      <span class="badge-text" class:strong={i > 0}>{badge}</span>
    {/each}
  </span>
</EdgeLabel>

<style>
  :global(.graph-link.active) {
    stroke: var(--accent) !important;
    stroke-dasharray: 6 4;
    animation: graph-link-flow 0.8s linear infinite;
  }

  @keyframes -global-graph-link-flow {
    to {
      stroke-dashoffset: -10;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    :global(.graph-link.active) {
      animation: none;
      stroke-dasharray: none;
    }
  }

  :global(.graph-link.bad) {
    stroke: var(--danger) !important;
  }

  .badges {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 1px var(--space-1);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface);
    color: var(--text-muted);
    font-size: var(--text-xs);
    white-space: nowrap;
  }

  .badges.selected {
    border-color: var(--accent);
  }

  .strong {
    color: var(--text);
    font-weight: 600;
  }

  .problem {
    display: inline-flex;
    color: var(--danger);
  }
</style>
