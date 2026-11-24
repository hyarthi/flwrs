<script lang="ts">
  // The graph editor's working parts: palette, canvas, the selected node's
  // settings and a JSON tab. Edits stay here, with their own undo, until
  // Save puts them in the scene's draft.
  import {
    Background,
    Controls,
    Panel,
    SvelteFlow,
    useSvelteFlow,
    type Connection,
    type Edge,
    type Node,
  } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import { setContext, untrack } from 'svelte';
  import Undo from '@lucide/svelte/icons/undo-2';
  import Redo from '@lucide/svelte/icons/redo-2';
  import Sparkles from '@lucide/svelte/icons/sparkles';
  import X from '@lucide/svelte/icons/x';
  import GraphFlowNode, { type GraphNodeData } from './GraphFlowNode.svelte';
  import GraphFlowEdge, { type GraphEdgeData } from './GraphFlowEdge.svelte';
  import LiveControls from './LiveControls.svelte';
  import LivePanel from './LivePanel.svelte';
  import { LiveView } from '../../lib/liveView.svelte';
  import { isActive, metricRows, nodeLine, turnsOn } from '../../lib/liveFeed';
  import SchemaField from '../form/SchemaField.svelte';
  import GroupFrames from '../scene/GroupFrames.svelte';
  import GroupLegend from '../scene/GroupLegend.svelte';
  import { safeUploadName } from '../node/uploads';
  import { ApiError } from '../../lib/api';
  import { provideGroupFocus } from '../../lib/groupFocus.svelte';
  import { bubbleOutline, centerOf, dragOutcome, type Frame, type Point as FramePoint, type Rect } from '../../lib/groupFrames';
  import { groupColors, groupsOf, joinGroup, leaveGroup, membersByGroup, nesting, withGroups, type SceneGroup } from '../../lib/groups';
  import {
    History,
    addLink,
    addNode,
    addProblem,
    applyProposal,
    asGraph,
    autoLayout,
    copyNodes,
    filesNamed,
    graphProblems,
    idProblem,
    isCopied,
    linkBadges,
    linkProblem,
    linksOf,
    locate,
    pasteNodes,
    portsOf,
    proposalFor,
    removeLinks,
    removeNodes,
    renameNode,
    setNode,
    withFiles,
    type GraphCatalog,
    type GraphLink,
    type GraphValue,
    type JsonObject,
    type Point,
  } from '../../lib/graphModel';
  import { PLATFORM_CONTEXT, labelOf, resolve, type Schema } from '../../lib/schemaForm';
  import type { Scope } from '../../lib/lua/types';
  import { describeNode, putUpload, setInnerLayout, updateDraftNode, type GraphNode, type SceneView, type Upload } from '../../lib/sceneView';
  import { getPluginSettings } from '../../lib/plugins';
  import { theme } from '../../lib/theme.svelte';

  let {
    view,
    node,
    schema,
    field,
    catalog,
    configuredFields,
    uploads,
    messages,
    active,
    running,
    liveArgs,
    codeScope,
    onopensettings,
    onsaved,
  }: {
    view: SceneView;
    node: GraphNode;
    schema: Schema;
    field: string;
    catalog: GraphCatalog;
    /** The settings the plugin's configurator sets. */
    configuredFields: string[];
    uploads: Upload[];
    messages: string[];
    active: boolean;
    /** The node runs (live, not only in the draft). */
    running: boolean;
    /** The running node's launch args (what its live setting is). */
    liveArgs: Record<string, unknown> | null;
    /** The scope a code setting sees (Lua completion). */
    codeScope?: (schema: Schema) => Scope | null;
    onopensettings: () => void;
    onsaved: (saved: GraphNode) => void;
  } = $props();

  const flow = useSvelteFlow();
  const focus = provideGroupFocus();

  // ---- live view: the running node's numbers and preview ----

  const live = untrack(() => new LiveView(view.scene.id, node.id));
  $effect(() => live.attach());
  $effect(() => live.setContext(running, active && mode === 'canvas'));
  let liveFloating = $state(false);
  const liveSetting = $derived(catalog.live_setting ?? null);
  const liveSettingTitle = $derived(liveSetting ? labelOf(liveSetting, resolve(schema, schema.properties?.[liveSetting] ?? {})) : null);
  /** A node's line on the canvas; a node the stats leave out wasn't measured. */
  function liveLine(id: string): string | undefined {
    const stats = live.stats;
    if (!stats) return undefined;
    return nodeLine(stats.nodes?.[id]) ?? 'not measured';
  }
  /** A link carries data when the node it feeds ran (or, if that one isn't measured, the one feeding it). */
  function linkActive(source: string, target: string): boolean {
    const nodes = live.stats?.nodes;
    if (!nodes) return false;
    return nodes[target] ? isActive(nodes[target]) : isActive(nodes[source]);
  }
  let serverPlatform = $state<string | null>(null);
  getPluginSettings().then((s) => (serverPlatform = s.platform)).catch(() => {});
  setContext(PLATFORM_CONTEXT, () => (node.config.launch === 'remote' ? null : serverPlatform));

  // ---- the graph being edited ----

  const argsOf = (n: GraphNode) => (n.config.launch_args ?? {}) as Record<string, unknown>;
  const storedText = $derived(JSON.stringify(argsOf(node)[field] ?? null));

  let graph = $state.raw<GraphValue>(untrack(() => asGraph(argsOf(node)[field])));
  /** What the draft holds, as far as this editor knows. */
  let baseText = $state(untrack(() => storedText));
  const dirty = $derived(JSON.stringify(graph) !== (baseText === 'null' ? JSON.stringify(asGraph(null)) : baseText));
  let saving = $state(false);
  let changedElsewhere = $state(false);

  const history = new History();
  let canUndo = $state(false);
  let canRedo = $state(false);
  const syncHistory = () => {
    canUndo = history.canUndo;
    canRedo = history.canRedo;
  };
  /** Typing in one setting is one undo step. */
  let coalesce: { key: string; time: number } | null = null;

  function edit(next: GraphValue, key?: string) {
    if (next === graph) return;
    const now = Date.now();
    if (!key || coalesce?.key !== key || now - coalesce.time > 800) history.record(graph);
    coalesce = key ? { key, time: now } : null;
    graph = next;
    saveError = null;
    notice = null;
    syncHistory();
  }

  function step(back: boolean) {
    const value = back ? history.undo(graph) : history.redo(graph);
    if (!value) return;
    coalesce = null;
    graph = value;
    if (selected && !graph.nodes[selected]) selected = null;
    syncHistory();
  }

  // The draft moved under the editor: follow it, unless there are edits here.
  $effect(() => {
    const text = storedText;
    untrack(() => {
      if (text === baseText || saving) return;
      if (dirty) {
        changedElsewhere = true;
        return;
      }
      adopt();
    });
  });

  function adopt() {
    graph = asGraph(argsOf(node)[field]);
    baseText = storedText;
    changedElsewhere = false;
    history.clear();
    detected = new Map();
    if (selected && !graph.nodes[selected]) selected = null;
    syncHistory();
  }

  // ---- layout ----

  let placed = $state.raw<Record<string, Point>>(untrack(() => ({ ...(node.inner_layout?.[field] ?? {}) })));
  const positions = $derived(autoLayout(graph, placed));

  $effect(() => {
    const stored = node.inner_layout?.[field];
    if (stored) untrack(() => (placed = { ...stored, ...moving }));
  });
  /** Moved here and not saved yet: they win over the stream's. */
  const moving: Record<string, Point> = {};

  async function saveLayout(places: Record<string, Point | null>) {
    for (const [id, p] of Object.entries(places)) if (p) moving[id] = p;
    try {
      await setInnerLayout(view.scene.id, node.id, field, places);
    } catch (e) {
      notice = `Couldn’t save where the nodes are: ${e instanceof Error ? e.message : e}`;
    } finally {
      for (const [id, p] of Object.entries(places)) if (p && moving[id] === p) delete moving[id];
    }
  }

  // Unplaced nodes keep where they were put.
  const autoSaved = new Set<string>();
  $effect(() => {
    const all = positions;
    untrack(() => {
      const fresh = Object.entries(all).filter(([id]) => !placed[id] && !autoSaved.has(id));
      if (!fresh.length) return;
      for (const [id] of fresh) autoSaved.add(id);
      placed = { ...placed, ...Object.fromEntries(fresh) };
      saveLayout(Object.fromEntries(fresh));
    });
  });

  // ---- what the canvas shows ----

  const portTitle = (type: string) => catalog.port_types[type]?.title ?? type;
  const groups = $derived((Array.isArray(graph.groups) ? graph.groups : []) as unknown as SceneGroup[]);
  const memberRows = $derived(Object.entries(graph.nodes).map(([id, n]) => ({ id, config: n })));
  const groupMembers = $derived(membersByGroup(memberRows));
  const groupColor = $derived(groupColors(groups.map((g) => g.id)));
  const groupLevels = $derived(nesting(groups, groupMembers));
  let frameFamily = $state<string | null>(untrack(() => catalog.groups?.families[0] ?? null));
  const frameGroups = $derived(groups.filter((g) => g.family === frameFamily).map((g) => ({ id: g.id, height: groupLevels.get(g.id)?.height ?? 0 })));
  const editableGroup = (g: SceneGroup) => !!catalog.groups?.families.includes(g.family);

  /** Selected node, for the settings. */
  let selected = $state<string | null>(null);
  /** Per node: settings filled from a file's proposal, still unreviewed. */
  let detected = $state(new Map<string, Set<string>>());
  let saveError = $state<string[] | null>(null);
  let notice = $state<string | null>(null);

  const local = $derived(graphProblems(catalog, graph));
  // The check speaks about what's saved: while there are edits, only the
  // catalog's own findings count.
  const located = $derived(locate([...(saveError ?? []), ...(dirty ? [] : messages)], field));
  const nodeProblems = (id: string) => [...(local.nodes.get(id) ?? []), ...(located.nodes.get(id) ?? [])];
  const linkProblems = (i: number) => [...(local.links.get(i) ?? []), ...(located.links.get(i) ?? [])];
  const otherProblems = $derived(saveError ? located.other : []);

  let nodes = $state.raw<Node<GraphNodeData>[]>([]);
  let edges = $state.raw<Edge<GraphEdgeData>[]>([]);

  $effect(() => {
    const g = graph;
    const at = positions;
    const colors = groupColor;
    const groupList = groups;
    const marks = detected;
    void [local, located, live.stats];
    untrack(() => {
      const before = new Map(nodes.map((n) => [n.id, n]));
      nodes = Object.entries(g.nodes).map(([id, n]) => {
        const kept = before.get(id);
        const kind = catalog.node_types[String(n.type)];
        const ports = portsOf(catalog, n);
        return {
          ...(kept?.measured && { measured: kept.measured }),
          id,
          type: 'graph',
          position: kept?.dragging ? kept.position : (at[id] ?? { x: 0, y: 0 }),
          selected: kept ? !!kept.selected : selected === id || pastedIds.has(id),
          data: {
            title: id,
            typeTitle: kind?.title ?? String(n.type),
            inputs: ports.inputs,
            outputs: ports.outputs,
            portTitle,
            problems: nodeProblems(id),
            detected: (marks.get(id)?.size ?? 0) > 0,
            chips: groupsOf(n).map((g) => ({ id: g, color: colors.get(g) ?? 1, family: groupList.find((x) => x.id === g)?.family ?? 'unknown' })),
            live: { stats: liveLine(id) },
          },
        };
      });
      const previous = new Map(edges.map((e) => [e.id, e]));
      edges = linksOf(g).flatMap((l, i) => {
        const [source, sourceHandle] = splitEnd(l.from);
        const [target, targetHandle] = splitEnd(l.to);
        if (!g.nodes[source] || !g.nodes[target]) return [];
        const id = `${l.from}>${l.to}`;
        return [{
          id,
          type: 'graph',
          source,
          sourceHandle,
          target,
          targetHandle,
          selected: previous.get(id)?.selected ?? false,
          data: { badges: linkBadges(catalog, g, l), problems: linkProblems(i), active: linkActive(source, target) },
        }];
      });
    });
  });

  function splitEnd(end: string): [string, string] {
    const dot = end.lastIndexOf('.');
    return [end.slice(0, dot), end.slice(dot + 1)];
  }

  let fitted = false;
  $effect(() => {
    if (active && !fitted && nodes.length) {
      fitted = true;
      requestAnimationFrame(() => flow.fitView({ padding: 0.15, maxZoom: 1.1 }));
    }
  });

  const nodeRects = $derived(
    new Map<string, Rect>(nodes.map((n) => [n.id, { x: n.position.x, y: n.position.y, width: n.measured?.width ?? 220, height: n.measured?.height ?? 80 }])),
  );
  let dragging = $state(false);
  let dropTarget = $state<string | null>(null);
  let framesLayer: { current: () => Frame[] } | undefined = $state();
  /** The single node being dragged, and the frames as they were when it began. */
  let drag: { id: string; start: FramePoint; frames: Frame[] } | null = null;

  const canJoinGroups = (id: string) => !!catalog.groups?.node_types.includes(String(graph.nodes[id]?.type));
  const editableId = (id: string) => {
    const g = groups.find((x) => x.id === id);
    return !!g && editableGroup(g);
  };
  const centerOfNode = (n: Node): FramePoint =>
    centerOf({ x: n.position.x, y: n.position.y, width: n.measured?.width ?? 220, height: n.measured?.height ?? 80 });

  /** The group's outline around its members other than this node. */
  function frameWithout(group: string, nodeId: string): Frame | null {
    const others = groupMembers.get(group);
    const members: Rect[] = [];
    const rest: Rect[] = [];
    for (const [id, r] of nodeRects) {
      if (id === nodeId) continue;
      (others?.has(id) ? members : rest).push(r);
    }
    return members.length ? bubbleOutline(group, members, rest, groupLevels.get(group)?.height ?? 0) : null;
  }

  function outcomeOf(id: string, end: FramePoint, modifier: boolean) {
    if (!drag || drag.id !== id || !canJoinGroups(id)) return null;
    const current = groupsOf(graph.nodes[id]);
    const outcome = dragOutcome({
      frames: drag.frames,
      start: drag.start,
      end,
      current,
      editable: editableId,
      size: (g) => groupMembers.get(g)?.size ?? 0,
      modifier,
      without: (g) => frameWithout(g, id),
    });
    // Landing on a frame it can't join is just a move.
    if (outcome && 'join' in outcome && joinGroup(groups, groupMembers, id, current, outcome.join).problem) return null;
    return outcome;
  }

  function dragStart(dragged: Node[]) {
    dragging = true;
    drag = dragged.length === 1 ? { id: dragged[0].id, start: centerOfNode(dragged[0]), frames: framesLayer?.current() ?? [] } : null;
  }

  function dragMove(dragged: Node[], event: MouseEvent | TouchEvent) {
    if (!drag || dragged.length !== 1) return;
    // A modifier-drag never joins, so there's no frame to mark.
    const outcome = event.altKey ? null : outcomeOf(dragged[0].id, centerOfNode(dragged[0]), false);
    dropTarget = outcome && 'join' in outcome ? outcome.join : null;
  }

  // ---- edits on the canvas ----

  const palette = $derived.by(() => {
    const byCategory = new Map<string, [string, GraphCatalog['node_types'][string]][]>();
    for (const entry of Object.entries(catalog.node_types)) {
      const category = entry[1].category ?? 'Other';
      byCategory.set(category, [...(byCategory.get(category) ?? []), entry]);
    }
    return [...byCategory];
  });

  const DRAG_TYPE = 'application/x-flwrs-graph-node';

  function add(type: string, at?: Point) {
    const problem = addProblem(catalog, graph, type);
    if (problem) {
      notice = problem;
      return;
    }
    const { graph: next, id } = addNode(catalog, graph, type);
    nodes = nodes.map((n) => ({ ...n, selected: false }));
    selected = id;
    edit(next);
    if (at) {
      const p = { x: Math.round(at.x), y: Math.round(at.y) };
      placed = { ...placed, [id]: p };
      saveLayout({ [id]: p });
    }
  }

  function ondrop(event: DragEvent) {
    const type = event.dataTransfer?.getData(DRAG_TYPE);
    if (!type) return;
    event.preventDefault();
    const at = flow.screenToFlowPosition({ x: event.clientX, y: event.clientY });
    add(type, { x: at.x - 110, y: at.y - 30 });
  }

  const endOf = (node: string | null | undefined, port: string | null | undefined) => `${node}.${port}`;

  function isValidConnection(c: Edge | Connection): boolean {
    return linkProblem(catalog, graph, endOf(c.source, c.sourceHandle), endOf(c.target, c.targetHandle)) === null;
  }

  function connect(c: Connection) {
    const from = endOf(c.source, c.sourceHandle);
    const to = endOf(c.target, c.targetHandle);
    const problem = linkProblem(catalog, graph, from, to);
    if (problem) notice = problem;
    else edit(addLink(graph, from, to));
  }

  /** A link dropped on a port that refuses it: say why. */
  function connectEnd(_event: unknown, state: { isValid?: boolean | null; fromHandle?: { nodeId: string; id?: string | null; type: string } | null; toHandle?: { nodeId: string; id?: string | null; type: string } | null }) {
    const { fromHandle: a, toHandle: b, isValid } = state;
    if (isValid || !a || !b) return;
    const [out, input] = a.type === 'source' ? [a, b] : [b, a];
    const why = linkProblem(catalog, graph, endOf(out.nodeId, out.id), endOf(input.nodeId, input.id));
    if (why) notice = why;
  }

  function removeSelected(gone: { nodes: Node[]; edges: Edge[] }) {
    let next = graph;
    const ids = gone.nodes.map((n) => n.id);
    if (ids.length) next = removeNodes(next, ids);
    const links = gone.edges
      .map((e) => linksOf(graph).find((l) => `${l.from}>${l.to}` === e.id))
      .filter((l): l is GraphLink => !!l);
    if (links.length) next = removeLinks(next, links);
    if (next !== graph) {
      edit(next);
      if (selected && !next.nodes[selected]) selected = null;
    }
  }

  function dragStop(dragged: Node[], event: MouseEvent | TouchEvent) {
    const outcome = dragged.length === 1 ? outcomeOf(dragged[0].id, centerOfNode(dragged[0]), event.altKey) : null;
    dragging = false;
    dropTarget = null;
    drag = null;
    const places: Record<string, Point> = {};
    for (const n of dragged) places[n.id] = { x: Math.round(n.position.x), y: Math.round(n.position.y) };
    placed = { ...placed, ...places };
    saveLayout(places);
    if (!outcome) return;
    const id = dragged[0].id;
    const row = graph.nodes[id];
    const have = groupsOf(row);
    if ('join' in outcome) {
      const joined = joinGroup(groups, groupMembers, id, have, outcome.join);
      if (joined.problem) notice = joined.problem;
      else edit(setNode(graph, id, withGroups(row, joined.ids) as JsonObject));
    } else {
      edit(setNode(graph, id, withGroups(row, leaveGroup(groups, groupMembers, have, outcome.leave)) as JsonObject));
    }
  }

  // ---- keyboard, copy / paste ----

  /** Keys are the canvas's unless a field has them. */
  function mine(target: EventTarget | null) {
    if (!active) return false;
    const el = target instanceof Element ? target : null;
    return !el?.closest('input, textarea, select, [contenteditable], .cm-editor');
  }

  function onkeydown(e: KeyboardEvent) {
    if (!mine(e.target) || !(e.metaKey || e.ctrlKey) || e.altKey) return;
    const key = e.key.toLowerCase();
    if (key === 'z' || key === 'y') {
      e.preventDefault();
      step(key === 'z' && !e.shiftKey);
    } else if (key === 'd') {
      e.preventDefault();
      const copied = selection();
      if (copied) paste(copied);
    }
  }

  const selection = () => {
    const ids = nodes.filter((n) => n.selected).map((n) => n.id);
    return ids.length ? copyNodes(graph, ids, positions) : null;
  };

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
    let value: unknown = null;
    try {
      value = JSON.parse(e.clipboardData?.getData('text/plain') ?? '');
    } catch {
      return; // not ours
    }
    if (!isCopied(value)) return;
    e.preventDefault();
    paste(value);
  }

  let pasteOffset = 0;
  /** What the last paste made: it becomes the selection. */
  let pastedIds = new Set<string>();
  function paste(copied: ReturnType<typeof copyNodes>) {
    pasteOffset += 48;
    const { graph: next, ids, positions: at, skipped } = pasteNodes(catalog, graph, copied, pasteOffset);
    if (ids.size) {
      pastedIds = new Set(ids.values());
      nodes = nodes.map((n) => ({ ...n, selected: false }));
      placed = { ...placed, ...at };
      edit(next);
      saveLayout(at);
      selected = ids.size === 1 ? [...ids.values()][0] : null;
    }
    if (skipped.length) notice = `Not pasted (the graph has as many as it can take): ${skipped.join(', ')}.`;
  }

  // ---- the selected node's settings ----

  const current = $derived(selected ? graph.nodes[selected] : undefined);
  const kind = $derived(current ? catalog.node_types[String(current.type)] : undefined);
  const settings = $derived(kind ? resolve(schema, { $ref: kind.settings } as Schema) : null);
  const groupable = $derived(!!kind && !!catalog.groups?.node_types.includes(String(current?.type)));
  const hiddenKeys = $derived(new Set(['type', ...(kind?.file_settings ?? []), ...(groupable ? ['groups'] : [])]));
  const settingKeys = $derived(Object.keys(settings?.properties ?? {}).filter((k) => !hiddenKeys.has(k)));

  let idText = $state('');
  $effect(() => {
    idText = selected ?? '';
  });
  const idError = $derived(selected ? idProblem(graph, selected, idText) : null);

  function rename() {
    if (!selected || idError || idText === selected) return;
    const next = idText;
    const old = selected;
    edit(renameNode(graph, old, next));
    const at = positions[old];
    placed = { ...placed, [next]: at };
    saveLayout({ [next]: at, [old]: null });
    if (detected.has(old)) {
      const marks = new Map(detected);
      marks.set(next, marks.get(old)!);
      marks.delete(old);
      detected = marks;
    }
    selected = next;
  }

  function clearMark(id: string, key: string) {
    if (!detected.get(id)?.has(key)) return;
    const marks = new Map(detected);
    const left = new Set(marks.get(id));
    left.delete(key);
    if (left.size) marks.set(id, left);
    else marks.delete(id);
    detected = marks;
  }

  function setSetting(key: string, value: unknown) {
    if (!selected || !current) return;
    const n = { ...current };
    if (value === undefined) delete n[key];
    else n[key] = value;
    clearMark(selected, key);
    edit(setNode(graph, selected, n), `${selected}:${key}`);
  }

  function toggleGroup(id: string, on: boolean) {
    if (!selected || !current) return;
    const have = groupsOf(current);
    let ids: string[];
    if (on) {
      const joined = joinGroup(groups, groupMembers, selected, have, id);
      if (joined.problem) {
        notice = joined.problem;
        return;
      }
      ids = joined.ids;
    } else {
      ids = leaveGroup(groups, groupMembers, have, id);
    }
    edit(setNode(graph, selected, withGroups(current, ids) as JsonObject));
  }

  function setGroups(value: unknown) {
    const next = { ...graph };
    if (value === undefined) delete next.groups;
    else next.groups = value as JsonObject[];
    edit(next, 'groups');
  }

  // ---- files ----

  /** Files uploaded here, until the stream lists them. */
  let extraFiles = $state<string[]>([]);
  const fileNames = $derived([...new Set([...uploads.map((u) => u.name), ...extraFiles])]);
  let uploading = $state(false);

  async function pickFile(id: string, key: string, name: string, justUploaded = false) {
    const before = graph.nodes[id];
    if (!before) return;
    const set = { ...before };
    if (name) set[key] = name;
    else delete set[key];
    clearMark(id, key);
    edit(setNode(graph, id, set));
    const nodeKind = catalog.node_types[String(before.type)];
    if (!name || nodeKind?.proposal?.file_setting !== key) return;
    const listField = catalog.files_field;
    try {
      const { values } = await describeNode(view.scene.id, node.id, justUploaded || !!view.draft, listField ? { [listField]: [name] } : {});
      const now = graph.nodes[id];
      // The choice moved on while it was being read.
      if (!now || now[key] !== name) return;
      const { node: filled, filled: keys } = applyProposal(now, proposalFor(nodeKind, values, name));
      if (keys.length) {
        edit(setNode(graph, id, filled));
        detected = new Map(detected).set(id, new Set([...(detected.get(id) ?? []), ...keys]));
      }
    } catch (e) {
      notice = `Couldn’t read ${name}: ${e instanceof Error ? e.message : e}`;
    }
  }

  async function uploadFor(id: string, key: string, file: File | undefined) {
    if (!file) return;
    uploading = true;
    try {
      const saved = await putUpload(view.scene.id, node.id, safeUploadName(file.name), file);
      extraFiles = [...extraFiles, saved.name];
      await pickFile(id, key, saved.name, true);
    } catch (e) {
      notice = `Couldn’t upload ${file.name}: ${e instanceof Error ? e.message : e}`;
    } finally {
      uploading = false;
    }
  }

  // ---- save, revert, JSON ----

  async function save() {
    saving = true;
    saveError = null;
    notice = null;
    const args = { ...argsOf(node), [field]: graph };
    // Left out = the server keeps or recomputes them. What this view holds may
    // be from before a run that cleared them, which it would refuse.
    if (node.config.launch !== 'remote' || node.config.configurator) for (const key of configuredFields) delete args[key];
    if (catalog.files_field) {
      const files = withFiles(args[catalog.files_field], filesNamed(catalog, graph));
      if (files.length) args[catalog.files_field] = files;
    }
    try {
      const saved = await updateDraftNode(view.scene.id, node.id, {
        name: node.name,
        plugin_id: node.plugin_id,
        config: { ...node.config, launch_args: args },
      });
      const stored = (saved.config.launch_args as Record<string, unknown> | undefined)?.[field];
      graph = asGraph(stored);
      baseText = JSON.stringify(stored ?? null);
      changedElsewhere = false;
      onsaved(saved);
      // Positions of nodes that are gone aren't needed any more.
      const stale = Object.keys(placed).filter((id) => !graph.nodes[id]);
      if (stale.length) {
        placed = Object.fromEntries(Object.entries(placed).filter(([id]) => graph.nodes[id]));
        saveLayout(Object.fromEntries(stale.map((id) => [id, null])));
      }
      const unfinished = graphProblems(catalog, graph);
      notice = unfinished.nodes.size || unfinished.links.size ? 'Saved to the draft, unfinished: it can’t run until the marked problems are fixed.' : 'Saved to the draft.';
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      saveError = error.details.length ? error.details : [error.message];
    } finally {
      saving = false;
    }
  }

  function revert() {
    if (changedElsewhere || dirty) adopt();
    saveError = null;
  }

  let mode = $state<'canvas' | 'json'>('canvas');
  let jsonText = $state('');
  let jsonError = $state<string | null>(null);

  function showJson() {
    jsonText = JSON.stringify(graph, null, 2);
    jsonError = null;
    mode = 'json';
  }

  function jsonInput(text: string) {
    jsonText = text;
    try {
      const value = JSON.parse(text);
      if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('the graph is a JSON object');
      jsonError = null;
      edit(asGraph(value), 'json');
    } catch (e) {
      jsonError = e instanceof Error ? e.message : String(e);
    }
  }

  const nodeTypes = { graph: GraphFlowNode };
  const edgeTypes = { graph: GraphFlowEdge };
