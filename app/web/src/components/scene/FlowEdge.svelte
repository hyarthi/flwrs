<script lang="ts" module>
  import type { Change, Conflict, NodeQueue, Position } from '../../lib/sceneView';

  /** How events flow along an edge, shown by its motes. */
  export interface EdgeFlow {
    /** After the box: working (green), the target's queue spilling (yellow), or blocked / dropping (orange). */
    tone: 'ok' | 'spill' | 'jam';
    /** The target's queue drops events: a mote leaves the box downwards. */
    drop: boolean;
  }

  /** What an edge on the canvas shows (`data` of a Svelte Flow edge). */
  export interface FlowEdgeData extends Record<string, unknown> {
    change: Change | null;
    conflict: Conflict | null;
    parts: string[];
    /** The edge maps events (rules or a script). */
    mapped: boolean;
    /** The edge filters events (a mote leaves the box upwards). */
    filtered: boolean;
    problems: string[];
    /** The saved box position; `null` = placed automatically. */
    via: Position | null;
    /** Automatic placement: how far (px) to push the box off the straight line, so parallel edges don't overlap. */
    offset: number;
    /** The target node's inbound queue. */
    queue: NodeQueue | null;
    /** `null` = no animation (scene inactive, an end down, a removed edge). */
    flow: EdgeFlow | null;
    onrestore: (id: string) => void;
    onconfig: (id: string) => void;
    /** A box dragged (or reset with `null`). */
    onmovebox: (id: string, via: Position | null) => void;
  }
</script>

