// One scene in the management API: what the scene view shows (its stream's
// events) and the draft calls the Layout tab makes. Every edit goes to the
// scene's draft (started on the first edit); submit commits it to live.

import { api } from './api';
import type { NodeDevice, NodeThreads, SceneGroup } from './groups';
import type { SceneState } from './scenes';

export type NodeType = 'SOURCE' | 'TRANSFORM' | 'SINK';
export type Change = 'unchanged' | 'added' | 'changed' | 'removed';
export type Conflict = 'changed_both' | 'removed_live' | 'changed_live';

export interface Position {
  x: number;
  y: number;
}

export interface SceneDefaults {
  durable?: boolean;
  remote_auth?: 'token' | 'mtls';
  require_tls?: boolean;
}

export interface Scene {
  id: string;
  name: string;
  enabled: boolean;
  max_hops: number | null;
  defaults: SceneDefaults;
  /** Left out by servers without groups. */
  groups?: SceneGroup[];
  revision: number;
  create_time: number;
  update_time: number;
}

/** A node (`Node` on the server). */
export interface GraphNode {
  id: string;
  scene_id: string;
  name: string | null;
  type: NodeType;
  plugin_id: string;
  plugin_version: string | null;
  config: Record<string, unknown>;
  has_token: boolean;
  launch_args_secrets: Record<string, boolean>;
  position: Position | null;
  /** Where the inner nodes of its graph settings sit: setting → inner node → position. */
  inner_layout?: Record<string, Record<string, Position>>;
  create_time: number;
  update_time: number;
}

/** An edge mapping (`MappingSpec` on the server). */
export interface Mapping {
  mode?: 'merge' | 'replace';
  /** Lua returning whether the event goes on. */
  filter?: { lua: string };
  rules?: MappingRule[];
  /** Lua returning the whole output (instead of rules). */
  script?: { lua: string };
  /** Limits for the mapping's Lua. */
  lua?: { max_instructions?: number; timeout_ms?: number; max_memory_bytes?: number };
}

/** One rule: a `target` path and exactly one of source / value / lua / delete (copies may decode or encode). */
export interface MappingRule {
  target: string;
  source?: string;
  value?: unknown;
  lua?: string;
  delete?: boolean;
  decode?: 'json';
  encode?: 'json';
}

/** A payload schema field, as the mapping test returns it. */
export interface SchemaField {
  key: string;
  type: string;
  description?: string;
  content_type?: string;
  element?: SchemaField;
  fields?: SchemaField[];
}

export type TestOutcome =
  | { outcome: 'output'; payload: Record<string, unknown> }
  | { outcome: 'filtered' }
  | { outcome: 'error'; message: string };

export interface MappingTest {
  source: SchemaField[] | null;
  target: SchemaField[] | null;
  problems: string[];
  result: TestOutcome | null;
}

/** An edge (`Edge` on the server). */
export interface GraphEdge {
  id: string;
  scene_id: string;
  name: string | null;
  source_node_id: string;
  target_node_id: string;
  mapping: Mapping | null;
  /** Where the edge's box sits (the point it passes through); `null` = routed by the editor. */
  via: Position | null;
  create_time: number;
  update_time: number;
}

export type WhenFull = 'block' | 'drop_newest' | 'drop_oldest';

/** A receiving node's inbound queue, as the director sets it up. */
export interface NodeQueue {
  kind: 'memory' | 'spill' | 'durable';
  when_full: WhenFull;
}

export interface DraftNode extends GraphNode {
  change: Change;
  conflict: Conflict | null;
  /** What differs from live: `name`, `plugin`, `config.<key>`, `uploads`. */
  parts: string[];
  uploads: Upload[];
}

export interface DraftEdge extends GraphEdge {
  change: Change;
  conflict: Conflict | null;
  parts: string[];
}

export interface Draft {
  scene_id: string;
  base_revision: number;
  live_revision: number;
  behind: boolean;
  has_changes: boolean;
  settings: {
    name: string;
    max_hops: number | null;
    defaults: SceneDefaults;
    groups?: SceneGroup[];
    change: Change;
    conflict: Conflict | null;
  };
  nodes: DraftNode[];
  edges: DraftEdge[];
  conflicts: string[];
  /** What undo would take back / redo make again; `null` = nothing. */
  undo: string | null;
  redo: string | null;
  /** A submit its pre-flight check held back; live is unchanged. */
  submit: DraftSubmit | null;
  create_time: number;
  update_time: number;
}

export interface DraftSubmit {
  state: 'WAITING' | 'FAILED';
  message: string | null;
  problems: PreflightIssue[];
  submit_time: number | null;
  retry: { attempt: number; max_attempts: number; next_attempt_time: number | null } | null;
}

