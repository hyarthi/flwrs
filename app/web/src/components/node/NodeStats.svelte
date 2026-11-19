<script lang="ts">
  // A node's run-time report from the scene's `status` events: the plugin's
  // state, its counters (with rates from successive samples), its inbound
  // queue and its link to the hub.
  import type { NodeReport, SceneStatusEvent } from '../../lib/sceneView';
  import { formatRate } from '../../lib/scenes';
  import { formatBytes } from './uploads';

  let { status, nodeId, running }: { status: SceneStatusEvent | null; nodeId: string; running: boolean } = $props();

  const report = $derived<NodeReport | undefined>(status?.nodes.find((n) => n.node_id === nodeId));

  // Events per second from the previous sample (a counter that went down
  // means the node restarted).
  let previous: { time: number; emitted: number; received: number } | null = null;
  let rates = $state<{ emitted: number; received: number } | null>(null);
  $effect(() => {
    if (!report || !status) return;
    const now = { time: status.time, emitted: report.counters.emitted, received: report.counters.received };
    if (previous && now.time > previous.time) {
      const per = (a: number, b: number) => ((a >= b ? a - b : a) * 1000) / (now.time - previous!.time);
      rates = { emitted: per(now.emitted, previous.emitted), received: per(now.received, previous.received) };
    }
    previous = now;
  });

  const n = (v: number | undefined) => (v === undefined ? '—' : v.toLocaleString());
  const tone = (state: string) =>
    state === 'RUNNING' ? 'ok' : state === 'FAILED' || state === 'UNHEALTHY' ? 'danger' : state === 'STOPPED' ? '' : 'warn';
</script>

{#if !running}
  <p class="muted small">Not running: the scene is disabled, or this node is only in the draft. Stats appear once it runs.</p>
{:else if !report}
  <p class="muted small">Waiting for the scene’s status…</p>
{:else}
  <div class="stats">
    <section>
      <h3>Plugin</h3>
      <dl>
        <dt>State</dt><dd><span class="badge {tone(report.status.state)}">{report.status.state.toLowerCase()}</span></dd>
        <dt>Version</dt><dd class="mono">{report.plugin_id} {report.plugin_version}</dd>
        <dt>Launches</dt><dd>{n(report.status.launches)}{report.status.failed_launches ? ` (${report.status.failed_launches} failed in a row)` : ''}</dd>
        {#if report.status.pid}<dt>Process</dt><dd class="mono">pid {report.status.pid}</dd>{/if}
        {#if report.status.last_error}<dt>Last error</dt><dd class="error">{report.status.last_error}</dd>{/if}
      </dl>
    </section>

    <section>
      <h3>Events</h3>
      <dl>
        <dt>Emitted</dt><dd>{n(report.counters.emitted)} <span class="muted">{rates ? formatRate(rates.emitted) : ''}</span></dd>
        <dt>Received</dt><dd>{n(report.counters.received)} <span class="muted">{rates ? formatRate(rates.received) : ''}</span></dd>
        <dt>Dropped</dt><dd class:warn={report.counters.dropped > 0}>{n(report.counters.dropped)}</dd>
        <dt>Mapping errors</dt><dd class:warn={report.mapping_errors > 0}>{n(report.mapping_errors)}</dd>
        <dt>Over the hop limit</dt><dd>{n(report.dropped_hops)}</dd>
        <dt>Restarts</dt><dd>{n(report.counters.restarts)}</dd>
      </dl>
    </section>

    {#if report.queue}
      {@const q = report.queue}
      <section>
        <h3>Inbound queue {q.durable ? '(durable)' : q.spill ? '(spills to disk)' : ''}</h3>
        <dl>
          <dt>Waiting</dt><dd>{n(q.len)} · {formatBytes(q.bytes)}</dd>
          <dt>In flight</dt><dd>{n(q.in_flight)}</dd>
          {#if q.spill || q.durable}<dt>On disk</dt><dd>{n(q.disk_len)} · {formatBytes(q.disk_bytes)}{q.spilling ? ' · spilling' : ''}</dd>{/if}
          <dt>Delivered</dt><dd>{n(q.delivered)}</dd>
          <dt>Redelivered</dt><dd>{n(q.redelivered)} <span class="muted">({n(q.nacked)} refused)</span></dd>
          <dt>Dead letters</dt><dd class:warn={q.dead_lettered > 0}>{n(q.dead_lettered)}</dd>
          <dt>Dropped</dt>
          <dd>
            full {n(q.dropped_full)} · timeout {n(q.dropped_timeout)} · budget {n(q.dropped_budget)} · too large {n(q.dropped_oversized)} · evicted {n(q.evicted)}{q.dropped_disk_error ? ` · disk errors ${n(q.dropped_disk_error)}` : ''}
          </dd>
          <dt>Blocked</dt><dd>{n(q.blocked_ms)} ms</dd>
        </dl>
      </section>
    {/if}

    <section>
      <h3>Link</h3>
      <dl>
        <dt>Round trip</dt><dd>{report.link.heartbeats.last_rtt_ms === null ? '—' : `${report.link.heartbeats.last_rtt_ms.toFixed(1)} ms`}</dd>
        <dt>Heartbeats</dt><dd>{n(report.link.heartbeats.answered)} answered · {n(report.link.heartbeats.missed)} missed</dd>
        <dt>Connections</dt>
        <dd>
          {n(report.link.connections.accepted)} accepted · {n(report.link.connections.rejected)} rejected · {n(report.link.connections.replaced)} replaced
        </dd>
        <dt>Lost</dt>
        <dd>
          closed {n(report.link.connections.closed_by_plugin)} · timed out {n(report.link.connections.timed_out)} · I/O {n(report.link.connections.io_errors)} · protocol {n(report.link.connections.protocol_violations)}
        </dd>
      </dl>
    </section>
  </div>
{/if}

<style>
  .stats {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: var(--space-3);
  }

  section {
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  h3 {
    margin: 0 0 var(--space-2);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0;
    font-size: var(--text-sm);
  }

  dt {
    color: var(--text-muted);
  }

  dd {
    margin: 0;
    font-variant-numeric: tabular-nums;
  }

  .warn {
    color: var(--warn);
  }

  .error {
    color: var(--danger);
    overflow-wrap: anywhere;
  }

  .small {
    font-size: var(--text-sm);
  }
</style>
