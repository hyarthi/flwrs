// Plugins in the management API: the registry, what uses each plugin, and
// uploads, upgrades and deletes. Changes need `[plugins] allow_upload` and,
// if the server has one, the admin token (`X-Flwrs-Admin-Token`): asked for
// when needed and kept in memory for this tab only (never stored).

import { api, ApiError } from './api';
import type { NodeType, SchemaField } from './sceneView';

export interface PluginInfo {
  id: string;
  name: string;
  version: string;
  type: NodeType;
  description: string | null;
  platforms: string[] | null;
  launch_args_style: string;
  has_launch_args_schema: boolean;
  in_schema: unknown;
  out_schema: unknown;
  ui: { icon: boolean; categories: string[]; color: string | null; docs: boolean } | null;
  configured_fields: string[];
  takes_threads: boolean;
  manifest_version: number;
  /** A remote plugin definition (manifest `launch: "remote"`). */
  remote_only: boolean;
  node_schemas: { in: boolean; out: boolean };
  installed: boolean;
  archive_sha256: string;
  create_time: number;
  update_time: number;
}

export interface PluginSettings {
  allow_upload: boolean;
  admin_token_required: boolean;
  max_upload_bytes: number;
  platform: string;
}

export interface PluginUse {
  plugin_id: string;
  scene_id: string;
  scene_name: string;
  node_id: string;
  node_name: string | null;
  draft_only: boolean;
}

const byId = (id: string) => `/plugins/by-id/${encodeURIComponent(id)}`;

export const listPlugins = () => api<PluginInfo[]>('/plugins');
export const getPluginInfo = (id: string) => api<PluginInfo>(byId(id));
export const getPluginSettings = () => api<PluginSettings>('/plugins/settings');
export const getUsage = () => api<PluginUse[]>('/plugins/usage');
export const getLaunchArgsSchema = (id: string) => api<Record<string, unknown>>(`${byId(id)}/launch-args-schema`);
export const getNodeSchemas = (id: string) =>
  api<{ in: SchemaField[] | null; out: SchemaField[] | null }>(`${byId(id)}/node-schemas`, { method: 'POST', json: { launch_args: {} } });

/** The plugin's docs (Markdown from the package), or null. */
export async function getDocs(id: string): Promise<string | null> {
  const response = await fetch(`/api${byId(id)}/ui/docs`, { credentials: 'same-origin' });
  return response.ok ? response.text() : null;
}

// ---- changes, with the admin token ----

let adminToken: string | null = null;
/** Asks the user for the admin token (set by the page that shows the prompt). */
let askToken: ((retry: boolean) => Promise<string | null>) | null = null;
export function setTokenPrompt(prompt: ((retry: boolean) => Promise<string | null>) | null) {
  askToken = prompt;
}

/** Runs a change, asking for the admin token when the server wants it (again if it was wrong). */
async function withToken<T>(change: (headers: Record<string, string>) => Promise<T>): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    try {
      return await change(adminToken ? { 'X-Flwrs-Admin-Token': adminToken } : {});
    } catch (e) {
      if (!(e instanceof ApiError) || e.status !== 401 || !askToken || attempt > 2) throw e;
      const token = await askToken(adminToken !== null);
      if (!token) throw e;
      adminToken = token;
    }
  }
}

function packageBody(file: File): FormData {
  const body = new FormData();
  body.append('file', file);
  return body;
}

export const uploadPlugin = (file: File) =>
  withToken((headers) => api<PluginInfo>('/plugins', { method: 'POST', body: packageBody(file), headers }));

export const upgradePlugin = (id: string, file: File) =>
  withToken((headers) => api<PluginInfo>(byId(id), { method: 'PUT', body: packageBody(file), headers }));

export const deletePlugin = (id: string) => withToken((headers) => api<null>(byId(id), { method: 'DELETE', headers }));

// ---- remote plugin definitions (JSON, no ZIP; no allow_upload needed) ----

/** A file of a definition: JSON (a schema), text (docs, an SVG) or base64 (a PNG). */
export type DefinitionFile = { json: unknown } | { text: string } | { base64: string };

/** A remote plugin definition: its manifest (`launch: "remote"`) and files by path. */
export interface Definition {
  manifest: Record<string, unknown>;
  files: Record<string, DefinitionFile>;
}

/** A remote plugin definition as a package ZIP (a download link). */
export const exportUrl = (id: string) => `/api${byId(id)}/export`;

export const getDefinition = (id: string) => api<Definition>(`${byId(id)}/definition`);

export const registerDefinition = (definition: Definition) =>
  withToken((headers) => api<PluginInfo>('/plugins/remote', { method: 'POST', json: definition, headers }));

export const replaceDefinition = (id: string, definition: Definition) =>
  withToken((headers) => api<PluginInfo>(`${byId(id)}/definition`, { method: 'PUT', json: definition, headers }));

/** Package files the upload takes. */
export const PACKAGE_ACCEPT = '.zip';
