<script lang="ts">
  // The scene's Stats tab: per node, live from the scene's `status` events:
  // state, event rates and counters, its inbound queue, delivery and the
  // link to the hub. Purging (a node's queue, or every queue) behind a
  // confirm.
  import Trash from '@lucide/svelte/icons/trash-2';
  import ConfirmDialog from '../ConfirmDialog.svelte';
  import { formatRate } from '../../lib/scenes';
  import { purgeSpool, type NodeReport, type SceneStatusEvent, type SceneView } from '../../lib/sceneView';
  import { formatBytes } from '../node/uploads';

  let { view, status }: { view: SceneView; status: SceneStatusEvent | null } = $props();

  const base = $derived(`/scenes/by-id/${encodeURIComponent(view.scene.id)}`);
  const names = $derived(new Map(view.nodes.map((n) => [n.id, n.name ?? n.plugin_id])));
  const types = $derived(new Map(view.nodes.map((n) => [n.id, n.type])));

  // Events per second from successive samples (a counter that went down: a restart).
  let previous = new Map<string, { time: number; emitted: number; received: number }>();
  let rates = $state(new Map<string, { emitted: number; received: number }>());
  $effect(() => {
    if (!status) return;
    const next = new Map<string, { emitted: number; received: number }>();
    const seen = new Map<string, { time: number; emitted: number; received: number }>();
    for (const r of status.nodes) {
      const now = { time: status.time, emitted: r.counters.emitted, received: r.counters.received };
      const before = previous.get(r.node_id);
      if (before && now.time > before.time) {
        const per = (a: number, b: number) => ((a >= b ? a - b : a) * 1000) / (now.time - before.time);
        next.set(r.node_id, { emitted: per(now.emitted, before.emitted), received: per(now.received, before.received) });
      }
      seen.set(r.node_id, now);
    }
    previous = seen;
    rates = next;
  });

  const reports = $derived(
    [...(status?.nodes ?? [])].sort((a, b) => {
      const order = { SOURCE: 0, TRANSFORM: 1, SINK: 2 } as Record<string, number>;
      return (order[types.get(a.node_id) ?? ''] ?? 3) - (order[types.get(b.node_id) ?? ''] ?? 3) || (names.get(a.node_id) ?? '').localeCompare(names.get(b.node_id) ?? '');
    }),
  );

  const n = (v: number) => v.toLocaleString();
  const tone = (state: string) =>
    state === 'RUNNING' ? 'ok' : state === 'FAILED' || state === 'UNHEALTHY' ? 'danger' : state === 'STOPPED' ? '' : 'warn';
  const dropped = (r: NodeReport) => r.counters.dropped;

  let purging = $state<{ nodeId?: string; label: string } | null>(null);
  let purged = $state<string | null>(null);
</script>

