// One scene in the management API: what the scene view shows (its stream's
// events) and the draft calls the Layout tab makes. Every edit goes to the
// scene's draft (started on the first edit); submit commits it to live.

import { api } from './api';
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
  create_time: number;
  update_time: number;
}

export interface Mapping {
  mode?: string;
  filter?: unknown;
  rules?: unknown[];
  script?: unknown;
}

/** An edge (`Edge` on the server). */
export interface GraphEdge {
  id: string;
  scene_id: string;
  name: string | null;
  source_node_id: string;
  target_node_id: string;
  mapping: Mapping | null;
  create_time: number;
  update_time: number;
}

export interface DraftNode extends GraphNode {
  change: Change;
  conflict: Conflict | null;
  /** What differs from live: `name`, `plugin`, `config.<key>`, `uploads`. */
  parts: string[];
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
    change: Change;
    conflict: Conflict | null;
  };
  nodes: DraftNode[];
  edges: DraftEdge[];
  conflicts: string[];
  create_time: number;
  update_time: number;
}

export interface Check {
  launchable: boolean;
  problems: string[];
  warnings: string[];
}

/** The `scene` event. */
export interface SceneView {
  scene: Scene;
  nodes: GraphNode[];
  edges: GraphEdge[];
  check: Check;
  draft: Draft | null;
  draft_check: Check | null;
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
}

/** The `status` event. */
export interface SceneStatusEvent {
  time: number;
  state: SceneState;
  errors: string[];
  max_hops: number | null;
  nodes: NodeReport[];
}

/** A registered plugin (`PluginInfo`). */
export interface Plugin {
  id: string;
  name: string;
  version: string;
  type: NodeType;
  description: string | null;
  ui: { icon: boolean; categories: string[]; color: string | null; docs: boolean } | null;
  installed: boolean;
}

const scene = (id: string) => `/scenes/by-id/${encodeURIComponent(id)}`;
const draft = (id: string) => `${scene(id)}/draft`;
const part = (s: string) => encodeURIComponent(s);

export const listPlugins = () => api<Plugin[]>('/plugins');
export const pluginIconUrl = (id: string) => `/api/plugins/by-id/${part(id)}/ui/icon`;

export const createDraftNode = (id: string, body: { plugin_id: string; name?: string; position: Position }) =>
  api<GraphNode>(`${draft(id)}/nodes`, { method: 'POST', json: body });
export const removeDraftNode = (id: string, nodeId: string) =>
  api<null>(`${draft(id)}/nodes/by-id/${part(nodeId)}`, { method: 'DELETE' });
export const restoreDraftNode = (id: string, nodeId: string) =>
  api<unknown>(`${draft(id)}/nodes/by-id/${part(nodeId)}/restore`, { method: 'POST' });

export const createDraftEdge = (id: string, source: string, target: string) =>
  api<GraphEdge>(`${draft(id)}/edges`, { method: 'POST', json: { source_node_id: source, target_node_id: target } });
export const removeDraftEdge = (id: string, edgeId: string) =>
  api<null>(`${draft(id)}/edges/by-id/${part(edgeId)}`, { method: 'DELETE' });
export const restoreDraftEdge = (id: string, edgeId: string) =>
  api<unknown>(`${draft(id)}/edges/by-id/${part(edgeId)}/restore`, { method: 'POST' });

export const updateDraftSettings = (id: string, body: { name: string; max_hops: number | null; defaults: SceneDefaults }) =>
  api<Draft>(`${draft(id)}/settings`, { method: 'PUT', json: body });
export const discardDraft = (id: string) => api<null>(draft(id), { method: 'DELETE' });
export const rebaseDraft = (id: string) =>
  api<{ taken_live: string[]; conflicts: string[]; dropped_edges: string[] }>(`${draft(id)}/rebase`, { method: 'POST' });
export const resolveConflict = (id: string, target: 'settings' | 'node' | 'edge', rowId: string | null, keep: 'draft' | 'live') =>
  api<unknown>(`${draft(id)}/resolve`, { method: 'POST', json: { target, id: rowId, keep } });
export const submitDraft = (id: string, baseRevision: number, disable: boolean) =>
  api<Scene>(`${draft(id)}/submit`, { method: 'POST', json: { base_revision: baseRevision, disable } });

/** Places nodes (live and draft alike; layout isn't a change). */
export const setLayout = (id: string, nodes: Record<string, Position | null>) =>
  api<null>(`${scene(id)}/layout`, { method: 'PATCH', json: { nodes } });

/** A check's messages that name a node (`node <id>`) or an edge's ends. */
export function messagesFor(messages: string[], nodeId: string): string[] {
  return messages.filter((m) => m.includes(`node ${nodeId}`));
}

export function edgeMessages(messages: string[], source: string, target: string): string[] {
  return messages.filter((m) => m.startsWith(`edge ${source} → ${target}`));
}
