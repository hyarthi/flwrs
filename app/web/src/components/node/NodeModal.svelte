<script lang="ts">
  // A node's modal (route `…/nodes/by-id/{node}`): Settings, Uploads and
  // Stats. It shows the node as the draft has it (if the scene has a
  // draft), else as live; edits go to the draft.
  import { untrack } from 'svelte';
  import Trash from '@lucide/svelte/icons/trash-2';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import Copy from '@lucide/svelte/icons/copy';
  import Modal from '../Modal.svelte';
  import Tabs from '../Tabs.svelte';
  import NodeSettings from './NodeSettings.svelte';
  import NodeUploads from './NodeUploads.svelte';
  import NodeStats from './NodeStats.svelte';
  import NodeLogs from './NodeLogs.svelte';
  import GraphEditor from '../graph/GraphEditor.svelte';
  import Wand from '@lucide/svelte/icons/wand-sparkles';
  import { ApiError } from '../../lib/api';
  import { topFields, type Schema } from '../../lib/schemaForm';
  import { groupsOf, membersByGroup } from '../../lib/groups';
  import { withoutLocation } from '../../lib/graphModel';
  import {
    configureDraftNode,
    getLaunchArgsSchema,
    getPlugin,
    listUploads,
    messagesFor,
    removeDraftNode,
    restoreDraftNode,
    type GraphNode,
    type Plugin,
    type SceneStatusEvent,
    type SceneView,
    type Upload,
  } from '../../lib/sceneView';

  let {
    view,
    status,
    nodeId,
    onclose,
  }: { view: SceneView; status: SceneStatusEvent | null; nodeId: string; onclose: () => void } = $props();

  const sceneId = $derived(view.scene.id);
  const draftNode = $derived(view.draft?.nodes.find((n) => n.id === nodeId));
  const liveNode = $derived(view.nodes.find((n) => n.id === nodeId));
  const node = $derived<GraphNode | undefined>(view.draft ? draftNode : liveNode);
  const removed = $derived(draftNode?.change === 'removed');

  type TabId = 'graph' | 'settings' | 'uploads' | 'stats' | 'logs';
  let tab = $state<TabId>('settings');
  /** Tabs shown so far: they stay mounted (hidden), so their inputs survive switching. */
  let visited = $state(new Set<TabId>(['settings']));
  function select(t: TabId) {
    tab = t;
    if (!visited.has(t)) visited = new Set([...visited, t]);
  }

  // ---- the plugin and its settings schema ----

  let plugin = $state<Plugin | null>(null);
  let schema = $state<Schema | null>(null);
  let loadError = $state<string | null>(null);
  let loaded = $state(false);
  $effect(() => {
    const pluginId = node?.plugin_id;
    if (!pluginId) return;
    untrack(() => {
      loaded = false;
      getPlugin(pluginId)
        .then(async (p) => {
          plugin = p;
          schema = p.has_launch_args_schema ? ((await getLaunchArgsSchema(pluginId)) as Schema) : null;
        })
        .catch((e) => {
          if (!(e instanceof ApiError && e.status === 404)) loadError = e instanceof Error ? e.message : String(e);
        })
        .finally(() => (loaded = true));
    });
  });

  /** The setting holding a graph, if the plugin has one: it gets a tab. */
  const graphField = $derived.by(() => {
    for (const [key, property] of Object.entries(schema?.properties ?? {})) {
      const catalog = property['x-flwrs-graph']?.catalog;
      if (catalog) return { key, catalog };
    }
    return null;
  });
  /** What a graph save changed, for the settings form to follow. */
  let graphSaved = $state<{ keys: string[]; args: Record<string, unknown> } | null>(null);

  // ---- uploads: the draft's, else live's (fetched) ----

  let liveUploads = $state<Upload[]>([]);
  function fetchLive() {
    if (view.draft || !liveNode) return;
    listUploads(sceneId, nodeId, false)
      .then((list) => (liveUploads = list))
      .catch(() => {});
  }
  $effect(() => {
    if (!view.draft) untrack(fetchLive);
  });
  const uploads = $derived<Upload[]>(view.draft ? (draftNode?.uploads ?? []) : liveUploads);

  const usedBy = $derived.by(() => {
    const map = new Map<string, string[]>();
    if (!schema || !node) return map;
    const args = (node.config.launch_args ?? {}) as Record<string, unknown>;
    for (const group of topFields(schema, [])) {
      for (const field of group.fields) {
        const value = args[field.key];
        if (field.upload && typeof value === 'string') map.set(value, [...(map.get(value) ?? []), field.key]);
      }
    }
    return map;
  });

  // ---- the form: re-created from what a save returned ----

  let formNode = $state<GraphNode | null>(null);
  let formKey = $state(0);
  let savedToken = $state<string | null>(null);
  $effect(() => {
    if (node && !formNode) untrack(() => (formNode = node));
  });

  const check = $derived(view.draft_check ?? view.check);
  const problems = $derived(messagesFor(check.problems, nodeId));
  const warnings = $derived(messagesFor(check.warnings, nodeId));
  const running = $derived(
    !!liveNode && (status?.state === 'RUNNING' || status?.state === 'WAITING' || status?.state === 'FAILED'),
  );
  const preflight = $derived(
    [...(status?.preflight ?? []), ...(status?.failed_update?.problems ?? []), ...(view.draft?.submit?.problems ?? [])].filter(
      (p) => p.node_id === nodeId,
    ),
  );

  let actionError = $state<string | null>(null);
  async function remove() {
    try {
      await removeDraftNode(sceneId, nodeId);
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    }
  }
  /** Whether the configurator can run for it: the plugin has one; a remote node needs an endpoint. */
  const configurable = $derived(
    !!plugin?.configured_fields.length && !!node && (node.config.launch !== 'remote' || !!(node.config as Record<string, unknown>).configurator),
  );
  let configuring = $state(false);
  async function configure() {
    configuring = true;
    actionError = null;
    try {
      const saved = await configureDraftNode(sceneId, nodeId);
      formNode = saved;
      formKey++;
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      actionError = [error.message, ...error.details].join(': ');
    } finally {
      configuring = false;
    }
  }

  async function restore() {
    try {
      await restoreDraftNode(sceneId, nodeId);
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    }
  }

  const title = $derived(node ? (node.name ?? plugin?.name ?? node.plugin_id) : 'Node');
