<script lang="ts">
  // The scene's Layout tab: the palette, the canvas and the draft bar. The
  // canvas shows the draft if there is one (with its diff markers: new,
  // changed, conflict, and removed rows as shadows), else the live graph.
  // Every edit goes to the draft (the server starts one on the first edit);
  // the scene's stream brings the result back. Positions are layout: they're
  // saved live and in the draft alike, and never count as changes.
  import {
    SvelteFlow,
    Background,
    Controls,
    MiniMap,
    useSvelteFlow,
    type Connection,
    type Edge,
    type Node,
  } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import { untrack } from 'svelte';
  import X from '@lucide/svelte/icons/x';
  import FlowNode, { type FlowNodeData } from './FlowNode.svelte';
  import FlowEdge, { type FlowEdgeData } from './FlowEdge.svelte';
  import Palette, { PLUGIN_DRAG_TYPE } from './Palette.svelte';
  import DraftBar from './DraftBar.svelte';
  import { ApiError } from '../../lib/api';
  import { navigate } from '../../lib/router.svelte';
  import { theme } from '../../lib/theme.svelte';
  import {
    createDraftEdge,
    createDraftNode,
    edgeMessages,
    listPlugins,
    messagesFor,
    pluginIconUrl,
    removeDraftEdge,
    removeDraftNode,
    restoreDraftEdge,
    restoreDraftNode,
    setLayout,
    type Change,
    type Conflict,
    type GraphEdge,
    type GraphNode,
    type Plugin,
    type Position,
    type SceneStatusEvent,
    type SceneView,
  } from '../../lib/sceneView';

  let { view, status }: { view: SceneView; status: SceneStatusEvent | null } = $props();

  const sceneId = $derived(view.scene.id);
  const flow = useSvelteFlow();

  // ---- plugins (the palette, node icons and colours) ----

  let plugins = $state<Plugin[] | null>(null);
  let pluginsError = $state<string | null>(null);
  listPlugins()
    .then((list) => (plugins = list))
    .catch((e) => (pluginsError = `Couldn't load the plugins: ${e instanceof Error ? e.message : e}`));
  const pluginById = $derived(new Map((plugins ?? []).map((p) => [p.id, p])));

  // ---- notices ----

  let notice = $state<{ title: string; details: string[] } | null>(null);
  function fail(title: string, e: unknown) {
    const error = e instanceof ApiError ? e : new ApiError(0, String(e));
    notice = { title: `${title}: ${error.message}`, details: error.details };
  }

  // ---- what the canvas shows ----

  type Row<T> = T & { change: Change | null; conflict: Conflict | null; parts: string[] };
  const graphNodes = $derived<Row<GraphNode>[]>(
    view.draft ? view.draft.nodes : view.nodes.map((n) => ({ ...n, change: null, conflict: null, parts: [] })),
  );
  const graphEdges = $derived<Row<GraphEdge>[]>(
    view.draft ? view.draft.edges : view.edges.map((e) => ({ ...e, change: null, conflict: null, parts: [] })),
  );
  const check = $derived(view.draft_check ?? view.check);
  const states = $derived(new Map((status?.nodes ?? []).map((n) => [n.node_id, n.status.state])));

  let nodes = $state.raw<Node<FlowNodeData>[]>([]);
  let edges = $state.raw<Edge<FlowEdgeData>[]>([]);

  /** Positions moved here and not saved yet (they win over the stream's). */
  const moving = new Map<string, Position>();
  /** Nodes placed automatically and saved once (not again if that failed). */
  const autoPlaced = new Set<string>();

  const NODE_WIDTH = 220;
  const NODE_HEIGHT = 48;
  const COLUMN = { SOURCE: 0, TRANSFORM: 320, SINK: 640 } as const;

  /** Where unplaced nodes go: a column per type, below what's placed. */
  function autoPositions(list: Row<GraphNode>[]): Map<string, Position> {
    const placed = list.filter((n) => n.position || moving.has(n.id));
    let top = 0;
    for (const n of placed) top = Math.max(top, (moving.get(n.id) ?? n.position!).y + NODE_HEIGHT + 48);
    const rows = { SOURCE: 0, TRANSFORM: 0, SINK: 0 };
    const out = new Map<string, Position>();
    for (const n of list) {
      if (n.position || moving.has(n.id)) continue;
      out.set(n.id, { x: COLUMN[n.type] ?? 0, y: top + rows[n.type]++ * 96 });
    }
    return out;
  }

  // Rebuilt from each `scene` / `status` event; selection is kept.
  $effect(() => {
    const list = graphNodes;
    const edgeList = graphEdges;
    const problems = check.problems;
    const warnings = check.warnings;
    const live = states;
    const byId = pluginById;
    untrack(() => {
      const selected = new Set(nodes.filter((n) => n.selected).map((n) => n.id));
      const selectedEdges = new Set(edges.filter((e) => e.selected).map((e) => e.id));
      const auto = autoPositions(list);
      nodes = list.map((n) => {
        const plugin = byId.get(n.plugin_id);
        const removed = n.change === 'removed';
        return {
          id: n.id,
          type: 'flwrs',
          position: moving.get(n.id) ?? n.position ?? auto.get(n.id)!,
          selected: selected.has(n.id),
          connectable: !removed,
          deletable: !removed,
          data: {
            title: n.name ?? plugin?.name ?? n.plugin_id,
            subtitle: n.name ? (plugin?.name ?? n.plugin_id) : n.plugin_id,
            nodeType: n.type,
            pluginId: n.plugin_id,
            color: plugin?.ui?.color ?? null,
            icon: plugin?.ui?.icon ? pluginIconUrl(n.plugin_id) : null,
            change: n.change,
            conflict: n.conflict,
            parts: n.parts,
            state: live.get(n.id) ?? null,
            problems: messagesFor(problems, n.id),
            warnings: messagesFor(warnings, n.id),
            onopen: openNode,
            onrestore: restoreNode,
          },
        };
      });
      edges = edgeList.map((e) => ({
        id: e.id,
        type: 'flwrs',
        source: e.source_node_id,
        target: e.target_node_id,
        selected: selectedEdges.has(e.id),
        deletable: e.change !== 'removed',
        data: {
          change: e.change,
          conflict: e.conflict,
          parts: e.parts,
          mapped: !!e.mapping && (!!e.mapping.script || (e.mapping.rules?.length ?? 0) > 0),
          filtered: !!e.mapping?.filter,
          problems: edgeMessages(problems, e.source_node_id, e.target_node_id),
          onrestore: restoreEdge,
        },
      }));
      // Save where unplaced nodes went, so they stay put.
      const unsaved = [...auto.entries()].filter(([id]) => !autoPlaced.has(id));
      if (unsaved.length) {
        for (const [id] of unsaved) autoPlaced.add(id);
        setLayout(sceneId, Object.fromEntries(unsaved)).catch(() => {});
      }
    });
  });

  // Fit the graph once, when the first nodes arrive.
  let fitted = false;
  $effect(() => {
    if (!fitted && nodes.length) {
      fitted = true;
      requestAnimationFrame(() => flow.fitView({ padding: 0.2, maxZoom: 1.25 }));
    }
  });

  // ---- edits ----

  function openNode(id: string) {
    navigate(`/scenes/by-id/${encodeURIComponent(sceneId)}/nodes/by-id/${encodeURIComponent(id)}`);
  }

  async function restoreNode(id: string) {
    try {
      await restoreDraftNode(sceneId, id);
    } catch (e) {
      fail("Couldn't restore the node", e);
    }
  }

  async function restoreEdge(id: string) {
    try {
      await restoreDraftEdge(sceneId, id);
    } catch (e) {
      fail("Couldn't restore the edge", e);
    }
  }

  async function addNode(plugin: Plugin, position: Position) {
    const at = { x: Math.round(position.x), y: Math.round(position.y) };
    try {
      const node = await createDraftNode(sceneId, { plugin_id: plugin.id, position: at });
      // Shown at once; the stream brings its state and check shortly.
      moving.set(node.id, at);
      nodes = [
        ...nodes.map((n) => ({ ...n, selected: false })),
        {
          id: node.id,
          type: 'flwrs',
          position: at,
          selected: true,
          data: {
            title: plugin.name,
            subtitle: plugin.id,
            nodeType: plugin.type,
            pluginId: plugin.id,
            color: plugin.ui?.color ?? null,
            icon: plugin.ui?.icon ? pluginIconUrl(plugin.id) : null,
            change: 'added',
            conflict: null,
            parts: [],
            state: null,
            problems: [],
            warnings: [],
            onopen: openNode,
            onrestore: restoreNode,
          },
        },
      ];
      setTimeout(() => moving.delete(node.id), 2000);
    } catch (e) {
      fail(`Couldn't add ${plugin.name}`, e);
    }
  }

  let canvas: HTMLDivElement;

  /** A palette click: the middle of the visible canvas, a little apart from the last added. */
  let nudge = 0;
  function addAtCenter(plugin: Plugin) {
    const rect = canvas.getBoundingClientRect();
    const center = flow.screenToFlowPosition({ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 });
    const offset = (nudge++ % 5) * 24;
    addNode(plugin, { x: center.x - NODE_WIDTH / 2 + offset, y: center.y - NODE_HEIGHT / 2 + offset });
  }

  function ondrop(event: DragEvent) {
    const id = event.dataTransfer?.getData(PLUGIN_DRAG_TYPE);
    const plugin = id ? pluginById.get(id) : undefined;
    if (!plugin) return;
    event.preventDefault();
    const at = flow.screenToFlowPosition({ x: event.clientX, y: event.clientY });
    addNode(plugin, { x: at.x - NODE_WIDTH / 2, y: at.y - NODE_HEIGHT / 2 });
  }

  function isValidConnection(c: Edge | Connection): boolean {
    if (!c.source || !c.target) return false;
    const source = graphNodes.find((n) => n.id === c.source);
    const target = graphNodes.find((n) => n.id === c.target);
    if (!source || !target || source.change === 'removed' || target.change === 'removed') return false;
    if (source.type === 'SINK' || target.type === 'SOURCE') return false;
    return !graphEdges.some((e) => e.change !== 'removed' && e.source_node_id === c.source && e.target_node_id === c.target);
  }

  async function connect(c: Connection) {
    try {
      const edge = await createDraftEdge(sceneId, c.source, c.target);
      edges = [
        ...edges,
        {
          id: edge.id,
          type: 'flwrs',
          source: edge.source_node_id,
          target: edge.target_node_id,
          data: { change: 'added', conflict: null, parts: [], mapped: false, filtered: false, problems: [], onrestore: restoreEdge },
        },
      ];
    } catch (e) {
      fail("Couldn't connect the nodes", e);
    }
  }

  async function remove(gone: { nodes: Node[]; edges: Edge[] }) {
    const nodeIds = new Set(gone.nodes.map((n) => n.id));
    // A node's edges go with it.
    const edgeIds = gone.edges.filter((e) => !nodeIds.has(e.source) && !nodeIds.has(e.target)).map((e) => e.id);
    try {
      for (const id of edgeIds) await removeDraftEdge(sceneId, id);
      for (const id of nodeIds) await removeDraftNode(sceneId, id);
    } catch (e) {
      fail("Couldn't remove everything selected", e);
    }
  }

  async function moved(dragged: Node[]) {
    if (!dragged.length) return;
    const positions: Record<string, Position> = {};
    for (const n of dragged) {
      const at = { x: Math.round(n.position.x), y: Math.round(n.position.y) };
      positions[n.id] = at;
      moving.set(n.id, at);
    }
    try {
      await setLayout(sceneId, positions);
    } catch (e) {
      fail("Couldn't save the positions", e);
    } finally {
      for (const id of Object.keys(positions)) if (moving.get(id) === positions[id]) moving.delete(id);
    }
  }

  const nodeTypes = { flwrs: FlowNode };
  const edgeTypes = { flwrs: FlowEdge };