export interface Check {
  launchable: boolean;
  problems: string[];
  warnings: string[];
  /** By node id: the core count each local node that takes one gets, and from where. */
  threads?: Record<string, NodeThreads>;
  /** By node id: the devices each local node claims. */
  devices?: Record<string, NodeDevice[]>;
}

/** The `scene` event. */
export interface SceneView {
  scene: Scene;
  nodes: GraphNode[];
  edges: GraphEdge[];
  check: Check;
  draft: Draft | null;
  draft_check: Check | null;
  /** Receiving nodes' queues (live graph), by node id. */
  queues: Record<string, NodeQueue>;
  draft_queues: Record<string, NodeQueue> | null;
}

export type NodeState =
  | 'LAUNCHING'
  | 'WAITING'
  | 'RUNNING'
  | 'DISCONNECTED'
  | 'UNHEALTHY'
  | 'STOPPING'
  | 'STOPPED'
  | 'FAILED';

export interface NodeReport {
  node_id: string;
  plugin_id: string;
  plugin_version: string;
  status: { state: NodeState; launches: number; failed_launches: number; pid: number | null; last_error: string | null };
  counters: { emitted: number; received: number; dropped: number; mapping_errors: number; restarts: number };
  queue: QueueStats | null;
  dropped_hops: number;
  mapping_errors: number;
  link: {
    heartbeats: { last_rtt_ms: number | null; answered: number; missed: number; unexpected: number };
    connections: {
      accepted: number;
      rejected: number;
      replaced: number;
      closed_by_plugin: number;
      timed_out: number;
      io_errors: number;
      protocol_violations: number;
    };
  };
}

export interface QueueStats {
  len: number;
  bytes: number;
  queued: number;
  delivered: number;
  dropped_oversized: number;
  dropped_full: number;
  dropped_timeout: number;
  dropped_budget: number;
  evicted: number;
  spill: boolean;
  durable: boolean;
  spilling: boolean;
  disk_len: number;
  disk_bytes: number;
  dropped_disk_error: number;
  blocked_ms: number;
  in_flight: number;
  redelivered: number;
  nacked: number;
  dead_lettered: number;
}

/** The `status` event. */
export interface SceneStatusEvent {
  time: number;
  state: SceneState;
  errors: string[];
  max_hops: number | null;
  nodes: NodeReport[];
  /** The last pre-flight check's problems (`WAITING`, `FAILED`). */
  preflight: PreflightIssue[];
  /** Where the retries stand (`WAITING`). */
  preflight_retry: { attempt: number; max_attempts: number; next_attempt_time: number | null } | null;
  /** An update the running scene couldn't take: it keeps its previous graph. */
  failed_update: { errors: string[]; problems: PreflightIssue[]; fail_time: number } | null;
}

export interface PreflightIssue {
  node_id: string;
  message: string;
  retryable: boolean;
}

/** `POST …/preflight`: what the nodes' checks found on this machine. */
export interface PreflightRun {
  ok: boolean;
  permanent: boolean;
  nodes: {
    node_id: string;
    plugin_id: string;
    checked: boolean;
    skipped: string | null;
    problems: { message: string; retryable: boolean }[];
    warnings: string[];
  }[];
}

/** Runs the scene's pre-flight checks (of its draft, if asked); starts nothing. */
export function runPreflight(sceneId: string, draft: boolean): Promise<PreflightRun> {
  const query = draft ? '?draft=true' : '';
  return api<PreflightRun>(`/scenes/by-id/${encodeURIComponent(sceneId)}/preflight${query}`, { method: 'POST' });
}

/** A registered plugin (`PluginInfo`). */
export interface Plugin {
  id: string;
  name: string;
  version: string;
  type: NodeType;
  description: string | null;
  ui: { icon: boolean; categories: string[]; color: string | null; docs: boolean } | null;
  /** Launch args the plugin's configurator sets (local nodes). */
  configured_fields: string[];
  /** Whether a node's `runtime.threads` reaches the plugin. */
  takes_threads: boolean;
  /**
   * What a node gets for each run-time setting it leaves out, shaped like a
   * node's `runtime` (manifest, else built-in, else the server's). Absent:
   * `queue.when_full` (block; drop newest on a cycle), `queue.durable` (the
   * scene's default, else off), `max_restarts` (5 until a critical or just-updated node first runs, otherwise no limit; -1 = try forever), `threads` (the
   * plugin decides).
   */
  node_defaults: Record<string, unknown>;
  has_launch_args_schema: boolean;
  /** A remote plugin definition: nothing runs here, its nodes are remote. */
  remote_only: boolean;
  installed: boolean;
}

export interface Upload {
  name: string;
  sha256: string;
  size: number;
  create_time: number;
}

const scene = (id: string) => `/scenes/by-id/${encodeURIComponent(id)}`;
const draft = (id: string) => `${scene(id)}/draft`;
const part = (s: string) => encodeURIComponent(s);