</script>

<Modal {title} {onclose} size={tab === 'graph' ? 'full' : 'lg'} fixed>
  {#if !node}
    <p class="muted">This node isn’t in the scene{view.draft ? '’s draft' : ''}.</p>
  {:else}
    <div class="meta">
      <span class="mono muted">{node.plugin_id}{node.plugin_version ? ` ${node.plugin_version}` : ''} · {node.type.toLowerCase()}</span>
      <button class="mono muted id" type="button" title="Copy the node id" onclick={() => navigator.clipboard?.writeText(node!.id).catch(() => {})}>
        {node.id}<Copy size={12} strokeWidth={1.75} />
      </button>
      {#if draftNode && draftNode.change !== 'unchanged'}
        <span class="badge {draftNode.change === 'removed' ? 'danger' : 'accent'}">{draftNode.change === 'added' ? 'new' : draftNode.change} in the draft</span>
      {/if}
      <span class="spacer"></span>
      {#if configurable && !removed}
        <button class="btn ghost" type="button" disabled={configuring} onclick={configure} title={`Run the plugin’s configurator again on the saved settings (sets ${plugin?.configured_fields.join(', ')}), in the draft`}>
          <Wand size={16} strokeWidth={1.75} />{configuring ? 'Configuring…' : 'Configure again'}
        </button>
      {/if}
      {#if removed}
        <button class="btn" type="button" onclick={restore}><RotateCcw size={16} strokeWidth={1.75} />Restore</button>
      {:else}
        <button class="btn ghost danger" type="button" onclick={remove} title="Remove from the draft (restorable until submitted)">
          <Trash size={16} strokeWidth={1.75} />Remove
        </button>
      {/if}
    </div>

    {#if actionError}<div class="notice danger" role="alert">{actionError}</div>{/if}
    {#if loadError}<div class="notice danger" role="alert">Couldn’t load the plugin: {loadError}</div>{/if}
    {#if preflight.length}
      <div class="notice {preflight.some((p) => !p.retryable) ? 'danger' : 'warn'}" role="status">
        Pre-flight check on this machine:
        <ul>{#each preflight as p, i (i)}<li>{withoutLocation(p.message)}{p.retryable ? ' (retrying)' : ''}</li>{/each}</ul>
      </div>
    {/if}
    {#if warnings.length}
      <div class="notice warn"><ul>{#each warnings as w, i (i)}<li>{withoutLocation(w)}</li>{/each}</ul></div>
    {/if}

    <Tabs
      tabs={[
        ...(graphField && !removed ? [{ id: 'graph', label: 'Graph' }] : []),
        { id: 'settings', label: 'Settings' },
        { id: 'uploads', label: `Uploads${uploads.length ? ` (${uploads.length})` : ''}` },
        { id: 'stats', label: 'Stats' },
        ...(liveNode ? [{ id: 'logs', label: 'Logs' }] : []),
      ]}
      active={tab}
      onselect={(t) => select(t as TabId)}
    />

    <!-- Only the tab's content scrolls; the header, the tabs and the
         settings' actions stay put. -->
    <div class="panes">
      {#if graphField && plugin && schema && visited.has('graph') && !removed}
        <div class="pane" hidden={tab !== 'graph'}>
          <GraphEditor
            {view}
            {node}
            {plugin}
            {schema}
            field={graphField.key}
            catalogPath={graphField.catalog}
            {uploads}
            messages={[...problems, ...warnings, ...preflight.map((p) => p.message)]}
            active={tab === 'graph'}
            running={!!liveNode && status?.state === 'RUNNING'}
            liveArgs={(liveNode?.config.launch_args ?? null) as Record<string, unknown> | null}
            onopensettings={() => select('settings')}
            onsaved={(saved) => {
              const args = (saved.config.launch_args ?? {}) as Record<string, unknown>;
              graphSaved = { keys: Object.keys(args).filter((k) => k === graphField?.key || !schema?.properties?.[k]?.writeOnly), args };
            }}
          />
        </div>
      {/if}
      <div class="pane" hidden={tab !== 'settings'}>
        {#if removed}
          <p class="muted">Removed in the draft. Restore it to edit its settings.</p>
        {:else if !loaded || !formNode}
          <p class="muted">Loading…</p>
        {:else}
          {#key formKey}
            <NodeSettings
              {sceneId}
              node={formNode}
              {plugin}
              {schema}
              {uploads}
              problems={problems.map(withoutLocation)}
              sceneDefaults={(view.draft?.settings ?? view.scene).defaults}
              sceneGroups={(view.draft?.settings ?? view.scene).groups ?? []}
              groupMembers={membersByGroup(view.draft?.nodes ?? view.nodes)}
              currentGroups={node ? groupsOf(node.config) : []}
              threads={check.threads?.[nodeId] ?? null}
              {savedToken}
              {graphSaved}
              graphField={graphField?.key ?? null}
              onopengraph={() => select('graph')}
              justSaved={formKey > 0}
              onsaved={(saved, token) => {
                formNode = saved;
                savedToken = token ?? savedToken;
                formKey++;
              }}
            />
          {/key}
        {/if}
      </div>
      {#if visited.has('uploads')}
        <div class="pane scroll" hidden={tab !== 'uploads'}>
          <NodeUploads {sceneId} {nodeId} {uploads} inDraft={!!view.draft} {usedBy} onchanged={fetchLive} />
        </div>
      {/if}
      {#if visited.has('stats')}
        <div class="pane scroll" hidden={tab !== 'stats'}>
          <NodeStats {status} {nodeId} {running} />
        </div>
      {/if}
      {#if liveNode && visited.has('logs')}
        <div class="pane" hidden={tab !== 'logs'}>
          <NodeLogs {sceneId} {nodeId} />
        </div>
      {/if}
    </div>
  {/if}
</Modal>

<style>
  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-top: calc(-1 * var(--space-2));
  }

  .spacer {
    flex: 1;
  }

  .id {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0;
    border: 0;
    background: none;
    cursor: pointer;
  }

  .panes {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-height: 0;
  }

  .pane {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-height: 0;
  }

  .pane[hidden] {
    display: none;
  }

  .pane.scroll {
    overflow-y: auto;
    /* Room for focus rings and the scrollbar, as in the settings. */
    margin: 0 calc(-1 * var(--space-2));
    padding: var(--space-1) var(--space-2);
  }

  ul {
    margin: 0;
    padding-left: var(--space-4);
  }
</style>
