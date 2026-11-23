<script lang="ts">
  // One plugin: its manifest at a glance, its docs (Markdown from the
  // package, sanitized), the settings its nodes take (from the launch-args
  // schema), its payload schemas, and the nodes using it. Upgrade = a new
  // package for the same id; delete while no live node uses it.
  import ArrowLeft from '@lucide/svelte/icons/arrow-left';
  import Upload from '@lucide/svelte/icons/upload';
  import Trash from '@lucide/svelte/icons/trash-2';
  import Pencil from '@lucide/svelte/icons/pencil';
  import Download from '@lucide/svelte/icons/download';
  import X from '@lucide/svelte/icons/x';
  import Zap from '@lucide/svelte/icons/zap';
  import Shuffle from '@lucide/svelte/icons/shuffle';
  import Inbox from '@lucide/svelte/icons/inbox';
  import Page from '../components/Page.svelte';
  import Tabs from '../components/Tabs.svelte';
  import ConfirmDialog from '../components/ConfirmDialog.svelte';
  import TokenPrompt from '../components/plugins/TokenPrompt.svelte';
  import SchemaTree from '../components/mapping/SchemaTree.svelte';
  import { ApiError } from '../lib/api';
  import { navigate } from '../lib/router.svelte';
  import { pluginIconUrl, type SchemaField } from '../lib/sceneView';
  import { constraintHint, kindOf, labelOf, resolve, topFields, type Schema } from '../lib/schemaForm';
  import { formatBytes } from '../components/node/uploads';
  import {
    PACKAGE_ACCEPT,
    deletePlugin,
    getDocs,
    getLaunchArgsSchema,
    getNodeSchemas,
    exportUrl,
    getPluginInfo,
    getPluginSettings,
    getUsage,
    upgradePlugin,
    type PluginInfo,
    type PluginSettings,
    type PluginUse,
  } from '../lib/plugins';

  let { id }: { id: string } = $props();

  let plugin = $state<PluginInfo | null>(null);
  let settings = $state<PluginSettings | null>(null);
  let usage = $state<PluginUse[]>([]);
  let schema = $state<Schema | null>(null);
  let schemas = $state<{ in: SchemaField[] | null; out: SchemaField[] | null } | null>(null);
  let docsHtml = $state<string | null>(null);
  let loadError = $state<string | null>(null);

  async function load() {
    try {
      const [p, s, u] = await Promise.all([getPluginInfo(id), getPluginSettings(), getUsage()]);
      plugin = p;
      settings = s;
      usage = u.filter((x) => x.plugin_id === id);
      loadError = null;
      document.title = `${p.name} · flwrs`;
      schema = p.has_launch_args_schema ? ((await getLaunchArgsSchema(id)) as Schema) : null;
      schemas = await getNodeSchemas(id).catch(() => null);
      docsHtml = p.ui?.docs ? await renderDocs(id) : null;
    } catch (e) {
      loadError = e instanceof ApiError && e.status === 404 ? 'No such plugin.' : e instanceof Error ? e.message : String(e);
    }
  }

  /** The package's Markdown docs as sanitized HTML (both libraries load only here). */
  async function renderDocs(pluginId: string): Promise<string | null> {
    const text = await getDocs(pluginId);
    if (!text) return null;
    const [{ marked }, { default: DOMPurify }] = await Promise.all([import('marked'), import('dompurify')]);
    const html = await marked.parse(text, { gfm: true });
    return DOMPurify.sanitize(html, { FORBID_TAGS: ['style', 'form', 'input'], FORBID_ATTR: ['style'] });
  }

  $effect(() => {
    void id;
    load();
  });

  let tab = $state('overview');
  const tabs = $derived([
    { id: 'overview', label: 'Overview' },
    ...(docsHtml ? [{ id: 'docs', label: 'Docs' }] : []),
    { id: 'settings', label: 'Node settings' },
    { id: 'schemas', label: 'Payloads' },
    { id: 'usage', label: `Used by${usage.length ? ` (${usage.length})` : ''}` },
  ]);

  const byScene = $derived.by(() => {
    const map = new Map<string, { name: string; nodes: PluginUse[] }>();
    for (const u of usage) {
      const entry = map.get(u.scene_id) ?? { name: u.scene_name, nodes: [] };
      entry.nodes.push(u);
      map.set(u.scene_id, entry);
    }
    return [...map.entries()];
  });
  const liveUses = $derived(usage.filter((u) => !u.draft_only).length);

  // ---- upgrade / delete ----

  let notice = $state<{ kind: 'ok' | 'danger'; title: string; details: string[] } | null>(null);
  let picker = $state<HTMLInputElement>();
  let upgrading = $state(false);
  let deleting = $state(false);

  async function upgrade(file: File | undefined) {
    if (!file || !plugin) return;
    upgrading = true;
    try {
      const next = await upgradePlugin(plugin.id, file);
      notice = { kind: 'ok', title: `Upgraded to ${next.version}`, details: [] };
      await load();
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      // A refused upgrade lists the nodes its new version doesn't fit.
      notice = { kind: 'danger', title: `Upgrade refused: ${error.message}`, details: error.details };
    } finally {
      upgrading = false;
      if (picker) picker.value = '';
    }
  }

  const TYPE_ICON = { SOURCE: Zap, TRANSFORM: Shuffle, SINK: Inbox };
  let iconFailed = $state(false);
  const fmtTime = (ms: number) => new Date(ms).toLocaleString();
