<script lang="ts">
  // The server at a glance, live from `GET /api/status/stream`: scenes by
  // state and what needs attention, event rates, the hub's connections, and
  // the queue budget, spill memory and disk. Time charts come from the
  // samples this page collects while it's open (no server-side history).
  import AlertTriangle from '@lucide/svelte/icons/triangle-alert';
  import CircleCheck from '@lucide/svelte/icons/circle-check';
  import Page from '../components/Page.svelte';
  import TimeChart from '../components/overview/TimeChart.svelte';
  import { openStream } from '../lib/stream.svelte';
  import { formatRate, type SceneRow, type SceneSummary } from '../lib/scenes';
  import ZoomOut from '@lucide/svelte/icons/zoom-out';
  import { History, WINDOWS, type OverviewEvent, type Sample, type Usage, type WindowId } from '../lib/overview';
  import { formatBytes } from '../components/node/uploads';

  let rows = $state<SceneRow[] | null>(null);
  let overview = $state<OverviewEvent | null>(null);
  let samples = $state<Sample[]>([]);
  let streamError = $state<string | null>(null);
  const history = new History();

  const stream = openStream('/status/stream', {
    scenes: (event: { scenes: SceneRow[] }) => {
      rows = event.scenes;
      streamError = null;
    },
    overview: (event: OverviewEvent) => {
      overview = event;
      if (history.add(event)) samples = [...history.samples];
    },
    stream_error: (event: { error: string }) => (streamError = event.error),
  });
  $effect(() => () => stream.close());

  const byId = $derived(new Map<string, SceneSummary>((overview?.scenes ?? []).map((s) => [s.scene_id, s])));

  type Attention = { scene: SceneRow; reason: string; level: 'danger' | 'warn' };
  const summary = $derived.by(() => {
    const counts = { running: 0, failed: 0, notRunning: 0, disabled: 0 };
    const attention: Attention[] = [];
    for (const scene of rows ?? []) {
      const s = byId.get(scene.id);
      if (s?.state === 'FAILED') {
        counts.failed++;
        attention.push({ scene, level: 'danger', reason: `failed to start (${s.errors} error${s.errors === 1 ? '' : 's'})` });
      } else if (s?.state === 'RUNNING') {
        counts.running++;
        if (s.running_nodes < s.nodes) {
          attention.push({ scene, level: 'warn', reason: `${s.nodes - s.running_nodes} of ${s.nodes} nodes not connected` });
        }
      } else if (scene.enabled) {
        counts.notRunning++;
        attention.push({ scene, level: 'warn', reason: 'enabled, but not running' });
      } else {
        counts.disabled++;
      }
      if (scene.problems.length) {
        attention.push({ scene, level: scene.enabled ? 'danger' : 'warn', reason: `${scene.problems.length} problem${scene.problems.length === 1 ? '' : 's'}: ${scene.problems[0]}` });
      }
    }
    attention.sort((a, b) => (a.level === b.level ? a.scene.name.localeCompare(b.scene.name) : a.level === 'danger' ? -1 : 1));
    return { counts, attention };
  });

  const latest = $derived(samples[samples.length - 1] ?? null);

  // ---- the charts' time domain: a window ending now, or a zoom ----

  const WINDOW_KEY = 'flwrs.overview.window';
  function storedWindow(): WindowId {
    try {
      const id = localStorage.getItem(WINDOW_KEY);
      if (WINDOWS.some((w) => w.id === id)) return id as WindowId;
    } catch {
      // storage unavailable: the default
    }
    return '15m';
  }
  let windowId = $state<WindowId>(storedWindow());
  function pickWindow(id: WindowId) {
    windowId = id;
    zoom = null;
    try {
      localStorage.setItem(WINDOW_KEY, id);
    } catch {
      // not remembered
    }
  }
  const win = $derived(WINDOWS.find((w) => w.id === windowId) ?? WINDOWS[2]);
  /** A zoomed span (both charts), fixed while samples keep arriving. */
  let zoom = $state<{ from: number; to: number } | null>(null);
  const domain = $derived.by(() => {
    if (zoom) return zoom;
    const now = latest?.time ?? Date.now();
    return { from: now - win.ms, to: now };
  });
  const clock = (t: number) => new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  const chart = $derived({
    from: domain.from,
    to: domain.to,
    live: zoom === null,
    windowLabel: win.label,
    onzoom: (from: number, to: number) => (zoom = { from, to }),
    onreset: () => (zoom = null),
  });
  const pct = (u: Usage) => (u.limit_bytes > 0 ? Math.min(100, (100 * u.used_bytes) / u.limit_bytes) : 0);
  const formatPct = (v: number) => (v === 0 ? '0%' : v < 0.1 ? '<0.1%' : v < 10 ? `${v.toFixed(1)}%` : `${Math.round(v)}%`);
  const perSecond = (v: number) => formatRate(v);

  const RATES = [
    { key: 'produced', label: 'Produced', color: '--series-1' },
    { key: 'delivered', label: 'Delivered', color: '--series-2' },
    { key: 'dropped', label: 'Dropped', color: '--series-3' },
  ];
  const USE = [
    { key: 'queued', label: 'Queue budget', color: '--series-1' },
    { key: 'spillMemory', label: 'Spill memory', color: '--series-2' },
    { key: 'spillDisk', label: 'Spool disk', color: '--series-3' },
  ];