</script>

<div class="layout">
  <Palette {plugins} error={pluginsError} onadd={addAtCenter} />
  <div class="main">
    <DraftBar {view} {check} onfocus={(id) => flow.fitView({ nodes: [{ id }], padding: 0.6, maxZoom: 1.25, duration: 300 })} onerror={fail} />
    {#if notice}
      <div class="notice danger floating" role="alert">
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
    <div
      class="canvas"
      bind:this={canvas}
      role="application"
      aria-label="Scene canvas"
      ondragover={(e) => {
        if (e.dataTransfer?.types.includes(PLUGIN_DRAG_TYPE)) {
          e.preventDefault();
          e.dataTransfer.dropEffect = 'copy';
        }
      }}
      {ondrop}
    >
      <SvelteFlow
        bind:nodes
        bind:edges
        {nodeTypes}
        {edgeTypes}
        colorMode={theme.current}
        snapGrid={[16, 16]}
        minZoom={0.2}
        maxZoom={2}
        deleteKey={['Backspace', 'Delete']}
        {isValidConnection}
        onbeforeconnect={(c) => {
          connect(c);
          return false;
        }}
        onbeforedelete={async (gone) => {
          await remove(gone);
          return false;
        }}
        onnodedragstop={({ nodes: dragged }) => moved(dragged)}
        proOptions={{ hideAttribution: true }}
      >
        <Background gap={16} />
        <Controls showLock={false} />
        <MiniMap pannable zoomable />
      </SvelteFlow>
      {#if nodes.length === 0}
        <div class="empty muted">Drag a plugin from the palette onto the canvas to add a node.</div>
      {/if}
    </div>
  </div>
</div>

<style>
  .layout {
    display: flex;
    flex: 1;
    min-height: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    overflow: hidden;
    background: var(--surface);
  }

  .main {
    position: relative;
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }

  .canvas {
    position: relative;
    flex: 1;
    min-height: 0;
  }

  .floating {
    position: absolute;
    top: 56px;
    left: 50%;
    z-index: 10;
    width: min(560px, calc(100% - 2 * var(--space-4)));
    transform: translateX(-50%);
    box-shadow: var(--shadow);
  }

  .grow {
    flex: 1;
  }

  .empty {
    position: absolute;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    pointer-events: none;
  }

  /* Svelte Flow in our tokens. */
  .canvas :global(.svelte-flow) {
    --xy-background-color: var(--bg);
    --xy-background-pattern-dots-color: var(--border-strong);
    --xy-edge-stroke: var(--border-strong);
    --xy-edge-stroke-selected: var(--accent);
    --xy-connectionline-stroke: var(--accent);
    --xy-handle-background-color: var(--border-strong);
    --xy-handle-border-color: var(--surface);
    --xy-selection-background-color: var(--accent-soft);
    --xy-selection-border: 1px dashed var(--accent);
    --xy-minimap-background-color: var(--surface);
    --xy-minimap-mask-background-color: light-dark(#0f172a14, #00000066);
    --xy-minimap-node-background-color: var(--border-strong);
    --xy-controls-button-background-color: var(--surface);
    --xy-controls-button-background-color-hover: var(--surface-hover);
    --xy-controls-button-color: var(--text);
    --xy-controls-button-color-hover: var(--text);
    --xy-controls-button-border-color: var(--border);
    --xy-controls-box-shadow: var(--shadow-sm);
    font-family: var(--font-sans);
  }

  .canvas :global(.svelte-flow__node-flwrs) {
    padding: 0;
    border: 0;
    background: none;
  }
</style>