</script>

<TokenPrompt />

{#snippet fieldList(fields: [string, Schema][], required: string[], root: Schema)}
  <ul class="fields">
    {#each fields as [key, raw] (key)}
      {@const f = resolve(root, raw)}
      {@const kind = kindOf(f)}
      {@const hint = constraintHint(f)}
      <li>
        <div class="field-head">
          <strong>{labelOf(key, f)}</strong>
          <code class="muted">{key}</code>
          <span class="badge">{f['x-flwrs-launch']?.source === 'upload' ? 'file' : kind === 'list' ? `list of ${f.items?.type ?? 'values'}` : kind}</span>
          {#if required.includes(key)}<span class="badge accent">required</span>{/if}
          {#if f.writeOnly}<span class="badge warn">secret</span>{/if}
          {#if f['x-flwrs-code']}<span class="badge">{f['x-flwrs-code'].language} code</span>{/if}
          {#if plugin?.configured_fields.includes(key)}<span class="badge accent">set by its configurator</span>{/if}
          {#if f['x-flwrs-advanced']}<span class="badge">advanced</span>{/if}
          {#if f['x-flwrs-listen']}<span class="badge">listen address</span>{/if}
          {#if f['x-flwrs-platforms']}<span class="badge">{f['x-flwrs-platforms'].join(', ')} only</span>{/if}
        </div>
        {#if f.description}<p class="desc">{f.description}</p>{/if}
        <p class="meta muted">
          {#if f.default !== undefined}Default <code>{JSON.stringify(f.default)}</code>{/if}
          {#if f.enum}{f.default !== undefined ? ' · ' : ''}One of {#each f.enum as v, i (i)}{i ? ', ' : ''}<code>{JSON.stringify(v)}</code>{/each}{/if}
          {#if hint}{f.default !== undefined || f.enum ? ' · ' : ''}{hint}{/if}
          {#if f['x-flwrs-launch']?.via}{f.default !== undefined || f.enum || hint ? ' · ' : ''}Delivered as {f['x-flwrs-launch'].via === 'env' ? 'an environment variable' : f['x-flwrs-launch'].via === 'file' ? 'a file in config/' : 'an argument'}{/if}
        </p>
        {#if kind === 'object' && f.properties}
          {@render fieldList(Object.entries(f.properties), f.required ?? [], root)}
        {/if}
      </li>
    {/each}
  </ul>
{/snippet}

<Page>
  {#if loadError}
    <div class="card gone">
      <h2>{loadError}</h2>
      <a class="btn" href="/plugins"><ArrowLeft size={16} strokeWidth={1.75} />All plugins</a>
    </div>
  {:else if !plugin}
    <div class="card gone muted">Loading…</div>
  {:else}
    {@const Icon = TYPE_ICON[plugin.type]}
    <header style:--plugin-color={plugin.ui?.color ?? undefined}>
      <a class="btn ghost icon" href="/plugins" aria-label="All plugins" title="All plugins"><ArrowLeft size={16} strokeWidth={1.75} /></a>
      <span class="icon">
        {#if plugin.ui?.icon && !iconFailed}
          <img src={pluginIconUrl(plugin.id)} alt="" width="28" height="28" onerror={() => (iconFailed = true)} />
        {:else}
          <Icon size={22} strokeWidth={1.75} />
        {/if}
      </span>
      <div class="title">
        <h1>{plugin.name} <span class="muted version">{plugin.version}</span></h1>
        <p class="muted">
          <span class="badge">{plugin.type.toLowerCase()}</span>
          {#if plugin.remote_only}<span class="badge accent" title="A remote plugin definition: it runs elsewhere">remote</span>{/if}
          {#each plugin.ui?.categories ?? [] as c (c)}<span class="badge">{c}</span>{/each}
          {plugin.description ?? ''}
        </p>
      </div>
      <div class="header-actions">
        {#if plugin.remote_only}
          <a class="btn" href={`/plugins/by-id/${encodeURIComponent(plugin.id)}/edit`} title="Edit this remote plugin definition"><Pencil size={16} strokeWidth={1.75} />Edit</a>
          <a class="btn" href={exportUrl(plugin.id)} download title="Download it as a package (.zip) another server can install"><Download size={16} strokeWidth={1.75} />Export</a>
        {/if}
        <button class="btn" type="button" disabled={upgrading || !settings?.allow_upload} title={settings?.allow_upload ? `A new package for ${plugin.id}` : 'Uploads are off on this server'} onclick={() => picker?.click()}>
          <Upload size={16} strokeWidth={1.75} />{upgrading ? 'Upgrading…' : 'Upgrade'}
        </button>
        <button class="btn ghost danger" type="button" disabled={(!settings?.allow_upload && !plugin.remote_only) || liveUses > 0} title={liveUses ? 'In use: remove its nodes first' : 'Delete'} onclick={() => (deleting = true)}>
          <Trash size={16} strokeWidth={1.75} />Delete
        </button>
        <input bind:this={picker} type="file" accept={PACKAGE_ACCEPT} hidden onchange={(e) => upgrade(e.currentTarget.files?.[0])} />
      </div>
    </header>

    {#if notice}
      <div class="notice {notice.kind}" role="status">
        <div class="grow">
          {notice.title}
          {#if notice.details.length}<ul>{#each notice.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}
        </div>
        <button class="btn ghost icon" type="button" aria-label="Dismiss" onclick={() => (notice = null)}><X size={16} strokeWidth={1.75} /></button>
      </div>
    {/if}
    {#if !plugin.installed}
      <div class="notice danger">This plugin’s files are missing on the server: its nodes can’t start. Upgrade it with its package to put them back.</div>
    {/if}

    <Tabs {tabs} active={tab} onselect={(t) => (tab = t)} />

    <div class="body">
      {#if tab === 'overview'}
        <dl class="card overview">
          <dt>Id</dt><dd class="mono">{plugin.id}</dd>
          <dt>Version</dt><dd class="mono">{plugin.version}</dd>
          <dt>Type</dt><dd>{plugin.type.toLowerCase()}</dd>
          <dt>Runs</dt><dd>{plugin.remote_only ? 'elsewhere: a remote plugin definition (nothing runs here; its nodes are remote)' : 'here (local nodes) or elsewhere (remote nodes)'}</dd>
          {#if !plugin.remote_only}<dt>Platforms</dt><dd>{plugin.platforms?.join(', ') ?? 'any'}</dd>{/if}
          <dt>Manifest</dt><dd>version {plugin.manifest_version}; launch args as {plugin.launch_args_style}</dd>
          {#if !plugin.remote_only}<dt>Cores</dt><dd>{plugin.takes_threads ? 'takes a core count (nodes’ runtime.threads)' : 'decides by itself'}</dd>{/if}
          <dt>Configurator</dt><dd>{plugin.configured_fields.length ? `sets ${plugin.configured_fields.join(', ')} from the other settings` : 'none'}</dd>
          <dt>Payloads</dt>
          <dd>
            {#if plugin.type !== 'SOURCE'}in: {plugin.node_schemas.in ? 'from each node’s settings' : 'fixed'}{/if}
            {#if plugin.type === 'TRANSFORM'} · {/if}
            {#if plugin.type !== 'SINK'}out: {plugin.node_schemas.out ? 'from each node’s settings' : 'fixed'}{/if}
          </dd>
          <dt>Package</dt><dd class="mono" title={plugin.archive_sha256}>sha256 {plugin.archive_sha256.slice(0, 16)}…</dd>
          <dt>Installed</dt><dd>{fmtTime(plugin.create_time)}{plugin.update_time !== plugin.create_time ? ` · upgraded ${fmtTime(plugin.update_time)}` : ''}</dd>
          {#if settings}<dt>Uploads</dt><dd class="muted">{settings.allow_upload ? `on (up to ${formatBytes(settings.max_upload_bytes)}${settings.admin_token_required ? '; with the admin token' : '; from this machine only'})` : 'off on this server'}</dd>{/if}
        </dl>
      {:else if tab === 'docs' && docsHtml}
        <!-- Sanitized with DOMPurify (the docs come from the package). -->
        <article class="card docs">{@html docsHtml}</article>
      {:else if tab === 'settings'}
        {#if schema?.properties}
          <div class="card settings">
            {#each topFields(schema, plugin.configured_fields) as group (`${group.advanced}:${group.group}`)}
              <section>
                <h3>{group.group ?? (group.advanced ? 'Advanced' : 'Settings')}</h3>
                {@render fieldList(group.fields.map((f) => [f.key, f.schema] as [string, Schema]), schema.required ?? [], schema)}
              </section>
            {/each}
          </div>
        {:else}
          <p class="muted">This plugin takes no settings.</p>
        {/if}
      {:else if tab === 'schemas'}
        <div class="schemas">
          {#each [['in', 'Takes', plugin.node_schemas.in], ['out', 'Sends', plugin.node_schemas.out]] as const as [side, label, perNode] (side)}
            {#if (side === 'in' && plugin.type !== 'SOURCE') || (side === 'out' && plugin.type !== 'SINK')}
              <section class="card">
                <h3>{label}</h3>
                {#if perNode}<p class="muted small">Each node sets its own in its settings{schemas?.[side] ? '; without one, this default:' : '.'}</p>{/if}
                {#if schemas?.[side]}
                  <SchemaTree fields={schemas[side]!} side={side === 'in' ? 'target' : 'source'} onpick={() => {}} />
                {:else if !perNode}
                  <p class="muted small">No schema.</p>
                {/if}
              </section>
            {/if}
          {/each}
        </div>
      {:else if tab === 'usage'}
        {#if byScene.length === 0}
          <p class="muted">No node uses this plugin.</p>
        {:else}
          <div class="card usage">
            {#each byScene as [sceneId, scene] (sceneId)}
              <section>
                <h3><a href={`/scenes/by-id/${encodeURIComponent(sceneId)}`}>{scene.name}</a></h3>
                <ul>
                  {#each scene.nodes as n (n.node_id)}
                    <li>
                      <a href={`/scenes/by-id/${encodeURIComponent(sceneId)}/nodes/by-id/${encodeURIComponent(n.node_id)}`}>{n.node_name ?? plugin.name}</a>
                      <span class="mono muted">{n.node_id}</span>
                      {#if n.draft_only}<span class="badge">only in the draft</span>{/if}
                    </li>
                  {/each}
                </ul>
              </section>
            {/each}
          </div>
        {/if}
      {/if}
    </div>
  {/if}
</Page>

{#if deleting && plugin}
  <ConfirmDialog
    title={`Delete ${plugin.name}?`}
    confirm="Delete"
    danger
    onconfirm={async () => {
      await deletePlugin(plugin!.id);
      navigate('/plugins');
    }}
    onclose={() => (deleting = false)}
  >
    <p>The package and its files are removed from the server.{usage.length ? ' Nodes in drafts that use it will show it as missing.' : ''}</p>
  </ConfirmDialog>
{/if}

<style>
  header {
    --plugin-color: var(--accent);
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
  }

  .icon {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    border-radius: var(--radius);
    background: var(--surface-2);
    color: var(--plugin-color);
  }

  .title {
    flex: 1;
    min-width: 0;
  }

  .title p {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1);
    font-size: var(--text-sm);
  }

  .version {
    font-family: var(--font-mono);
    font-size: var(--text-md);
    font-weight: 400;
  }

  .header-actions {
    display: flex;
    gap: var(--space-2);
  }

  .grow {
    flex: 1;
  }

  .small {
    font-size: var(--text-sm);
  }

  .gone {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-2);
    padding: var(--space-5);
  }

  .overview {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-2) var(--space-4);
    margin: 0;
    padding: var(--space-4);
    font-size: var(--text-sm);
  }

  .overview dt {
    color: var(--text-muted);
  }

  .overview dd {
    margin: 0;
  }

  .docs {
    max-width: 860px;
    padding: var(--space-4) var(--space-5);
    line-height: 1.6;
    overflow-wrap: anywhere;
  }

  .docs :global(h1),
  .docs :global(h2),
  .docs :global(h3) {
    margin: var(--space-4) 0 var(--space-2);
  }

  .docs :global(p),
  .docs :global(ul),
  .docs :global(ol),
  .docs :global(table) {
    margin: 0 0 var(--space-3);
  }

  .docs :global(pre) {
    padding: var(--space-3);
    overflow: auto;
    border-radius: var(--radius);
    background: var(--surface-2);
  }

  .docs :global(table) {
    border-collapse: collapse;
  }

  .docs :global(th),
  .docs :global(td) {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
  }

  .settings,
  .usage {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-4);
  }

  h3 {
    margin: 0 0 var(--space-2);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .usage h3 {
    text-transform: none;
    letter-spacing: 0;
    font-size: var(--text-sm);
  }

  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .fields .fields {
    margin-top: var(--space-2);
    padding-left: var(--space-4);
    border-left: 1px solid var(--border);
  }

  .field-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-sm);
  }

  .desc {
    margin: 2px 0 0;
    font-size: var(--text-sm);
  }

  .meta {
    margin: 2px 0 0;
    font-size: var(--text-xs);
  }

  .schemas {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
    gap: var(--space-3);
  }

  .schemas .card {
    padding: var(--space-3);
  }

  .usage ul {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
    font-size: var(--text-sm);
  }

  .usage li {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
