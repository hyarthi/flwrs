<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import Upload from '@lucide/svelte/icons/upload';
  import Download from '@lucide/svelte/icons/download';
  import Trash from '@lucide/svelte/icons/trash-2';
  import Workflow from '@lucide/svelte/icons/workflow';
  import X from '@lucide/svelte/icons/x';
  import { SvelteMap } from 'svelte/reactivity';
  import Page from '../components/Page.svelte';
  import Switch from '../components/Switch.svelte';
  import ConfirmDialog from '../components/ConfirmDialog.svelte';
  import NewSceneDialog from '../components/scenes/NewSceneDialog.svelte';
  import ExportDialog from '../components/scenes/ExportDialog.svelte';
  import { ApiError } from '../lib/api';
  import { navigate } from '../lib/router.svelte';
  import { openStream } from '../lib/stream.svelte';
  import {
    IMPORT_ACCEPT,
    Rates,
    deleteScene,
    formatRate,
    importScene,
    setEnabled,
    type SceneRow,
    type SceneSummary,
    type StatusEvent,
  } from '../lib/scenes';

  // ---- live data ----

  let rows = $state<SceneRow[] | null>(null);
  let listError = $state<string | null>(null);
  const status = new SvelteMap<string, SceneSummary>();
  const rateTracker = new Rates();
  let rates = $state(new Map<string, number>());

  const stream = openStream('/scenes/stream', {
    scenes: (event: { scenes: SceneRow[] }) => {
      rows = event.scenes;
      listError = null;
    },
    status: (event: StatusEvent) => {
      status.clear();
      for (const s of event.scenes) status.set(s.scene_id, s);
      rateTracker.update(event);
      rates = new Map(rateTracker.current);
    },
    stream_error: (event: { error: string }) => {
      listError = event.error;
    },
  });
  $effect(() => () => stream.close());

  // ---- filter ----

  let filter = $state('');
  const shown = $derived(
    (rows ?? []).filter((r) => !filter.trim() || r.name.toLowerCase().includes(filter.trim().toLowerCase())),
  );

  // ---- notices ----

  interface Notice {
    kind: 'ok' | 'warn' | 'danger';
    title: string;
    details: string[];
  }
  let notice = $state<Notice | null>(null);

  function fail(title: string, e: unknown) {
    const error = e instanceof ApiError ? e : new ApiError(0, String(e));
    notice = { kind: 'danger', title: `${title}: ${error.message}`, details: error.details };
  }

  // ---- state shown per row ----

  type Shown = { label: string; tone: '' | 'ok' | 'warn' | 'danger'; detail?: string };

  function stateOf(row: SceneRow): Shown {
    const s = status.get(row.id);
    if (s?.state === 'FAILED') return { label: 'Failed', tone: 'danger', detail: `${s.errors} load error(s)` };
    if (!row.enabled) return s?.state === 'RUNNING' ? { label: 'Stopping', tone: 'warn' } : { label: 'Disabled', tone: '' };
    if (s?.state === 'RUNNING') {
      return s.running_nodes < s.nodes
        ? { label: `Running ${s.running_nodes}/${s.nodes}`, tone: 'warn', detail: 'Not every node is connected' }
        : { label: 'Running', tone: 'ok' };
    }
    return { label: 'Starting', tone: 'warn' };
  }

  // ---- actions ----

  let pending = new SvelteMap<string, boolean>();

  async function toggle(row: SceneRow, enabled: boolean) {
    pending.set(row.id, true);
    try {
      await setEnabled(row.id, enabled);
      row.enabled = enabled; // the stream confirms shortly
    } catch (e) {
      fail(`Couldn't ${enabled ? 'enable' : 'disable'} “${row.name}”`, e);
    } finally {
      pending.delete(row.id);
    }
  }

  let creating = $state(false);
  let exporting = $state<SceneRow | null>(null);
  let deleting = $state<SceneRow | null>(null);

  // Import: the button's file picker, or files dropped on the page.
  let picker: HTMLInputElement;
  let importing = $state(false);
  let dragging = $state(0);

  async function importFiles(files: FileList | File[] | null) {
    const list = [...(files ?? [])];
    if (!list.length) return;
    importing = true;
    try {
      for (const file of list) {
        const result = await importScene(file);
        const details = [...result.problems, ...result.warnings];
        notice = {
          kind: result.problems.length ? 'warn' : 'ok',
          title: result.problems.length
            ? `Imported “${result.scene.name}” (disabled): fix these before enabling it`
            : `Imported “${result.scene.name}” (disabled)`,
          details,
        };
      }
    } catch (e) {
      fail('Import failed', e);
    } finally {
      importing = false;
      if (picker) picker.value = '';
    }
  }

  const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes('Files') ?? false;
