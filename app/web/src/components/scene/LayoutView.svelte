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
    ControlButton,
    MiniMap,
    useSvelteFlow,
    type Connection,
    type Edge,
    type Node,
  } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import { untrack } from 'svelte';
  import X from '@lucide/svelte/icons/x';
  import Info from '@lucide/svelte/icons/info';
  import EditorHelp from './EditorHelp.svelte';
  import FlowNode, { type FlowNodeData } from './FlowNode.svelte';
  import FlowEdge, { type EdgeFlow, type FlowEdgeData } from './FlowEdge.svelte';
  import EdgeDialog from './EdgeDialog.svelte';
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
    type NodeState,
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
  const queues = $derived(view.draft_queues ?? view.queues);

  // ---- flow states: what the motes and glows show ----

  /** A receiving node's queue between the last two `status` samples. */
  interface QueueTrend {
    spilling: boolean;
    /** Producers waited for room (block policy, queue full), or gave up. */
    blocked: boolean;
    /** Events dropped for a full queue (drop newest / drop oldest). */
    dropping: boolean;
  }
  let trends = $state(new Map<string, QueueTrend>());
  let lastQueues = new Map<string, { blocked_ms: number; dropped_timeout: number; dropped_full: number; evicted: number }>();
  $effect(() => {
    const reports = status?.nodes ?? [];
    untrack(() => {
      const next = new Map<string, QueueTrend>();
      const seen = new Map<string, { blocked_ms: number; dropped_timeout: number; dropped_full: number; evicted: number }>();
      for (const r of reports) {
        if (!r.queue) continue;
        const q = r.queue;
        const before = lastQueues.get(r.node_id);
        // A counter that went down: the node restarted; no trend from that sample.
        const grew = (now: number, then: number | undefined) => then !== undefined && now > then;
        next.set(r.node_id, {
          spilling: q.spilling,
          blocked: grew(q.blocked_ms, before?.blocked_ms) || grew(q.dropped_timeout, before?.dropped_timeout),
          dropping: grew(q.dropped_full, before?.dropped_full) || grew(q.evicted, before?.evicted),
        });
        seen.set(r.node_id, { blocked_ms: q.blocked_ms, dropped_timeout: q.dropped_timeout, dropped_full: q.dropped_full, evicted: q.evicted });
      }
      lastQueues = seen;
      trends = next;
    });
  });

  const liveNodeIds = $derived(new Set(view.nodes.map((n) => n.id)));
  const liveEdgeIds = $derived(new Set(view.edges.map((e) => e.id)));
  /** The scene runs: edges animate. */
  const flowing = $derived(view.scene.enabled && status?.state === 'RUNNING');

  function healthOf(n: Row<GraphNode>, state: NodeState | undefined): FlowNodeData['health'] {
    if (!view.scene.enabled || n.change === 'removed' || !liveNodeIds.has(n.id)) return null;
    if (state === 'RUNNING') return 'ok';
    if (state === 'DISCONNECTED' || state === 'LAUNCHING' || state === 'WAITING' || state === 'STOPPING') return 'warn';
    return 'bad';
  }

  /**
   * An edge's motes: none while the scene doesn't run, for removed edges,
   * or when an end that runs live isn't connected (nothing flows from or
   * into it). Ends only in the draft don't block it, but then (as for an
   * edge only in the draft) only flow and filter show, not queue states.
   */
  function flowOf(e: Row<GraphEdge>, byNode: Map<string, Row<GraphNode>>): EdgeFlow | null {
    if (!flowing || e.change === 'removed') return null;
    const ends = [e.source_node_id, e.target_node_id].map((id) => {
      const node = byNode.get(id);
      if (!node || node.change === 'removed') return false;
      if (!liveNodeIds.has(id)) return 'draft';
      return states.get(id) === 'RUNNING';
    });
    if (ends.includes(false)) return null;
    const stateful = liveEdgeIds.has(e.id) && ends.every((end) => end === true);
    const trend = stateful ? trends.get(e.target_node_id) : undefined;
    if (trend?.dropping) return { tone: 'jam', drop: true };
    if (trend?.blocked) return { tone: 'jam', drop: false };
    if (trend?.spilling) return { tone: 'spill', drop: false };
    return { tone: 'ok', drop: false };
  }

  /** Parallel and opposite edges between the same two nodes, spread apart. */
  function offsets(list: Row<GraphEdge>[]): Map<string, number> {
    const groups = new Map<string, string[]>();
    for (const e of list) {
      const key = [e.source_node_id, e.target_node_id].sort().join('|');
      groups.set(key, [...(groups.get(key) ?? []), e.id]);
    }
    const out = new Map<string, number>();
    for (const ids of groups.values()) {
      ids.sort();
      ids.forEach((id, i) => out.set(id, (i - (ids.length - 1) / 2) * 56));
    }
    return out;
  }

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
    void [queues, trends, flowing, liveNodeIds, liveEdgeIds];
    untrack(() => {
      let selected = new Set(nodes.filter((n) => n.selected).map((n) => n.id));
      if (pendingSelection && [...pendingSelection].every((id) => list.some((n) => n.id === id))) {
        selected = pendingSelection;
        pendingSelection = null;
      }
      const selectedEdges = new Set(edges.filter((e) => e.selected).map((e) => e.id));
      const auto = autoPositions(list);
      // What Svelte Flow measured stays with each node: a node without its
      // size counts as unmeasured, which hides its edges until it's measured
      // again (and restarts their animations: every status sample).
      const previous = new Map(nodes.map((n) => [n.id, n]));
      nodes = list.map((n) => {
        const plugin = byId.get(n.plugin_id);
        const removed = n.change === 'removed';
        const before = previous.get(n.id);
        return {
          ...(before?.measured && { measured: before.measured }),
          ...(before?.width !== undefined && { width: before.width }),
          ...(before?.height !== undefined && { height: before.height }),
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
            health: healthOf(n, live.get(n.id)),
            problems: messagesFor(problems, n.id),
            warnings: messagesFor(warnings, n.id),
            onopen: openNode,
            onrestore: restoreNode,
          },
        };
      });
      const byNode = new Map(list.map((n) => [n.id, n]));
      const spread = offsets(edgeList);
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
          via: e.via,
          offset: spread.get(e.id) ?? 0,
          queue: queues[e.target_node_id] ?? null,
          flow: flowOf(e, byNode),
          onrestore: restoreEdge,
          onconfig: (id: string) => (editingEdge = id),
          onmovebox: moveBox,
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

  let editingEdge = $state<string | null>(null);
  let helpOpen = $state(false);

  async function moveBox(id: string, via: Position | null) {
    try {
      await setLayout(sceneId, {}, { [id]: via });
    } catch (e) {
      fail("Couldn't save the edge's position", e);
    }
  }

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
      const node = await createDraftNode(sceneId, {
        plugin_id: plugin.id,
        position: at,
        // A remote plugin definition's nodes can only be remote.
        ...(plugin.remote_only ? { config: { launch: 'remote' } } : {}),
      });
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
            health: null,
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
          data: {
            change: 'added',
            conflict: null,
            parts: [],
            mapped: false,
            filtered: false,
            problems: [],
            via: null,
            offset: 0,
            queue: queues[edge.target_node_id] ?? null,
            flow: null,
            onrestore: restoreEdge,
            onconfig: (id: string) => (editingEdge = id),
            onmovebox: moveBox,
          },
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

  // ---- keyboard: undo / redo, copy / paste ----

  let draftBar: { step: (back: boolean) => Promise<void> } | undefined = $state();

  /** Keys and clipboard events are the canvas's unless a field or a dialog has them. */
  function mine(target: EventTarget | null): boolean {
    if (document.querySelector('dialog[open]')) return false;
    const el = target instanceof Element ? target : null;
    return !el?.closest('input, textarea, select, [contenteditable], .cm-editor');
  }

  function onkeydown(e: KeyboardEvent) {
    // `?` (some keyboards and tools report it as Shift + the slash key).
    const help = e.key === '?' || (e.code === 'Slash' && e.shiftKey);
    if (help && mine(e.target) && !e.metaKey && !e.ctrlKey && !e.altKey) {
      e.preventDefault();
      helpOpen = true;
      return;
    }
    if (!mine(e.target) || !(e.metaKey || e.ctrlKey) || e.altKey) return;
    const key = e.key.toLowerCase();
    if (key === 'z' || key === 'y') {
      e.preventDefault();
      draftBar?.step(key === 'z' && !e.shiftKey);
    } else if (key === 'd') {
      e.preventDefault();
      const copied = selection();
      if (copied) paste(copied);
    }
  }

  /** What a copy carries: the selected nodes, and the edges between them. */
  interface Copied {
    flwrs: 'nodes';
    version: 1;
    nodes: { key: string; plugin_id: string; name: string | null; config: Record<string, unknown>; position: Position }[];
    edges: { source: string; target: string; name: string | null; mapping: GraphEdge['mapping'] }[];
  }

  function selection(): Copied | null {
    const chosen = nodes.filter((n) => n.selected && n.data.change !== 'removed');
    if (!chosen.length) return null;
    const ids = new Set(chosen.map((n) => n.id));
    const rows = new Map(graphNodes.map((n) => [n.id, n]));
    return {
      flwrs: 'nodes',
      version: 1,
      nodes: chosen.map((n) => {
        const row = rows.get(n.id)!;
        // Tokens and secrets never leave the server: pasted nodes need them set again.
        const { token: _, ...config } = row.config as Record<string, unknown>;
        // A local node's configured fields are its configurator's: it sets them again.
        const configured = pluginById.get(row.plugin_id)?.configured_fields ?? [];
        if (config.launch !== 'remote' && configured.length && config.launch_args && typeof config.launch_args === 'object') {
          config.launch_args = Object.fromEntries(Object.entries(config.launch_args).filter(([k]) => !configured.includes(k)));
        }
        return { key: n.id, plugin_id: row.plugin_id, name: row.name, config, position: n.position };
      }),
      edges: graphEdges
        .filter((e) => e.change !== 'removed' && ids.has(e.source_node_id) && ids.has(e.target_node_id))
        .map((e) => ({ source: e.source_node_id, target: e.target_node_id, name: e.name, mapping: e.mapping })),
    };
  }

  function oncopy(e: ClipboardEvent) {
    if (!mine(e.target)) return;
    const copied = selection();
    if (!copied || !e.clipboardData) return;
    e.preventDefault();
    e.clipboardData.setData('text/plain', JSON.stringify(copied));
    pasteOffset = 0;
  }

  function onpaste(e: ClipboardEvent) {
    if (!mine(e.target)) return;
    const text = e.clipboardData?.getData('text/plain');
    let copied: Copied | null = null;
    try {
      const value = text ? JSON.parse(text) : null;
      if (value?.flwrs === 'nodes' && Array.isArray(value.nodes)) copied = value;
    } catch {
      // Not ours.
    }
    if (!copied) return;
    e.preventDefault();
    paste(copied);
  }

  let pasteOffset = 0;
  /** New nodes and edges like the copied ones, a little apart, as one undo step. */
  async function paste(copied: Copied) {
    pasteOffset += 48;
    const label = copied.nodes.length === 1 ? `Paste ${copied.nodes[0].name ?? copied.nodes[0].plugin_id}` : `Paste ${copied.nodes.length} nodes`;
    const group = { group: `paste-${Date.now()}-${Math.random().toString(36).slice(2)}`, label };
    const ids = new Map<string, string>();
    try {
      for (const n of copied.nodes) {
        const at = { x: Math.round(n.position.x + pasteOffset), y: Math.round(n.position.y + pasteOffset) };
        const created = await createDraftNode(sceneId, { plugin_id: n.plugin_id, name: n.name, config: n.config, position: at }, group);
        ids.set(n.key, created.id);
        moving.set(created.id, at);
      }
      for (const e of copied.edges) {
        const source = ids.get(e.source);
        const target = ids.get(e.target);
        if (source && target) await createDraftEdge(sceneId, source, target, { name: e.name, mapping: e.mapping }, group);
      }
      // The pasted nodes become the selection once the stream has them.
      pendingSelection = new Set(ids.values());
      setTimeout(() => ids.forEach((id) => moving.delete(id)), 2000);
    } catch (err) {
      fail(ids.size ? `Pasted ${ids.size} of ${copied.nodes.length} nodes` : "Couldn't paste", err);
    }
  }
  let pendingSelection: Set<string> | null = null;

  const nodeTypes = { flwrs: FlowNode };
  const edgeTypes = { flwrs: FlowEdge };
</script>

<svelte:window {onkeydown} {oncopy} {onpaste} />

<div class="layout">
  <Palette {plugins} error={pluginsError} onadd={addAtCenter} />
  <div class="main">
    <DraftBar bind:this={draftBar} {view} {check} onfocus={(id) => flow.fitView({ nodes: [{ id }], padding: 0.6, maxZoom: 1.25, duration: 300 })} onerror={fail} />
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
        <Controls showLock={false}>
          <ControlButton class="help-button" onclick={() => (helpOpen = true)} title="How to work with the editor (?)" aria-label="How to work with the editor">
            <Info size={14} strokeWidth={2} />
          </ControlButton>
        </Controls>
        <MiniMap pannable zoomable />
      </SvelteFlow>
      {#if helpOpen}
        <EditorHelp onclose={() => (helpOpen = false)} />
      {/if}
      {#if editingEdge}
        <EdgeDialog {view} edgeId={editingEdge} onclose={() => (editingEdge = null)} onopennode={(id) => { editingEdge = null; openNode(id); }} />
      {/if}
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

  /* Svelte Flow fills its control buttons' icons; Lucide's are outlines. */
  .canvas :global(.help-button svg) {
    fill: none;
  }
</style>