<script lang="ts">
  import { BaseEdge, EdgeLabel, useSvelteFlow, type EdgeProps } from '@xyflow/svelte';
  import Funnel from '@lucide/svelte/icons/funnel';
  import ArrowRightLeft from '@lucide/svelte/icons/arrow-right-left';
  import Cog from '@lucide/svelte/icons/cog';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import MemoryStick from '@lucide/svelte/icons/memory-stick';
  import HardDriveDownload from '@lucide/svelte/icons/hard-drive-download';
  import Database from '@lucide/svelte/icons/database';

  let { id, source, target, data: given, selected, markerEnd, sourceX, sourceY, targetX, targetY }: EdgeProps & {
    data?: FlowEdgeData;
  } = $props();

  const NONE: FlowEdgeData = {
    change: null,
    conflict: null,
    parts: [],
    mapped: false,
    filtered: false,
    problems: [],
    via: null,
    offset: 0,
    queue: null,
    flow: null,
    onrestore: () => {},
    onconfig: () => {},
    onmovebox: () => {},
  };
  const data = $derived(given ?? NONE);
  const flow = useSvelteFlow();

  // ---- geometry: source → box → target ----

  let dragging = $state<Position | null>(null);
  const S = $derived({ x: sourceX, y: sourceY });
  const T = $derived({ x: targetX, y: targetY });
  const loop = $derived(source === target);

  const autoVia = $derived.by(() => {
    if (loop) return { x: (S.x + T.x) / 2, y: Math.min(S.y, T.y) - 80 };
    // Offsets are along one normal per node pair (from the lower id to the
    // higher), so A→B and B→A go to opposite sides.
    const [a, b] = source < target ? [S, T] : [T, S];
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    const len = Math.hypot(dx, dy) || 1;
    return { x: (S.x + T.x) / 2 + (-dy / len) * data.offset, y: (S.y + T.y) / 2 + (dx / len) * data.offset };
  });
  const V = $derived(dragging ?? data.via ?? autoVia);

  const fmt = (p: Position) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`;
  const geometry = $derived.by(() => {
    let tx = T.x - S.x;
    let ty = T.y - S.y;
    if (loop) [tx, ty] = [-1, 0];
    const tl = Math.hypot(tx, ty) || 1;
    [tx, ty] = [tx / tl, ty / tl];
    const d1 = Math.hypot(V.x - S.x, V.y - S.y);
    const d2 = Math.hypot(T.x - V.x, T.y - V.y);
    const k1 = Math.min(Math.max(d1 * 0.5, 24), 140);
    const k2 = Math.min(Math.max(d2 * 0.5, 24), 140);
    const v1 = Math.min(d1 * 0.4, 60);
    const v2 = Math.min(d2 * 0.4, 60);
    const first = `M${fmt(S)} C${fmt({ x: S.x + k1, y: S.y })} ${fmt({ x: V.x - tx * v1, y: V.y - ty * v1 })} ${fmt(V)}`;
    const second = ` C${fmt({ x: V.x + tx * v2, y: V.y + ty * v2 })} ${fmt({ x: T.x - k2, y: T.y })} ${fmt(T)}`;
    return { first, full: first + second };
  });

  // Lengths, so the motes reach the box half-way through their trip.
  let firstPath = $state<SVGPathElement>();
  let fullPath = $state<SVGPathElement>();
  const EXIT = 26;
  const lengths = $derived.by(() => {
    void geometry;
    const first = firstPath?.getTotalLength() ?? 1;
    const full = fullPath?.getTotalLength() ?? 2;
    return { first, full };
  });
  const atBox = $derived(Math.min(Math.max(lengths.first / lengths.full, 0.01), 0.99));
  const atBoxExit = $derived(lengths.first / (lengths.first + EXIT));

  // A dropped event leaves at a bearing of 135° or 215° (down right / down left).
  const dropBearing = Math.random() < 0.5 ? 135 : 215;
  const exitPath = (bearing: number) => {
    const rad = (bearing * Math.PI) / 180;
    return `${geometry.first} L${fmt({ x: V.x + Math.sin(rad) * EXIT, y: V.y - Math.cos(rad) * EXIT })}`;
  };

  // The three motes; one may leave at the box (the filter's takes the middle one).
  const motes = $derived.by(() => {
    if (!data.flow) return [];
    const exits: (number | null)[] = [null, null, null];
    if (data.filtered) exits[1] = 0;
    if (data.flow.drop) exits[data.filtered ? 2 : 1] = dropBearing;
    return exits.map((bearing, i) => ({ i, bearing }));
  });

  const reduceMotion = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;
  const DUR = '2.4s';
  // Fast near the nodes, slow through the box: ease-out into it, ease-in out of it.
  const SPLINES = '0 0 0.58 1;0.42 0 1 1;0 0 1 1';

  // ---- the box: drag to place, double-click to reset ----

  let press: { x: number; y: number; moved: boolean } | null = null;
  function down(e: PointerEvent) {
    if ((e.target as Element).closest('button')) return;
    press = { x: e.clientX, y: e.clientY, moved: false };
    (e.currentTarget as Element).setPointerCapture(e.pointerId);
  }
  function move(e: PointerEvent) {
    if (!press) return;
    if (!press.moved && Math.hypot(e.clientX - press.x, e.clientY - press.y) < 4) return;
    press.moved = true;
    dragging = flow.screenToFlowPosition({ x: e.clientX, y: e.clientY }, { snapToGrid: false });
  }
  function up() {
    if (press?.moved && dragging) data.onmovebox(id, { x: Math.round(dragging.x), y: Math.round(dragging.y) });
    press = null;
    // Kept until the saved position arrives (or the next drag).
    setTimeout(() => (dragging = null), 1500);
  }

  const removed = $derived(data.change === 'removed');
  const tone = $derived(
    data.conflict ? 'conflict' : removed ? 'removed' : data.change === 'added' ? 'added' : data.change === 'changed' ? 'changed' : '',
  );
  const configured = $derived(data.mapped || data.filtered);
  const KIND = { memory: 'In-memory queue', spill: 'Queue spills to disk when memory is short', durable: 'Durable queue (on disk until delivered)' };
</script>

<BaseEdge path={geometry.full} {markerEnd} class={`flow-edge ${tone} ${selected ? 'selected' : ''}`} interactionWidth={16} />
<path bind:this={firstPath} d={geometry.first} class="measure" />
<path bind:this={fullPath} d={geometry.full} class="measure" />

{#if !reduceMotion}
  {#each motes as mote (`${mote.i}:${mote.bearing}:${data.flow?.tone}`)}
    {@const begin = `${mote.i * 0.25}s`}
    {@const after = mote.bearing === 0 ? 'ok' : mote.bearing !== null ? 'jam' : data.flow?.tone}
    <g class="mote" opacity="0">
      <!-- The colour after the box: the two circles swap there. A filtered
           event stays green; a dropped one turns orange. -->
      <circle r="3.5" class="pre">
        <animate attributeName="opacity" dur={DUR} {begin} repeatCount="indefinite" calcMode="discrete" values="1;0" keyTimes="0;0.375" />
      </circle>
      <circle r="3.5" class={`post ${after}`} opacity="0">
        <animate attributeName="opacity" dur={DUR} {begin} repeatCount="indefinite" calcMode="discrete" values="0;1" keyTimes="0;0.375" />
      </circle>
      {#if mote.bearing === null}
        <animateMotion path={geometry.full} dur={DUR} {begin} repeatCount="indefinite" calcMode="spline" keyTimes="0;0.375;0.75;1" keyPoints={`0;${atBox};1;1`} keySplines={SPLINES} />
        <animate attributeName="opacity" dur={DUR} {begin} repeatCount="indefinite" values="0;1;1;0;0" keyTimes="0;0.05;0.7;0.75;1" />
      {:else}
        <animateMotion path={exitPath(mote.bearing)} dur={DUR} {begin} repeatCount="indefinite" calcMode="spline" keyTimes="0;0.375;0.75;1" keyPoints={`0;${atBoxExit};1;1`} keySplines={SPLINES} />
        <animate attributeName="opacity" dur={DUR} {begin} repeatCount="indefinite" values="0;1;1;0;0" keyTimes="0;0.05;0.375;0.75;1" />
      {/if}
    </g>
  {/each}
{/if}

<EdgeLabel x={V.x} y={V.y} selectEdgeOnClick transparent>
  <div class="anchor">
    <div
      class="box nodrag nopan"
      class:removed
      class:selected
      class:dragging={dragging !== null}
      role="group"
      aria-label="Edge"
      title="Drag to move · double-click to place it automatically"
      onpointerdown={down}
      onpointermove={move}
      onpointerup={up}
      onpointercancel={up}
      ondblclick={() => data.onmovebox(id, null)}
    >
      {#if data.problems.length}
        <span class="icon danger" title={data.problems.join('\n')}><CircleAlert size={12} strokeWidth={2} /></span>
      {/if}
      <span class="config" class:on={configured} title={configured ? [data.filtered && 'Filters events', data.mapped && 'Maps events'].filter(Boolean).join(' · ') : 'Passes events through unchanged'}>
        {#if configured}
          {#if data.filtered}<Funnel size={12} strokeWidth={2} />{/if}
          {#if data.mapped}<ArrowRightLeft size={12} strokeWidth={2} />{/if}
        {:else}
          <span class="dot"></span>
        {/if}
      </span>
      {#if data.change === 'changed'}
        <span class="changed" title={`Changed in the draft: ${data.parts.join(', ')}`}></span>
      {/if}
      {#if removed}
        <button class="icon-btn" type="button" title="Restore edge" aria-label="Restore edge" onclick={() => data.onrestore(id)}>
          <RotateCcw size={12} strokeWidth={2} />
        </button>
      {:else}
        <button class="icon-btn" type="button" title="Edge settings" aria-label="Edge settings" onclick={() => data.onconfig(id)}>
          <Cog size={13} strokeWidth={2} />
        </button>
      {/if}
    </div>
    {#if data.queue && !removed}
      <span class="kind {data.queue.kind}" title={`${KIND[data.queue.kind]}; when full: ${data.queue.when_full.replace('_', ' ')} (the target node’s)`}>
        {#if data.queue.kind === 'durable'}<Database size={11} strokeWidth={2} />{:else if data.queue.kind === 'spill'}<HardDriveDownload size={11} strokeWidth={2} />{:else}<MemoryStick size={11} strokeWidth={2} />{/if}
      </span>
    {/if}
  </div>
</EdgeLabel>

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

  .measure {
    fill: none;
    stroke: none;
    pointer-events: none;
  }

  .mote {
    pointer-events: none;
  }

  .mote .pre {
    fill: var(--flow-ok);
  }

  .mote .post.ok {
    fill: var(--flow-ok);
  }

  .mote .post.spill {
    fill: var(--flow-spill);
  }

  .mote .post.jam {
    fill: var(--flow-jam);
  }

  /* The label sits centred on the box; the kind icon hangs below it. */
  .anchor {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
  }

  .box {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    height: 22px;
    padding: 0 2px 0 6px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--surface);
    box-shadow: var(--shadow-sm);
    cursor: grab;
    touch-action: none;
    user-select: none;
  }

  .box.selected {
    border-color: var(--accent);
  }

  .box.dragging {
    cursor: grabbing;
    box-shadow: var(--shadow);
  }

  .box.removed {
    border-style: dashed;
    opacity: 0.7;
  }

  .config {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    color: var(--text-faint);
  }

  .config.on {
    color: var(--accent);
  }

  .dot {
    width: 6px;
    height: 6px;
    border: 1.5px solid var(--text-faint);
    border-radius: 50%;
  }

  .changed {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
  }

  .icon {
    display: grid;
    place-items: center;
  }

  .icon.danger {
    color: var(--danger);
  }

  .icon-btn {
    display: grid;
    place-items: center;
    width: 18px;
    height: 18px;
    padding: 0;
    border: 0;
    border-radius: 4px;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
  }

  .icon-btn:hover {
    background: var(--surface-hover);
    color: var(--text);
  }

  .kind {
    position: absolute;
    top: 25px;
    display: grid;
    place-items: center;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--surface-2);
    color: var(--text-muted);
    pointer-events: auto;
  }

  .kind.spill {
    color: var(--flow-spill);
  }

  .kind.durable {
    color: var(--accent);
  }
</style>