<div class="stats">
  <div class="head">
    <p class="muted small">
      {#if !status}
        Waiting for the scene’s status…
      {:else if status.state === 'STOPPED'}
        The scene isn’t running{view.scene.enabled ? ' yet' : ''}: counters appear while it runs (they restart with each node).
      {:else if status.state === 'FAILED'}
        The scene failed to start.
      {:else if status.state === 'WAITING'}
        Waiting for the pre-flight check{status.preflight_retry ? ` (check ${status.preflight_retry.attempt} of ${status.preflight_retry.max_attempts})` : ''}{status.nodes.length ? ': the nodes below keep running the previous graph meanwhile.' : '.'}
      {:else}
        Live: counters since each node started; rates between the last two samples.
      {/if}
    </p>
    <button class="btn" type="button" onclick={() => (purging = { label: 'every queue of this scene' })}>
      <Trash size={16} strokeWidth={1.75} />Purge all queues
    </button>
  </div>

  {#if purged}<div class="notice ok" role="status">{purged}</div>{/if}
  {#if status?.errors.length}
    <div class="notice {status.state === 'WAITING' ? 'warn' : 'danger'}"><ul>{#each status.errors as e, i (i)}<li>{e}</li>{/each}</ul></div>
  {/if}

  {#if reports.length}
    <div class="card table-card">
      <table class="table">
        <thead>
          <tr>
            <th>Node</th>
            <th>State</th>
            <th class="num">Out /s</th>
            <th class="num">In /s</th>
            <th class="num">Dropped</th>
            <th class="num">Queued</th>
            <th class="num">On disk</th>
            <th class="num">In flight</th>
            <th class="num">Redelivered</th>
            <th class="num">Dead letters</th>
            <th class="num">Mapping errors</th>
            <th class="num">Round trip</th>
            <th class="num">Restarts</th>
            <th><span class="sr-only">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each reports as r (r.node_id)}
            {@const q = r.queue}
            {@const rate = rates.get(r.node_id)}
            <tr>
              <td>
                <a href={`${base}/nodes/by-id/${encodeURIComponent(r.node_id)}`}>{names.get(r.node_id) ?? r.node_id}</a>
                <div class="mono muted">{r.plugin_id} {r.plugin_version}</div>
              </td>
              <td>
                <span class="badge {tone(r.status.state)}" title={r.status.last_error ?? undefined}>{r.status.state.toLowerCase()}</span>
              </td>
              <td class="num">{types.get(r.node_id) === 'SINK' ? '—' : rate ? formatRate(rate.emitted) : n(r.counters.emitted)}</td>
              <td class="num">{types.get(r.node_id) === 'SOURCE' ? '—' : rate ? formatRate(rate.received) : n(r.counters.received)}</td>
              <td class="num" class:warn={dropped(r) > 0}>{n(dropped(r))}</td>
              <td class="num">{q ? `${n(q.len)} · ${formatBytes(q.bytes)}` : '—'}</td>
              <td class="num" class:spill={q?.spilling}>{q && (q.spill || q.durable) ? `${n(q.disk_len)} · ${formatBytes(q.disk_bytes)}` : '—'}</td>
              <td class="num">{q ? n(q.in_flight) : '—'}</td>
              <td class="num">{q ? n(q.redelivered) : '—'}</td>
              <td class="num" class:warn={(q?.dead_lettered ?? 0) > 0}>{q ? n(q.dead_lettered) : '—'}</td>
              <td class="num" class:warn={r.mapping_errors > 0}>{n(r.mapping_errors)}</td>
              <td class="num">{r.link.heartbeats.last_rtt_ms === null ? '—' : `${r.link.heartbeats.last_rtt_ms.toFixed(1)} ms`}</td>
              <td class="num">{n(r.counters.restarts)}</td>
              <td class="actions">
                {#if q}
                  <button class="btn ghost icon" type="button" title="Purge this node’s queue" aria-label={`Purge ${names.get(r.node_id) ?? r.node_id}’s queue`} onclick={() => (purging = { nodeId: r.node_id, label: `${names.get(r.node_id) ?? r.node_id}’s queue` })}>
                    <Trash size={16} strokeWidth={1.75} />
                  </button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:else if status}
    <div class="card empty muted">No node reports yet.</div>
  {/if}
</div>

{#if purging}
  <ConfirmDialog
    title="Purge {purging.label}?"
    confirm="Purge"
    danger
    onconfirm={async () => {
      const result = await purgeSpool(view.scene.id, purging!.nodeId);
      purged = `Purged ${purging!.label}: ${result.purged_events.toLocaleString()} event(s) dropped.`;
    }}
    onclose={() => (purging = null)}
  >
    <p>Every event waiting in {purging.label} is dropped, including those handed to a plugin and not acknowledged yet, and spool files start over. This can’t be undone.</p>
  </ConfirmDialog>
{/if}

<style>
  .stats {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-height: 0;
    overflow: auto;
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .small {
    font-size: var(--text-sm);
  }

  .table-card {
    padding: 0;
    overflow-x: auto;
  }

  .num {
    text-align: right;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .warn {
    color: var(--warn);
  }

  .spill {
    color: var(--flow-spill);
  }

  .actions {
    text-align: right;
  }

  .empty {
    padding: var(--space-5);
    text-align: center;
  }

  ul {
    margin: 0;
    padding-left: var(--space-4);
  }
</style>
