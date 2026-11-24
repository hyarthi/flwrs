<script lang="ts" module>
  import type { Change, Conflict, NodeState, NodeType } from '../../lib/sceneView';

  /** What a node on the canvas shows (`data` of a Svelte Flow node). */
  export interface FlowNodeData extends Record<string, unknown> {
    title: string;
    subtitle: string;
    nodeType: NodeType;
    pluginId: string;
    color: string | null;
    icon: string | null;
    change: Change | null;
    conflict: Conflict | null;
    parts: string[];
    state: NodeState | null;
    /** The glow of an active scene's node: connected, reconnecting / starting, down. `null` = none. */
    health: 'ok' | 'warn' | 'bad' | null;
    problems: string[];
    warnings: string[];
    /** The groups it's in, of every family. */
    chips: { id: string; color: number; family: string }[];
    /** The devices it claims. */
    devices: { kind: string; device: string; clash: boolean }[];
    onopen: (id: string) => void;
    onrestore: (id: string) => void;
  }
</script>

<script lang="ts">
  import { Handle, Position as Side } from '@xyflow/svelte';
  import Zap from '@lucide/svelte/icons/zap';
  import Shuffle from '@lucide/svelte/icons/shuffle';
  import Inbox from '@lucide/svelte/icons/inbox';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';
  import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import Camera from '@lucide/svelte/icons/camera';
  import Cpu from '@lucide/svelte/icons/cpu';
  import { useGroupFocus } from '../../lib/groupFocus.svelte';

  let { id, data, selected }: { id: string; data: FlowNodeData; selected?: boolean } = $props();

  const removed = $derived(data.change === 'removed');
  const TypeIcon = $derived(data.nodeType === 'SOURCE' ? Zap : data.nodeType === 'SINK' ? Inbox : Shuffle);
  let iconFailed = $state(false);

  const focus = useGroupFocus();
  const inFocus = $derived(!!focus?.id && data.chips.some((c) => c.id === focus.id));
  const dimmed = $derived(!!focus?.id && !inFocus);

  const stateTone = $derived.by(() => {
    switch (data.state) {
      case 'RUNNING':
        return 'ok';
      case 'FAILED':
      case 'UNHEALTHY':
        return 'danger';
      case 'LAUNCHING':
      case 'WAITING':
      case 'DISCONNECTED':
      case 'STOPPING':
        return 'warn';
      default:
        return '';
    }
  });

  const changeLabel = $derived.by(() => {
    if (data.conflict) return 'Conflict with live: resolve it in Review';
    switch (data.change) {
      case 'added':
        return 'New in the draft';
      case 'changed':
        return `Changed in the draft${data.parts.length ? `: ${data.parts.join(', ')}` : ''}`;
      case 'removed':
        return 'Removed in the draft';
      default:
        return null;
    }
  });
</script>

<div
  class="node"
  class:selected
  class:added={data.change === 'added'}
  class:changed={data.change === 'changed'}
  class:removed
  class:conflict={data.conflict}
  class:in-focus={inFocus}
  class:dimmed
  class:glow-ok={data.health === 'ok'}
  class:glow-warn={data.health === 'warn'}
  class:glow-bad={data.health === 'bad'}
  style:--node-color={data.color ?? undefined}
  role="button"
  tabindex="-1"
  title={changeLabel ?? undefined}
  ondblclick={() => !removed && data.onopen(id)}