export const listPlugins = () => api<Plugin[]>('/plugins');
export const pluginIconUrl = (id: string) => `/api/plugins/by-id/${part(id)}/ui/icon`;

/** A package file the launch-args schema names (text), cached per plugin version; `null` if it can't be had. */
const schemaFileCache = new Map<string, Promise<string | null>>();
function getSchemaFile(kind: 'definitions' | 'templates' | 'graph-catalogs', pluginId: string, version: string, path: string): Promise<string | null> {
  const key = `${kind}:${pluginId}@${version}:${path}`;
  if (!schemaFileCache.has(key)) {
    const url = `/api/plugins/by-id/${part(pluginId)}/${kind}/${path.split('/').map(part).join('/')}`;
    schemaFileCache.set(
      key,
      fetch(url, { credentials: 'same-origin' })
        .then((r) => (r.ok ? r.text() : null))
        .catch(() => null),
    );
  }
  return schemaFileCache.get(key)!;
}

/** A code field's LuaCATS definitions file (`x-flwrs-code` `definitions`). */
export const getDefinitions = (pluginId: string, version: string, path: string) => getSchemaFile('definitions', pluginId, version, path);

/** A field's template (`x-flwrs-template`): what an empty field is filled with. */
export const getTemplate = (pluginId: string, version: string, path: string) => getSchemaFile('templates', pluginId, version, path);

/** A graph setting's catalog (`x-flwrs-graph`), as text. */
export const getGraphCatalog = (pluginId: string, version: string, path: string) => getSchemaFile('graph-catalogs', pluginId, version, path);

/** The payload schemas a node of the plugin has with these launch args. */
export const getNodeSchemas = (pluginId: string, launchArgs: unknown) =>
  api<{ in: SchemaField[] | null; out: SchemaField[] | null }>(`/plugins/by-id/${part(pluginId)}/node-schemas`, {
    method: 'POST',
    json: { launch_args: launchArgs ?? {} },
  });

export const getPlugin = (id: string) => api<Plugin>(`/plugins/by-id/${part(id)}`);
export const getLaunchArgsSchema = (id: string) => api<Record<string, unknown>>(`/plugins/by-id/${part(id)}/launch-args-schema`);

/** Replaces a node in the draft (its secrets: left out = kept, `null` = cleared). */
export const updateDraftNode = (
  id: string,
  nodeId: string,
  body: { name: string | null; plugin_id: string; config: Record<string, unknown> },
  group?: UndoGroup,
) => api<GraphNode>(`${draft(id)}/nodes/by-id/${part(nodeId)}`, { method: 'PUT', json: body, headers: undoHeaders(group) });

/** A node's uploads: the draft's (if the scene has one) or live's. */
export const listUploads = (id: string, nodeId: string, inDraft: boolean) =>
  api<Upload[]>(`${inDraft ? draft(id) : scene(id)}/nodes/by-id/${part(nodeId)}/uploads`);
export const putUpload = (id: string, nodeId: string, name: string, file: Blob) =>
  api<Upload>(`${draft(id)}/nodes/by-id/${part(nodeId)}/uploads/${part(name)}`, {
    method: 'PUT',
    body: file,
    headers: { 'Content-Type': 'application/octet-stream' },
  });
export const deleteUpload = (id: string, nodeId: string, name: string) =>
  api<null>(`${draft(id)}/nodes/by-id/${part(nodeId)}/uploads/${part(name)}`, { method: 'DELETE' });
export const uploadContentUrl = (id: string, nodeId: string, name: string, inDraft: boolean) =>
  `/api${inDraft ? draft(id) : scene(id)}/nodes/by-id/${part(nodeId)}/uploads/${part(name)}/content`;

/** Requests in one undo group are one undo step (named by `label`). */
export interface UndoGroup {
  group: string;
  label?: string;
}
const undoHeaders = (g?: UndoGroup): Record<string, string> =>
  g ? { 'X-Flwrs-Undo-Group': g.group, ...(g.label ? { 'X-Flwrs-Undo-Label': g.label } : {}) } : {};

export const createDraftNode = (
  id: string,
  body: { plugin_id: string; name?: string | null; position: Position; config?: Record<string, unknown> },
  group?: UndoGroup,
) => api<GraphNode>(`${draft(id)}/nodes`, { method: 'POST', json: body, headers: undoHeaders(group) });

export const undoDraft = (id: string) => api<{ step: string; draft: Draft }>(`${draft(id)}/undo`, { method: 'POST' });
export const redoDraft = (id: string) => api<{ step: string; draft: Draft }>(`${draft(id)}/redo`, { method: 'POST' });
/** Runs a draft node's configurator again (on its settings as saved). */
export const configureDraftNode = (id: string, nodeId: string) =>
  api<GraphNode>(`${draft(id)}/nodes/by-id/${part(nodeId)}/configure`, { method: 'POST' });

