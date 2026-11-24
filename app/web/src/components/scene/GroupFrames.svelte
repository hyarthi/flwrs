<script lang="ts">
  // The shown family's group outlines, under the nodes and in flow
  // coordinates, so panning and zooming cost nothing. Redrawn at most once
  // per animation frame, and only for the groups a change touches.
  import { ViewportPortal } from '@xyflow/svelte';
  import { untrack } from 'svelte';
  import { FrameEngine, type Frame, type FrameGroup, type Rect } from '../../lib/groupFrames';

  let {
    groups,
    members,
    rects,
    colors,
    dragging,
    dropTarget,
    focused,
  }: {
    groups: FrameGroup[];
    members: Map<string, Set<string>>;
    rects: Map<string, Rect>;
    colors: Map<string, number>;
    dragging: boolean;
    dropTarget: string | null;
    focused: string | null;
  } = $props();

  /** Time for full outlines per animation frame. */
  const BUDGET_MS = 8;

  const engine = new FrameEngine();
  let frames = $state.raw<Frame[]>([]);
  let raf = 0;

  function schedule() {
    if (!raf) raf = requestAnimationFrame(tick);
  }

  function publish() {
    // Outer frames first, so inner ones paint on top.
    frames = groups.flatMap((g) => engine.frames.get(g.id) ?? []).sort((a, b) => b.box.width * b.box.height - a.box.width * a.box.height);
  }

  function tick() {
    raf = 0;
    engine.step(BUDGET_MS, dragging);
    publish();
    if (engine.busy) schedule();
  }

  $effect(() => {
    const input = { groups, members, rects };
    untrack(() => {
      if (engine.sync(input)) schedule();
    });
  });

  $effect(() => {
    if (!dragging && untrack(() => engine.settle())) schedule();
  });

  $effect(() => () => {
    if (raf) cancelAnimationFrame(raf);
    raf = 0;
  });

  /** The outlines as they stand, for telling what a drag landed on. */
  export const current = () => frames;
</script>

<ViewportPortal target="back">
  <svg class="frames" aria-hidden="true">
    {#each frames as f (f.id)}
      <g class="frame" class:drop={dropTarget === f.id} class:focused={focused === f.id} class:dimmed={focused !== null && focused !== f.id} style:--frame-color={`var(--group-${colors.get(f.id) ?? 1})`}>
        <path d={f.d} />
        <text x={f.box.x + 12} y={f.box.y + 14}>{f.id}</text>
      </g>
    {/each}
  </svg>
</ViewportPortal>

<style>
  .frames {
    position: absolute;
    top: 0;
    left: 0;
    width: 1px;
    height: 1px;
    overflow: visible;
    pointer-events: none;
  }

  path {
    fill: color-mix(in srgb, var(--frame-color) 9%, transparent);
    stroke: var(--frame-color);
    stroke-width: 1.5;
    stroke-linejoin: round;
    transition: opacity var(--dur-fast) var(--ease);
  }

  text {
    fill: var(--frame-color);
    font-size: var(--text-xs);
    font-weight: 600;
    paint-order: stroke;
    stroke: var(--bg);
    stroke-width: 3px;
  }

  .frame.dimmed {
    opacity: 0.25;
  }

  .frame.focused path {
    fill: color-mix(in srgb, var(--frame-color) 18%, transparent);
    stroke-width: 2.5;
  }

  .frame.drop path {
    fill: color-mix(in srgb, var(--frame-color) 26%, transparent);
    stroke-width: 3;
    stroke-dasharray: 6 4;
  }
</style>
