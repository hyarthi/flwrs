<script lang="ts">
  // The installed plugins: icon, name, version, type, platforms, what uses
  // them. Upload (the button, or a package dropped on the page), delete
  // (refused while live nodes use it), and new remote plugin definitions.
  import Plus from '@lucide/svelte/icons/plus';
  import Upload from '@lucide/svelte/icons/upload';
  import Trash from '@lucide/svelte/icons/trash-2';
  import Download from '@lucide/svelte/icons/download';
  import Puzzle from '@lucide/svelte/icons/puzzle';
  import X from '@lucide/svelte/icons/x';
  import Zap from '@lucide/svelte/icons/zap';
  import Shuffle from '@lucide/svelte/icons/shuffle';
  import Inbox from '@lucide/svelte/icons/inbox';
  import { SvelteSet } from 'svelte/reactivity';
  import Page from '../components/Page.svelte';
  import ConfirmDialog from '../components/ConfirmDialog.svelte';
  import TokenPrompt from '../components/plugins/TokenPrompt.svelte';
  import { ApiError } from '../lib/api';
  import { pluginIconUrl } from '../lib/sceneView';
  import {
    PACKAGE_ACCEPT,
    deletePlugin,
    exportUrl,
    getPluginSettings,
    getUsage,
    listPlugins,
    uploadPlugin,
    type PluginInfo,
    type PluginSettings,
    type PluginUse,
  } from '../lib/plugins';

  let plugins = $state<PluginInfo[] | null>(null);
  let usage = $state<PluginUse[]>([]);
  let settings = $state<PluginSettings | null>(null);
  let loadError = $state<string | null>(null);

  async function load() {
    try {
      [plugins, usage, settings] = await Promise.all([listPlugins(), getUsage(), getPluginSettings()]);
      loadError = null;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }
  load();

  const usedBy = $derived.by(() => {
    const map = new Map<string, { live: number; draft: number; scenes: Set<string> }>();
    for (const u of usage) {
      const entry = map.get(u.plugin_id) ?? { live: 0, draft: 0, scenes: new Set<string>() };
      if (u.draft_only) entry.draft++;
      else entry.live++;
      entry.scenes.add(u.scene_name);
      map.set(u.plugin_id, entry);
    }
    return map;
  });

  let filter = $state('');
  const shown = $derived(
    (plugins ?? []).filter((p) => {
      const q = filter.trim().toLowerCase();
      return !q || p.name.toLowerCase().includes(q) || p.id.toLowerCase().includes(q) || (p.ui?.categories ?? []).some((c) => c.toLowerCase().includes(q));
    }),
  );

  // ---- notices ----

  // Stacked, newest last: an upload of several packages leaves one per file.
  interface Notice {
    id: number;
    kind: 'ok' | 'danger';
    title: string;
    details: string[];
  }
  let notices = $state<Notice[]>([]);
  let nextNotice = 0;
  function notify(kind: Notice['kind'], title: string, details: string[] = []) {
    notices.push({ id: nextNotice++, kind, title, details });
  }
  const front = $derived(notices.at(-1));
  const behind = $derived(notices.slice(-3, -1));
  function dismiss(id: number) {
    notices = notices.filter((n) => n.id !== id);
  }
  function fail(title: string, e: unknown) {
    const error = e instanceof ApiError ? e : new ApiError(0, String(e));
    notify('danger', `${title}: ${error.message}`, error.details);
  }

  // ---- upload ----

  let picker: HTMLInputElement;
  let uploading = $state(false);
  let dragging = $state(0);

  async function upload(files: FileList | File[] | null) {
    const list = [...(files ?? [])];
    if (!list.length) return;
    uploading = true;
    try {
      // Each file on its own: one refused package doesn't stop the others.
      for (const file of list) {
        try {
          const plugin = await uploadPlugin(file);
          notify('ok', `Installed ${plugin.name} ${plugin.version}`);
        } catch (e) {
          fail(`${file.name}: upload failed`, e);
        }
      }
      await load();
    } finally {
      uploading = false;
      if (picker) picker.value = '';
    }
  }
  const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes('Files') ?? false;

  // ---- delete ----

  let deleting = $state<PluginInfo | null>(null);
  const failedIcons = new SvelteSet<string>();
  const TYPE_ICON = { SOURCE: Zap, TRANSFORM: Shuffle, SINK: Inbox };
</script>

<TokenPrompt />

<div
  class="drop"
  role="region"
  aria-label="Plugins"
  ondragenter={(e) => {
    if (hasFiles(e) && settings?.allow_upload) {
      e.preventDefault();
      dragging++;
    }
  }}
  ondragover={(e) => {
    if (hasFiles(e) && settings?.allow_upload) e.preventDefault();
  }}
  ondragleave={(e) => {
    if (hasFiles(e)) dragging = Math.max(0, dragging - 1);
  }}
  ondrop={(e) => {
    if (!hasFiles(e) || !settings?.allow_upload) return;
    e.preventDefault();
    dragging = 0;
    upload(e.dataTransfer?.files ?? null);
  }}
>
  <Page>
    {#snippet actions()}
      <input class="input filter" type="search" placeholder="Filter" aria-label="Filter plugins" bind:value={filter} />
      <a class="btn" href="/plugins/new-remote"><Plus size={16} strokeWidth={1.75} />New remote plugin</a>
      <button
        class="btn primary"
        type="button"
        disabled={uploading || !settings?.allow_upload}
        title={settings && !settings.allow_upload ? 'Uploads are off on this server ([plugins] allow_upload)' : 'Upload a plugin package (.zip)'}
        onclick={() => picker.click()}
      >
        <Upload size={16} strokeWidth={1.75} />{uploading ? 'Uploading…' : 'Upload'}
      </button>
      <input bind:this={picker} type="file" accept={PACKAGE_ACCEPT} multiple hidden onchange={(e) => upload(e.currentTarget.files)} />
    {/snippet}

    {#if front}
      <!-- A stack of cards: the newest in front, up to two older ones peeking
           out above it (just their edges); dismissing one shows the next. -->
      <div class="stack" style:--behind={behind.length}>
        {#each behind as n, i (n.id)}
          <div class="layer notice {n.kind}" style:--depth={behind.length - i} aria-hidden="true"></div>
        {/each}
        <div class="front notice {front.kind}" role="status">
          <div class="grow">
            {front.title}
            {#if front.details.length}<ul>{#each front.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}
          </div>
          {#if notices.length > 1}
            <button class="btn ghost" type="button" onclick={() => (notices = [])}>Dismiss all ({notices.length})</button>
          {/if}
          <button class="btn ghost icon" type="button" aria-label="Dismiss" onclick={() => dismiss(front.id)}><X size={16} strokeWidth={2} /></button>
        </div>
      </div>
    {/if}
    {#if loadError}<div class="notice danger" role="alert">{loadError}</div>{/if}
    {#if settings && !settings.allow_upload}
      <p class="muted small">Plugin uploads are off on this server: set <code>[plugins] allow_upload = true</code> to install, upgrade and delete packages here. Remote plugin definitions run nothing here, so the wizard can still add them.</p>
    {/if}

    {#if plugins === null}
      <div class="card empty muted">Loading plugins…</div>
    {:else if plugins.length === 0}
      <div class="card empty">
        <Puzzle size={32} strokeWidth={1.5} />
        <h2>No plugins yet</h2>
        <p class="muted">Plugins are what nodes run: sources, transforms and sinks. Upload a package (a <code>.zip</code> made with <code>flwrs-trellis package</code>), or drop it here.</p>
      </div>
    {:else}
      <div class="card table-card">
        <table class="table">
          <thead>
            <tr><th>Plugin</th><th>Version</th><th>Type</th><th>Platforms</th><th>Used by</th><th><span class="sr-only">Actions</span></th></tr>
          </thead>
          <tbody>
            {#each shown as p (p.id)}
              {@const use = usedBy.get(p.id)}
              {@const Icon = TYPE_ICON[p.type]}
              <tr>
                <td>
                  <a class="name" href={`/plugins/by-id/${encodeURIComponent(p.id)}`} style:--plugin-color={p.ui?.color ?? undefined}>
                    <span class="icon">
                      {#if p.ui?.icon && !failedIcons.has(p.id)}
                        <img src={pluginIconUrl(p.id)} alt="" width="20" height="20" onerror={() => failedIcons.add(p.id)} />
                      {:else}
                        <Icon size={16} strokeWidth={1.75} />
                      {/if}
                    </span>
                    <span>
                      {p.name}
                      <span class="mono muted block">{p.id}</span>
                    </span>
                  </a>
                </td>
                <td class="mono">{p.version}</td>
                <td><span class="badge">{p.type.toLowerCase()}</span>{#if p.remote_only}<span class="badge accent" title="A remote plugin definition: it runs elsewhere">remote</span>{/if}{#if !p.installed}<span class="badge danger" title="Its files are missing on the server">files missing</span>{/if}</td>
                <td class="muted small">{p.remote_only ? 'elsewhere' : (p.platforms?.join(', ') ?? 'any')}</td>
                <td class="muted small" title={use ? [...use.scenes].join('\n') : undefined}>
                  {#if use}
                    {use.live ? `${use.live} node${use.live === 1 ? '' : 's'}` : ''}{use.live && use.draft ? ' + ' : ''}{use.draft ? `${use.draft} in drafts` : ''}
                    · {use.scenes.size} scene{use.scenes.size === 1 ? '' : 's'}
                  {:else}
                    —
                  {/if}
                </td>
                <td class="actions">
                  {#if p.remote_only}
                    <a class="btn ghost icon" href={exportUrl(p.id)} download title="Export as a package (.zip)" aria-label={`Export ${p.name}`}><Download size={16} strokeWidth={1.75} /></a>
                  {/if}
                  <button
                    class="btn ghost icon danger"
                    type="button"
                    disabled={(!settings?.allow_upload && !p.remote_only) || (use?.live ?? 0) > 0}
                    title={(use?.live ?? 0) > 0 ? 'In use: remove its nodes first' : 'Delete'}
                    aria-label={`Delete ${p.name}`}
                    onclick={() => (deleting = p)}
                  >
                    <Trash size={16} strokeWidth={1.75} />
                  </button>
                </td>
              </tr>
            {:else}
              <tr><td colspan="6" class="muted">No plugin matches “{filter}”.</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </Page>

  {#if dragging > 0}
    <div class="overlay" aria-hidden="true">
      <div class="card"><Upload size={28} strokeWidth={1.5} />Drop plugin packages (<code>.zip</code>) to install them</div>
    </div>
  {/if}
</div>

{#if deleting}
  <ConfirmDialog
    title={`Delete ${deleting.name}?`}
    confirm="Delete"
    danger
    onconfirm={async () => {
      await deletePlugin(deleting!.id);
      notify('ok', `Deleted ${deleting!.name}`);
      await load();
    }}
    onclose={() => (deleting = null)}
  >
    <p>{deleting.remote_only ? 'The definition is' : 'The package and its files are'} removed from the server. Nodes in drafts that use it will show it as missing.</p>
  </ConfirmDialog>
{/if}

<style>
  .drop {
    position: relative;
    min-height: 100%;
  }

  .filter {
    width: 220px;
  }

  .grow {
    flex: 1;
  }

  /* Each card behind the front one sits PEEK higher and a little narrower. */
  .stack {
    --peek: 8px;
    position: relative;
    padding-top: calc(var(--behind) * var(--peek));
  }

  .stack .notice {
    border-color: var(--border);
  }

  .layer {
    position: absolute;
    inset: calc(var(--behind) * var(--peek)) 0 0;
    padding: 0;
    box-shadow: var(--shadow-sm);
    transform: translateY(calc(var(--depth) * var(--peek) * -1)) scale(calc(1 - var(--depth) * 0.03));
    transform-origin: top center;
    transition: transform var(--dur) var(--ease);
  }

  .front {
    position: relative;
    box-shadow: var(--shadow);
  }

  .small {
    font-size: var(--text-sm);
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-6);
    text-align: center;
  }

  .table-card {
    padding: 0;
    overflow-x: auto;
  }

  .name {
    --plugin-color: var(--accent);
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text);
    font-weight: 500;
  }

  .icon {
    display: grid;
    place-items: center;
    flex: none;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--plugin-color);
  }

  .block {
    display: block;
    font-weight: 400;
  }

  .actions {
    text-align: right;
  }

  .overlay {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    background: light-dark(#ffffffcc, #0e1014cc);
  }

  .overlay .card {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-5);
    border: 2px dashed var(--accent);
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