export const removeDraftNode = (id: string, nodeId: string) =>
  api<null>(`${draft(id)}/nodes/by-id/${part(nodeId)}`, { method: 'DELETE' });
export const restoreDraftNode = (id: string, nodeId: string) =>
  api<unknown>(`${draft(id)}/nodes/by-id/${part(nodeId)}/restore`, { method: 'POST' });

export const createDraftEdge = (
  id: string,
  source: string,
  target: string,
  extra: { name?: string | null; mapping?: Mapping | null } = {},
  group?: UndoGroup,
) =>
  api<GraphEdge>(`${draft(id)}/edges`, {
    method: 'POST',
    json: { source_node_id: source, target_node_id: target, ...extra },
    headers: undoHeaders(group),
  });
export const removeDraftEdge = (id: string, edgeId: string) =>
  api<null>(`${draft(id)}/edges/by-id/${part(edgeId)}`, { method: 'DELETE' });
export const restoreDraftEdge = (id: string, edgeId: string) =>
  api<unknown>(`${draft(id)}/edges/by-id/${part(edgeId)}/restore`, { method: 'POST' });

/** The scene's settings in the draft; `groups` left out = kept. */
export const updateDraftSettings = (
  id: string,
  body: { name: string; max_hops: number | null; defaults: SceneDefaults; groups?: SceneGroup[] },
  group?: UndoGroup,
) => api<Draft>(`${draft(id)}/settings`, { method: 'PUT', json: body, headers: undoHeaders(group) });
export const discardDraft = (id: string) => api<null>(draft(id), { method: 'DELETE' });
export const rebaseDraft = (id: string) =>
  api<{ taken_live: string[]; conflicts: string[]; dropped_edges: string[] }>(`${draft(id)}/rebase`, { method: 'POST' });
export const resolveConflict = (id: string, target: 'settings' | 'node' | 'edge', rowId: string | null, keep: 'draft' | 'live') =>
  api<unknown>(`${draft(id)}/resolve`, { method: 'POST', json: { target, id: rowId, keep } });
export const submitDraft = (id: string, baseRevision: number, disable: boolean) =>
  api<Scene>(`${draft(id)}/submit`, { method: 'POST', json: { base_revision: baseRevision, disable } });

/** Places nodes and edge boxes (live and draft alike; layout isn't a change). */
export const setLayout = (id: string, nodes: Record<string, Position | null>, edges: Record<string, Position | null> = {}) =>
  api<null>(`${scene(id)}/layout`, { method: 'PATCH', json: { nodes, edges } });

/** Places a graph setting's inner nodes (`null` forgets one). Layout: nothing else changes. */
export const setInnerLayout = (id: string, nodeId: string, field: string, places: Record<string, Position | null>) =>
  api<null>(`${scene(id)}/layout`, { method: 'PATCH', json: { inner: { [nodeId]: { [field]: places } } } });

/** What a node's configurator says about the uploads these launch args name; nothing is stored. */
export const describeNode = (id: string, nodeId: string, draft: boolean, launchArgs: Record<string, unknown>) =>
  api<{ values: Record<string, unknown> | null }>(`${scene(id)}/nodes/by-id/${part(nodeId)}/describe?draft=${draft}`, {
    method: 'POST',
    json: { launch_args: launchArgs },
  });

/** Replaces an edge in the draft. */
export const updateDraftEdge = (
  id: string,
  edgeId: string,
  body: { name: string | null; source_node_id: string; target_node_id: string; mapping: Mapping | null },
) => api<GraphEdge>(`${draft(id)}/edges/by-id/${part(edgeId)}`, { method: 'PUT', json: body });

/** Tries a mapping between two nodes (the draft's, else live's) on a sample payload. */
export const testMapping = (
  id: string,
  body: { source_node_id: string; target_node_id: string; mapping: Mapping | null; payload?: Record<string, unknown> },
) => api<MappingTest>(`${scene(id)}/mapping/test`, { method: 'POST', json: body });

/** Drops every queued event of the scene's nodes, or of one node. */
export const purgeSpool = (id: string, nodeId?: string) =>
  api<{ purged_events: number }>(nodeId ? `${scene(id)}/nodes/by-id/${part(nodeId)}/spool` : `${scene(id)}/spool`, { method: 'DELETE' });

/** A check's messages that name a node (`node <id>`) or an edge's ends. */
export function messagesFor(messages: string[], nodeId: string): string[] {
  return messages.filter((m) => m.includes(`node ${nodeId}`));
}

export function edgeMessages(messages: string[], source: string, target: string): string[] {
  return messages.filter((m) => m.startsWith(`edge ${source} → ${target}`));
}