</script>

<div
  class="drop"
  role="region"
  aria-label="Scenes"
  ondragenter={(e) => {
    if (hasFiles(e)) {
      e.preventDefault();
      dragging++;
    }
  }}
  ondragover={(e) => {
    if (hasFiles(e)) e.preventDefault();
  }}
  ondragleave={(e) => {
    if (hasFiles(e)) dragging = Math.max(0, dragging - 1);
  }}
  ondrop={(e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragging = 0;
    importFiles(e.dataTransfer?.files ?? null);
  }}
>
  <Page>
    {#snippet actions()}
      <input class="input filter" type="search" placeholder="Filter by name" aria-label="Filter scenes by name" bind:value={filter} />
      <span class="live" class:on={stream.state === 'live'} title={stream.state === 'live' ? 'Live updates' : 'Reconnecting…'}>
        <span class="dot"></span>{stream.state === 'live' ? 'Live' : 'Reconnecting…'}
      </span>
      <button class="btn" type="button" onclick={() => picker.click()} disabled={importing}>
        <Upload size={16} strokeWidth={1.75} />{importing ? 'Importing…' : 'Import'}
      </button>
      <button class="btn primary" type="button" onclick={() => (creating = true)}>
        <Plus size={16} strokeWidth={1.75} />New scene
      </button>
      <input bind:this={picker} type="file" accept={IMPORT_ACCEPT} hidden onchange={(e) => importFiles(e.currentTarget.files)} />
    {/snippet}

    {#if notice}
      <div class="notice {notice.kind}" role="status">
        <div class="grow">
          {notice.title}
          {#if notice.details.length}
            <ul>
              {#each notice.details as detail, i (i)}<li>{detail}</li>{/each}
            </ul>
          {/if}
        </div>
        <button class="btn ghost icon" type="button" aria-label="Dismiss" onclick={() => (notice = null)}>
          <X size={16} strokeWidth={1.75} />
        </button>
      </div>
    {/if}
    {#if listError}
      <div class="notice danger" role="alert">{listError}</div>
    {/if}

    {#if rows === null}
      <div class="card empty muted">Loading scenes…</div>
    {:else if rows.length === 0}
      <div class="card empty">
        <Workflow size={32} strokeWidth={1.5} />
        <h2>No scenes yet</h2>
        <p class="muted">A scene is a workflow: plugins as nodes, events flowing along edges. Create one, or import a <code>.flwr</code> / <code>.far</code> file (drop it here).</p>
        <div class="row">
          <button class="btn" type="button" onclick={() => picker.click()}><Upload size={16} strokeWidth={1.75} />Import</button>
          <button class="btn primary" type="button" onclick={() => (creating = true)}><Plus size={16} strokeWidth={1.75} />New scene</button>
        </div>
      </div>
    {:else}
      <div class="card table-card">
        <table class="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>State</th>
              <th>Graph</th>
              <th class="num">Events</th>
              <th>Checks</th>
              <th>Enabled</th>
              <th><span class="sr-only">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {#each shown as row (row.id)}
              {@const state = stateOf(row)}
              <tr>
                <td>
                  <a class="name" href={`/scenes/by-id/${encodeURIComponent(row.id)}`}>{row.name}</a>
                  <div class="mono muted">{row.id}</div>
                </td>
                <td><span class="badge {state.tone}" title={state.detail}>{state.label}</span></td>
                <td class="muted">{row.nodes} {row.nodes === 1 ? 'node' : 'nodes'} · {row.edges} {row.edges === 1 ? 'edge' : 'edges'}</td>
                <td class="num">{status.get(row.id)?.state === 'RUNNING' ? formatRate(rates.get(row.id)) : '—'}</td>
                <td>
                  {#if row.problems.length}
                    <span class="badge danger" title={row.problems.join('\n')}>
                      {row.problems.length} {row.problems.length === 1 ? 'problem' : 'problems'}
                    </span>
                  {/if}
                  {#if row.warnings.length}
                    <span class="badge warn" title={row.warnings.join('\n')}>
                      {row.warnings.length} {row.warnings.length === 1 ? 'warning' : 'warnings'}
                    </span>
                  {/if}
                  {#if !row.problems.length && !row.warnings.length}
                    <span class="muted">OK</span>
                  {/if}
                </td>
                <td>
                  <Switch
                    checked={row.enabled}
                    label={row.enabled ? `Disable ${row.name}` : `Enable ${row.name}`}
                    disabled={!row.enabled && row.problems.length > 0}
                    busy={pending.has(row.id)}
                    onchange={(on) => toggle(row, on)}
                  />
                </td>
                <td class="actions">
                  <button class="btn ghost icon" type="button" title="Export" aria-label={`Export ${row.name}`} onclick={() => (exporting = row)}>
                    <Download size={16} strokeWidth={1.75} />
                  </button>
                  <button class="btn ghost icon danger" type="button" title="Delete" aria-label={`Delete ${row.name}`} onclick={() => (deleting = row)}>
                    <Trash size={16} strokeWidth={1.75} />
                  </button>
                </td>
              </tr>
            {:else}
              <tr><td colspan="7" class="muted">No scene matches “{filter}”.</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </Page>

  {#if dragging > 0}
    <div class="overlay" aria-hidden="true">
      <div class="card">
        <Upload size={28} strokeWidth={1.5} />
        Drop <code>.flwr</code> or <code>.far</code> files to import them
      </div>
    </div>
  {/if}
</div>

{#if creating}
  <NewSceneDialog
    onclose={() => (creating = false)}
    oncreated={(scene) => {
      creating = false;
      navigate(`/scenes/by-id/${encodeURIComponent(scene.id)}`);
    }}
  />
{/if}

{#if exporting}
  <ExportDialog id={exporting.id} name={exporting.name} onclose={() => (exporting = null)} />
{/if}

{#if deleting}
  {@const row = deleting}
  <ConfirmDialog
    title="Delete scene"
    confirm="Delete"
    danger
    onclose={() => (deleting = null)}
    onconfirm={async () => {
      await deleteScene(row.id);
    }}
  >
    <p>Delete <strong>{row.name}</strong>? Its nodes, edges, uploaded files and queued events go with it.</p>
    {#if row.enabled}
      <p class="muted">It's running: it stops first.</p>
    {/if}
  </ConfirmDialog>
{/if}

<style>
  .drop {
    position: relative;
    min-height: 100%;
  }

  .filter {
    width: 220px;
    margin-right: auto;
  }

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

  .grow {
    flex: 1;
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-7) var(--space-5);
    text-align: center;
    color: var(--text-muted);
  }

  .empty h2 {
    color: var(--text);
  }

  .empty p {
    max-width: 460px;
  }

  .row {
    display: flex;
    gap: var(--space-2);
  }

  .table-card {
    overflow-x: auto;
  }

  .table tbody tr:last-child td {
    border-bottom: 0;
  }

  .table td {
    vertical-align: middle;
    font-size: var(--text-sm);
  }

  .name {
    color: var(--text);
    font-weight: 500;
  }

  .num {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  td.actions {
    white-space: nowrap;
    text-align: right;
  }

  .badge + .badge {
    margin-left: var(--space-1);
  }

  .overlay {
    position: absolute;
    inset: var(--space-3);
    display: grid;
    place-items: center;
    border: 2px dashed var(--accent);
    border-radius: var(--radius-lg);
    background: color-mix(in srgb, var(--bg) 80%, transparent);
    pointer-events: none;
  }

  .overlay .card {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-4) var(--space-5);
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
  }
</style>