</script>

<svelte:window {onkeydown} {oncopy} {onpaste} />

<div class="editor">
  <div class="bar">
    <div class="modes" role="tablist">
      <button type="button" role="tab" aria-selected={mode === 'canvas'} class:on={mode === 'canvas'} onclick={() => (mode = 'canvas')}>Canvas</button>
      <button type="button" role="tab" aria-selected={mode === 'json'} class:on={mode === 'json'} onclick={showJson}>JSON</button>
    </div>
    <button class="btn ghost icon" type="button" title="Undo (⌘Z)" aria-label="Undo" disabled={!canUndo} onclick={() => step(true)}><Undo size={16} strokeWidth={1.75} /></button>
    <button class="btn ghost icon" type="button" title="Redo (⇧⌘Z)" aria-label="Redo" disabled={!canRedo} onclick={() => step(false)}><Redo size={16} strokeWidth={1.75} /></button>
    {#if notice}
      <span class="note" role="status">{notice}<button class="btn ghost icon" type="button" aria-label="Dismiss" onclick={() => (notice = null)}><X size={12} /></button></span>
    {/if}
    <LiveControls {live} {running} settingTitle={liveSettingTitle} settingOn={!!liveSetting && turnsOn(liveArgs?.[liveSetting])} {onopensettings} />
    <span class="spacer"></span>
    {#if changedElsewhere}<span class="badge warn">changed elsewhere</span>{/if}
    {#if dirty}<span class="badge accent">unsaved</span>{/if}
    <button class="btn ghost" type="button" disabled={!dirty && !changedElsewhere} onclick={revert}>Revert</button>
    <button class="btn primary" type="button" disabled={!dirty || saving} onclick={save}>{saving ? 'Saving…' : 'Save to draft'}</button>
  </div>

  {#if saveError}
    <div class="notice danger" role="alert">
      Not saved.
      {#if otherProblems.length}<ul>{#each otherProblems as p, i (i)}<li>{p}</li>{/each}</ul>{/if}
      {#if located.nodes.size || located.links.size}<span class="muted"> The marked nodes and links say why.</span>{/if}
    </div>
  {/if}

  {#if mode === 'json'}
    <div class="json">
      <textarea class="input mono" spellcheck="false" aria-label="The graph as JSON" value={jsonText} oninput={(e) => jsonInput(e.currentTarget.value)}></textarea>
      {#if jsonError}<p class="error">{jsonError}</p>{/if}
    </div>
  {:else}
    <div class="work">
      <aside class="palette" aria-label="Node types">
        {#each palette as [category, entries] (category)}
          <h4>{category}</h4>
          {#each entries as [type, entry] (type)}
            <button
              type="button"
              class="entry"
              draggable="true"
              title={entry.description}
              disabled={addProblem(catalog, graph, type) !== null}
              ondragstart={(e) => e.dataTransfer?.setData(DRAG_TYPE, type)}
              onclick={() => add(type)}
            >{entry.title}</button>
          {/each}
        {/each}
      </aside>

      <div
        class="canvas"
        class:focusing={focus.id !== null}
        role="application"
        aria-label="Graph canvas"
        ondragover={(e) => {
          if (e.dataTransfer?.types.includes(DRAG_TYPE)) {
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
          onconnectend={connectEnd}
          onbeforedelete={async (gone) => {
            removeSelected(gone);
            return false;
          }}
          onnodeclick={({ node: n }) => (selected = n.id)}
          onpaneclick={() => (selected = null)}
          onnodedragstart={({ nodes: dragged }) => dragStart(dragged)}
          onnodedrag={({ nodes: dragged, event }) => dragMove(dragged, event)}
          onnodedragstop={({ nodes: dragged, event }) => dragStop(dragged, event)}
          proOptions={{ hideAttribution: true }}
        >
          <Background gap={16} />
          <GroupFrames bind:this={framesLayer} groups={frameGroups} members={groupMembers} rects={nodeRects} colors={groupColor} {dragging} {dropTarget} focused={focus.id} />
          {#if groups.length}
            <Panel position="top-left">
              <GroupLegend {groups} members={groupMembers} colors={groupColor} problems={[]} editable={editableGroup} bind:family={frameFamily} />
            </Panel>
          {/if}
          <Controls showLock={false} />
        </SvelteFlow>
        {#if Object.keys(graph.nodes).length === 0}
          <div class="empty muted">Add a node from the list on the left.</div>
        {/if}
      </div>

      <aside class="inspector" aria-label="Node settings">
        <LivePanel {live} {running} bind:floating={liveFloating} />
        {#if current && selected && kind && settings}
          {@const nodeId = selected}
          <h3>{kind.title}</h3>
          {#if kind.description}<p class="hint">{kind.description}</p>{/if}
          {#each nodeProblems(nodeId) as p, i (i)}<p class="error">{p}</p>{/each}
          <label class="field">
            Id
            <input class="input mono" bind:value={idText} onblur={rename} onkeydown={(e) => e.key === 'Enter' && rename()} />
            {#if idError}<span class="error">{idError}</span>{/if}
          </label>
          {#each kind.file_settings ?? [] as key (key)}
            {@const value = typeof current[key] === 'string' ? (current[key] as string) : ''}
            <div class="field">
              <label for={`file-${key}`}>{labelOf(key, resolve(schema, settings.properties?.[key] ?? {}))}</label>
              <div class="row">
                <select id={`file-${key}`} class="input grow" {value} onchange={(e) => pickFile(nodeId, key, e.currentTarget.value)}>
                  <option value="">Pick an uploaded file</option>
                  {#each fileNames as name (name)}<option value={name}>{name}</option>{/each}
                  {#if value && !fileNames.includes(value)}<option {value}>{value} (missing)</option>{/if}
                </select>
                <label class="btn">
                  {uploading ? 'Uploading…' : 'Upload…'}
                  <input type="file" hidden disabled={uploading} onchange={(e) => uploadFor(nodeId, key, e.currentTarget.files?.[0])} />
                </label>
              </div>
            </div>
          {/each}
          {#if groupable && groups.length}
            <div class="field">
              Groups
              {#each groups as g (g.id)}
                <label class="check">
                  <input type="checkbox" checked={groupsOf(current).includes(g.id)} onchange={(e) => toggleGroup(g.id, e.currentTarget.checked)} />
                  <span class="mono">{g.id}</span> <span class="muted small">{g.family}</span>
                </label>
              {/each}
            </div>
          {/if}
          {#each settingKeys as key (`${nodeId}:${key}`)}
            {#if detected.get(nodeId)?.has(key)}
              <span class="detected"><Sparkles size={12} strokeWidth={2} /> Detected from the file: review it</span>
            {/if}
            <SchemaField
              root={schema}
              schema={resolve(schema, settings.properties![key])}
              name={key}
              value={current[key]}
              required={settings.required?.includes(key) ?? false}
              {codeScope}
              onchange={(v) => setSetting(key, v)}
            />
          {/each}
          {#if live.statsWanted && live.stats}
            {@const rows = metricRows(live.stats.nodes?.[nodeId] as Record<string, unknown> | undefined)}
            <div class="field" data-testid="node-live">
              <span>Live</span>
              {#if rows.length}
                <dl class="live-rows">{#each rows as row (row.label)}<dt>{row.label}</dt><dd class="mono">{row.value}</dd>{/each}</dl>
              {:else}
                <span class="hint">Not measured: the node reports no numbers for it.</span>
              {/if}
            </div>
          {/if}
        {:else}
          <h3>Graph</h3>
          <p class="hint">Select a node to edit it. Drag between ports to link them.</p>
          {#if catalog.groups}
            {@const groupsSchema = (schema.properties?.[field]?.properties as Record<string, Schema> | undefined)?.groups}
            {#if groupsSchema}
              <SchemaField root={schema} schema={groupsSchema} name="groups" label="Groups" value={graph.groups} onchange={setGroups} />
            {/if}
          {/if}
        {/if}
      </aside>
    </div>
  {/if}
</div>

<style>
  .editor {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: var(--space-2);
    min-height: 0;
  }

  .bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }

  .spacer {
    flex: 1;
  }

  .modes {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    overflow: hidden;
  }

  .modes button {
    padding: var(--space-1) var(--space-2);
    border: 0;
    background: none;
    color: var(--text-muted);
    font: inherit;
    font-size: var(--text-sm);
    cursor: pointer;
  }

  .modes button.on {
    background: var(--accent-soft);
    color: var(--text);
  }

  .note {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    color: var(--text-muted);
    font-size: var(--text-sm);
  }

  .work {
    position: relative;
    display: flex;
    flex: 1;
    min-height: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    overflow: hidden;
  }

  .palette {
    display: flex;
    flex: none;
    flex-direction: column;
    gap: 2px;
    width: 160px;
    padding: var(--space-2);
    border-right: 1px solid var(--border);
    background: var(--surface);
    overflow-y: auto;
  }

  .palette h4 {
    margin: var(--space-2) 0 var(--space-1);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 600;
  }

  .entry {
    padding: var(--space-1) var(--space-2);
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text);
    font: inherit;
    font-size: var(--text-sm);
    text-align: left;
    cursor: grab;
  }

  .entry:hover:not(:disabled),
  .entry:focus-visible {
    background: var(--surface-hover);
  }

  .entry:disabled {
    color: var(--text-muted);
    cursor: not-allowed;
  }

  .canvas {
    position: relative;
    flex: 1;
    min-width: 0;
  }

  .canvas :global(.svelte-flow) {
    --xy-background-color: var(--bg);
    --xy-background-pattern-dots-color: var(--border-strong);
    --xy-edge-stroke: var(--border-strong);
    --xy-edge-stroke-selected: var(--accent);
    --xy-connectionline-stroke: var(--accent);
    --xy-selection-background-color: var(--accent-soft);
    --xy-selection-border: 1px dashed var(--accent);
    --xy-controls-button-background-color: var(--surface);
    --xy-controls-button-background-color-hover: var(--surface-hover);
    --xy-controls-button-color: var(--text);
    --xy-controls-button-color-hover: var(--text);
    --xy-controls-button-border-color: var(--border);
    --xy-controls-box-shadow: var(--shadow-sm);
    font-family: var(--font-sans);
  }

  .canvas :global(.svelte-flow__node-graph) {
    padding: 0;
    border: 0;
    background: none;
  }

  .canvas.focusing :global(.svelte-flow__edge) {
    opacity: 0.2;
  }

  .empty {
    position: absolute;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    pointer-events: none;
  }

  .inspector {
    display: flex;
    flex: none;
    flex-direction: column;
    gap: var(--space-2);
    width: 340px;
    padding: var(--space-3);
    border-left: 1px solid var(--border);
    background: var(--surface);
    overflow-y: auto;
  }

  .inspector h3 {
    margin: 0;
    font-size: var(--text-md, 1rem);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--text-sm);
  }

  .row {
    display: flex;
    gap: var(--space-2);
  }

  .grow {
    flex: 1;
    min-width: 0;
  }

  .check {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }

  .live-rows {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 2px var(--space-2);
    margin: 0;
    font-size: var(--text-xs);
  }

  .live-rows dt {
    color: var(--text-muted);
  }

  .live-rows dd {
    margin: 0;
    text-align: right;
  }

  .hint,
  .small {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .error {
    margin: 0;
    color: var(--danger);
    font-size: var(--text-sm);
  }

  .detected {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    color: var(--accent);
    font-size: var(--text-xs);
  }

  .json {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: var(--space-1);
    min-height: 0;
  }

  .json textarea {
    flex: 1;
    resize: none;
  }

  ul {
    margin: 0;
    padding-left: var(--space-4);
  }
</style>
