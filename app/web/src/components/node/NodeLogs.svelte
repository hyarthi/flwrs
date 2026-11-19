<script lang="ts">
  // A node's recent logs, live (`…/logs/stream`): the plugin's log and error
  // events, its stdout / stderr, and the server's notes about it. Filters by
  // level, source and text; follows new records unless scrolled up.
  import Download from '@lucide/svelte/icons/download';
  import Eraser from '@lucide/svelte/icons/eraser';
  import { openStream } from '../../lib/stream.svelte';

  interface LogRecord {
    seq: number;
    time: number;
    level: 'trace' | 'debug' | 'info' | 'warn' | 'error';
    source: 'plugin' | 'error' | 'stdout' | 'stderr' | 'supervisor';
    message: string;
    details?: Record<string, string>;
  }

  let { sceneId, nodeId }: { sceneId: string; nodeId: string } = $props();

  const MAX = 2000;
  let records = $state<LogRecord[]>([]);
  let missed = $state(0);
  let clearedAt = $state(0);

  $effect(() => {
    const stream = openStream(`/scenes/by-id/${encodeURIComponent(sceneId)}/nodes/by-id/${encodeURIComponent(nodeId)}/logs/stream`, {
      logs: (e: { records: LogRecord[] }) => (records = e.records),
      log: (r: LogRecord) => {
        records = records.length >= MAX ? [...records.slice(records.length - MAX + 1), r] : [...records, r];
      },
      lagged: (e: { missed: number }) => (missed += e.missed),
    });
    return () => stream.close();
  });

  // ---- filters ----

  const LEVELS = ['trace', 'debug', 'info', 'warn', 'error'] as const;
  let minLevel = $state<(typeof LEVELS)[number]>('debug');
  let source = $state<'all' | 'plugin' | 'output' | 'server'>('all');
  let text = $state('');
  const shown = $derived(
    records.filter((r) => {
      if (r.seq <= clearedAt) return false;
      if (LEVELS.indexOf(r.level) < LEVELS.indexOf(minLevel)) return false;
      if (source === 'plugin' && r.source !== 'plugin' && r.source !== 'error') return false;
      if (source === 'output' && r.source !== 'stdout' && r.source !== 'stderr') return false;
      if (source === 'server' && r.source !== 'supervisor') return false;
      const q = text.trim().toLowerCase();
      return !q || r.message.toLowerCase().includes(q) || Object.entries(r.details ?? {}).some(([k, v]) => `${k}=${v}`.toLowerCase().includes(q));
    }),
  );

  // ---- following ----

  let list: HTMLDivElement;
  let follow = $state(true);
  $effect(() => {
    void shown.length;
    if (follow && list) queueMicrotask(() => list.scrollTo({ top: list.scrollHeight }));
  });
  function scrolled() {
    follow = list.scrollHeight - list.scrollTop - list.clientHeight < 24;
  }

  const time = (ms: number) => new Date(ms).toLocaleTimeString(undefined, { hour12: false }) + `.${String(ms % 1000).padStart(3, '0')}`;
  const SOURCE = { plugin: 'plugin', error: 'error', stdout: 'stdout', stderr: 'stderr', supervisor: 'server' };
  const line = (r: LogRecord) =>
    `${new Date(r.time).toISOString()} ${r.level.toUpperCase()} [${SOURCE[r.source]}] ${r.message}${
      r.details && Object.keys(r.details).length ? ` {${Object.entries(r.details).map(([k, v]) => `${k}=${v}`).join(', ')}}` : ''
    }`;

  function download() {
    const blob = new Blob([shown.map(line).join('\n') + '\n'], { type: 'text/plain' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `node-${nodeId}.log`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }
</script>

<div class="logs">
  <div class="bar">
    <select class="input" bind:value={minLevel} aria-label="Lowest level shown">
      {#each LEVELS as l (l)}<option value={l}>{l} and up</option>{/each}
    </select>
    <select class="input" bind:value={source} aria-label="Source">
      <option value="all">Everything</option>
      <option value="plugin">The plugin’s logs</option>
      <option value="output">stdout / stderr</option>
      <option value="server">The server’s notes</option>
    </select>
    <input class="input grow" type="search" placeholder="Filter" bind:value={text} />
    <button class="btn ghost icon" type="button" title="Clear the view" aria-label="Clear the view" onclick={() => (clearedAt = records.at(-1)?.seq ?? 0)}>
      <Eraser size={16} strokeWidth={1.75} />
    </button>
    <button class="btn ghost icon" type="button" title="Download what’s shown" aria-label="Download" onclick={download}>
      <Download size={16} strokeWidth={1.75} />
    </button>
  </div>

  {#if missed}
    <p class="muted small">{missed} record(s) skipped: they came faster than shown.</p>
  {/if}

  <div class="list" bind:this={list} onscroll={scrolled} role="log" aria-live="polite">
    {#each shown as r (r.seq)}
      <div class="record {r.level}">
        <span class="time">{time(r.time)}</span>
        <span class="level">{r.level}</span>
        <span class="source">{SOURCE[r.source]}</span>
        <span class="message">
          {r.message}
          {#if r.details && Object.keys(r.details).length}
            <span class="details">{Object.entries(r.details).map(([k, v]) => `${k}=${v}`).join(' ')}</span>
          {/if}
        </span>
      </div>
    {:else}
      <p class="muted small empty">
        {records.length ? 'Nothing matches the filters.' : 'Nothing yet. Records appear while the node runs (the last 500 are kept; the server log has everything).'}
      </p>
    {/each}
  </div>
  {#if !follow}
    <button class="btn follow" type="button" onclick={() => { follow = true; list.scrollTo({ top: list.scrollHeight }); }}>Follow new records</button>
  {/if}
</div>

<style>
  .logs {
    flex: 1;
    min-height: 0;
    position: relative;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .bar {
    display: flex;
    gap: var(--space-2);
  }

  .bar select {
    width: auto;
  }

  .grow {
    flex: 1;
    min-width: 0;
  }

  /* Fills the tab (the node modal is a fixed height): the filters stay, the records scroll. */
  .list {
    flex: 1;
    min-height: 200px;
    padding: var(--space-1) 0;
    overflow: auto;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--surface-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }

  .record {
    display: grid;
    grid-template-columns: max-content 3.5em 4.5em 1fr;
    gap: var(--space-2);
    padding: 1px var(--space-2);
    line-height: 1.5;
  }

  .record:hover {
    background: var(--surface-hover);
  }

  .time,
  .source {
    color: var(--text-faint);
  }

  .level {
    color: var(--text-muted);
    text-transform: uppercase;
  }

  .record.warn .level,
  .record.warn .message {
    color: var(--warn);
  }

  .record.error .level,
  .record.error .message {
    color: var(--danger);
  }

  .record.debug .message,
  .record.trace .message {
    color: var(--text-muted);
  }

  .message {
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }

  .details {
    margin-left: var(--space-2);
    color: var(--text-faint);
  }

  .small {
    font-size: var(--text-sm);
  }

  .empty {
    padding: var(--space-4);
    font-family: var(--font-sans);
    text-align: center;
  }

  .follow {
    position: absolute;
    right: var(--space-3);
    bottom: var(--space-3);
    box-shadow: var(--shadow);
  }
</style>