>
  {#if data.nodeType !== 'SOURCE'}
    <Handle type="target" position={Side.Left} isConnectable={!removed} />
  {/if}

  <span class="icon" aria-hidden="true">
    {#if data.icon && !iconFailed}
      <img src={data.icon} alt="" width="18" height="18" onerror={() => (iconFailed = true)} />
    {:else}
      <TypeIcon size={16} strokeWidth={1.75} />
    {/if}
  </span>

  <span class="text">
    <span class="title">{data.title}</span>
    <span class="subtitle">{data.subtitle}</span>
  </span>

  <span class="marks">
    {#if data.problems.length}
      <span class="mark danger" title={data.problems.join('\n')}><CircleAlert size={14} strokeWidth={2} /></span>
    {:else if data.warnings.length}
      <span class="mark warn" title={data.warnings.join('\n')}><TriangleAlert size={14} strokeWidth={2} /></span>
    {/if}
    {#if data.state && !removed}
      <span class="state {stateTone}" title={data.state.toLowerCase()}></span>
    {/if}
  </span>

  {#if data.chips.length || data.devices.length}
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
      {#each data.devices as d (`${d.kind}:${d.device}`)}
        <span class="device" class:clash={d.clash} title={d.clash ? `${d.kind} ${d.device}: claimed by another node too` : `Claims ${d.kind} ${d.device}`}>
          {#if d.kind === 'camera'}<Camera size={11} strokeWidth={2} />{:else}<Cpu size={11} strokeWidth={2} />{/if}
          {d.device}
        </span>
      {/each}
    </span>
  {/if}

  {#if data.conflict}
    <span class="tag conflict-tag">conflict</span>
  {:else if data.change === 'added'}
    <span class="tag">new</span>
  {:else if data.change === 'changed'}
    <span class="dot" aria-label="Changed"></span>
  {/if}

  {#if removed}
    <button class="restore nodrag" type="button" title="Restore" aria-label="Restore node" onclick={() => data.onrestore(id)}>
      <RotateCcw size={14} strokeWidth={2} />
    </button>
  {/if}

  {#if data.nodeType !== 'SINK'}
    <Handle type="source" position={Side.Right} isConnectable={!removed} />
  {/if}
</div>

<style>
  .node {
    --node-color: var(--accent);
    position: relative;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    width: 220px;
    min-height: 48px;
    padding: var(--space-2) var(--space-3) var(--space-2) calc(var(--space-3) + 4px);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--text);
    box-shadow: var(--shadow-sm);
    font-size: var(--text-sm);
    overflow: visible;
    transition:
      border-color var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease),
      opacity var(--dur) var(--ease);
  }

  /* The plugin's colour as a stripe on the left. */
  .node::before {
    content: '';
    position: absolute;
    inset: -1px auto -1px -1px;
    width: 4px;
    border-radius: var(--radius) 0 0 var(--radius);
    background: var(--node-color);
  }

  .node.glow-ok {
    box-shadow: 0 0 14px 3px var(--glow-ok);
  }

  .node.glow-warn {
    box-shadow: 0 0 14px 3px var(--glow-warn);
  }

  .node.glow-bad {
    box-shadow: 0 0 14px 3px var(--glow-bad);
  }

  .node.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--focus-ring);
  }

  .node.selected.glow-ok {
    box-shadow: 0 0 0 2px var(--focus-ring), 0 0 14px 3px var(--glow-ok);
  }

  .node.selected.glow-warn {
    box-shadow: 0 0 0 2px var(--focus-ring), 0 0 14px 3px var(--glow-warn);
  }

  .node.selected.glow-bad {
    box-shadow: 0 0 0 2px var(--focus-ring), 0 0 14px 3px var(--glow-bad);
  }

  .node.added {
    border-color: var(--accent);
    border-width: 1.5px;
  }

  .node.conflict {
    border-color: var(--warn);
    border-width: 1.5px;
  }

  .node.removed {
    border-style: dashed;
    box-shadow: none;
    background: var(--surface-2);
  }

  /* A shadow: faded, but its restore button isn't. */
  .node.removed > :not(.restore),
  .node.removed::before {
    opacity: 0.45;
  }

  .node.dimmed {
    opacity: 0.25;
  }

  .node.in-focus {
    border-color: var(--text);
  }

  .chips {
    display: flex;
    flex: 0 0 100%;
    flex-wrap: wrap;
    gap: var(--space-1);
    min-width: 0;
  }

  .chip,
  .device {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    max-width: 100%;
    padding: 0 5px;
    overflow: hidden;
    border-radius: var(--radius-sm);
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 16px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .chip {
    border: 1px solid var(--chip-color);
    background: color-mix(in srgb, var(--chip-color) 14%, transparent);
    color: var(--text);
    cursor: inherit;
  }

  .device {
    border: 1px solid var(--border-strong);
    background: var(--surface-2);
    color: var(--text-muted);
  }

  .device.clash {
    border-color: var(--danger);
    background: var(--danger-soft);
    color: var(--danger);
  }

  .icon {
    display: grid;
    place-items: center;
    flex: none;
    width: 28px;
    height: 28px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--node-color);
  }

  .icon img {
    display: block;
    object-fit: contain;
  }

  .text {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    line-height: 1.3;
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
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }

  .marks {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    flex: none;
  }

  .mark {
    display: grid;
    place-items: center;
  }

  .mark.danger {
    color: var(--danger);
  }

  .mark.warn {
    color: var(--warn);
  }

  .state {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-faint);
  }

  .state.ok {
    background: var(--ok);
  }

  .state.warn {
    background: var(--warn);
  }

  .state.danger {
    background: var(--danger);
  }

  .tag {
    position: absolute;
    top: -9px;
    right: 10px;
    padding: 0 6px;
    border-radius: 999px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 10px;
    font-weight: 600;
    line-height: 16px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .tag.conflict-tag {
    background: var(--warn);
  }

  .dot {
    position: absolute;
    top: -4px;
    right: -4px;
    width: 10px;
    height: 10px;
    border: 2px solid var(--surface);
    border-radius: 50%;
    background: var(--accent);
  }

  .restore {
    position: absolute;
    top: -10px;
    right: -10px;
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    padding: 0;
    border: 1px solid var(--border-strong);
    border-radius: 50%;
    background: var(--surface);
    color: var(--text);
    cursor: pointer;
  }

  .restore:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  /* Handles: our tokens instead of the library's colours. */
  .node :global(.svelte-flow__handle) {
    width: 10px;
    height: 10px;
    border: 2px solid var(--surface);
    background: var(--border-strong);
  }

  .node :global(.svelte-flow__handle:hover),
  .node :global(.svelte-flow__handle.connectingto) {
    background: var(--accent);
  }
</style>