</script>

<Page>
  {#snippet actions()}
    <span class="live" class:on={stream.state === 'live'} title={stream.state === 'live' ? 'Live updates' : 'Reconnecting…'}>
      <span class="dot"></span>{stream.state === 'live' ? 'Live' : 'Reconnecting…'}
    </span>
  {/snippet}

  {#if streamError}<div class="notice danger" role="alert">{streamError}</div>{/if}

  <div class="tiles">
    <a class="card tile" href="/scenes">
      <span class="label">Scenes</span>
      <span class="hero">{rows?.length ?? '–'}</span>
      <span class="parts">
        <span><b>{summary.counts.running}</b> running</span>
        {#if summary.counts.failed}<span class="bad"><b>{summary.counts.failed}</b> failed</span>{/if}
        {#if summary.counts.notRunning}<span class="warn"><b>{summary.counts.notRunning}</b> not running</span>{/if}
        <span class="muted"><b>{summary.counts.disabled}</b> disabled</span>
      </span>
    </a>
    <div class="card tile">
      <span class="label">Events</span>
      <span class="hero">{latest ? perSecond(latest.produced) : '–'}</span>
      <span class="parts">
        <span>produced</span>
        {#if latest}<span class="muted">{perSecond(latest.delivered)} delivered · {perSecond(latest.dropped)} dropped</span>{/if}
      </span>
    </div>
    <div class="card tile">
      <span class="label">Hub</span>
      <span class="hero">{overview?.hub_connections.connected ?? '–'}</span>
      <span class="parts">
        <span>plugins connected</span>
        {#if overview}
          <span class="muted">{overview.hub_connections.pending} in handshake · <span class="mono">{overview.hub_address}</span></span>
          {#if overview.hub_connections.refused_busy || overview.hub_connections.failed_handshakes}
            <span class="warn" title="Since the server started">{overview.hub_connections.failed_handshakes} failed handshakes · {overview.hub_connections.refused_busy} refused (busy)</span>
          {/if}
        {/if}
      </span>
    </div>
  </div>

  {#if overview}
    <div class="card usage">
      {#each [{ label: 'Queue budget', hint: '[hub] max_inflight_bytes', u: overview.queued_memory }, { label: 'Spill memory', hint: '[director] memory_soft_inflight_bytes (soft)', u: overview.spill_memory }, { label: 'Spool disk', hint: '[director] spill_max_bytes', u: overview.spill_disk }] as m (m.label)}
        {@const p = pct(m.u)}
        <div class="meter" title={m.hint}>
          <div class="meter-head">
            <span>{m.label}</span>
            <span class="mono muted">{formatBytes(m.u.used_bytes)} of {formatBytes(m.u.limit_bytes)} · {formatPct(p)}</span>
          </div>
          <div class="bar" role="meter" aria-label={m.label} aria-valuemin="0" aria-valuemax="100" aria-valuenow={Math.round(p)}>
            <span class="fill" class:warn={p >= 75} class:bad={p >= 90} style:width={`${p}%`}></span>
          </div>
        </div>
      {/each}
    </div>
  {/if}

  <div class="chart-bar">
    <div class="seg" role="group" aria-label="Time window">
      {#each WINDOWS as w (w.id)}
        <button type="button" class="btn ghost small" class:active={w.id === windowId && !zoom} aria-pressed={w.id === windowId && !zoom} onclick={() => pickWindow(w.id)}>{w.id}</button>
      {/each}
    </div>
    {#if zoom}
      <span class="muted small mono">{clock(zoom.from)} – {clock(zoom.to)}</span>
      <button type="button" class="btn ghost small" onclick={() => (zoom = null)}><ZoomOut size={14} strokeWidth={1.75} />Reset zoom</button>
    {:else}
      <span class="muted small">Drag across a chart to zoom in (down to 30 s); samples are kept for an hour while this page is open.</span>
    {/if}
  </div>

  <div class="charts">
    <TimeChart title="Event rates" {samples} series={RATES} format={perSecond} {...chart} />
    <TimeChart title="Memory and disk (share of each limit)" {samples} series={USE} format={formatPct} cap={100} {...chart} />
  </div>

  <section class="card attention">
    <h2>Needs attention</h2>
    {#if rows === null}
      <p class="muted">Loading…</p>
    {:else if summary.attention.length === 0}
      <p class="ok"><CircleCheck size={16} strokeWidth={1.75} />Nothing: every enabled scene runs with all its nodes connected, and no scene has problems.</p>
    {:else}
      <ul>
        {#each summary.attention as a, i (i)}
          <li class={a.level}>
            <AlertTriangle size={14} strokeWidth={1.75} />
            <a href={`/scenes/by-id/${encodeURIComponent(a.scene.id)}`}>{a.scene.name}</a>
            <span class="muted">{a.reason}</span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>
</Page>

<style>
  .live {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .live .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--warn);
  }

  .live.on .dot {
    background: var(--ok);
  }

  .tiles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: var(--space-3);
  }

  .tile,
  .usage,
  .attention {
    padding: var(--space-4);
  }

  .tile {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    color: var(--text);
  }

  a.tile:hover {
    border-color: var(--border-strong);
  }

  .label {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }

  .hero {
    font-size: var(--text-2xl);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .parts {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1) var(--space-3);
    font-size: var(--text-sm);
  }

  .bad {
    color: var(--danger);
  }

  .warn {
    color: var(--warn);
  }

  .usage {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: var(--space-4);
  }

  .meter {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--text-sm);
  }

  .meter-head {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
  }

  .bar {
    height: 8px;
    border-radius: 4px;
    background: var(--surface-2);
    overflow: hidden;
  }

  .fill {
    display: block;
    height: 100%;
    min-width: 2px;
    border-radius: 4px;
    background: var(--accent);
  }

  .fill.warn {
    background: var(--warn);
  }

  .fill.bad {
    background: var(--danger);
  }

  .chart-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
  }

  .seg {
    display: flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--surface);
  }

  .seg .active {
    background: var(--accent-soft);
    color: var(--accent);
  }

  .small {
    font-size: var(--text-sm);
  }

  .charts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
    gap: var(--space-3);
  }

  .attention h2 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-lg);
  }

  .attention ul {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .attention li {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-md);
  }

  .attention li.danger :global(svg) {
    color: var(--danger);
  }

  .attention li.warn :global(svg) {
    color: var(--warn);
  }

  .ok {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    color: var(--text-muted);
  }

  .ok :global(svg) {
    color: var(--ok);
  }

  @media (max-width: 520px) {
    .charts {
      grid-template-columns: 1fr;
    }
  }
</style>
