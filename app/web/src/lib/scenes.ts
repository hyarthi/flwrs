// Scenes in the management API: the live list's events and the calls the
// list page makes.

import { api } from './api';

/** A row of the `scenes` event (`SceneListRow` on the server). */
export interface SceneRow {
  id: string;
  name: string;
  enabled: boolean;
  max_hops: number | null;
  create_time: number;
  update_time: number;
  nodes: number;
  edges: number;
  problems: string[];
  warnings: string[];
}

export type SceneState = 'RUNNING' | 'STOPPED' | 'WAITING' | 'FAILED';

/** A scene in the `status` event (`SceneSummary`). */
export interface SceneSummary {
  scene_id: string;
  state: SceneState;
  nodes: number;
  running_nodes: number;
  errors: number;
  emitted: number;
  received: number;
  dropped: number;
}

export interface StatusEvent {
  time: number;
  scenes: SceneSummary[];
}

export interface Scene {
  id: string;
  name: string;
  enabled: boolean;
  max_hops: number | null;
}

export interface ImportResult {
  scene: Scene;
  launchable: boolean;
  problems: string[];
  warnings: string[];
}

const byId = (id: string) => `/scenes/by-id/${encodeURIComponent(id)}`;

export const createScene = (name: string) => api<Scene>('/scenes', { method: 'POST', json: { name } });

export const setEnabled = (id: string, enabled: boolean) =>
  api<Scene>(`${byId(id)}/${enabled ? 'enable' : 'disable'}`, { method: 'POST' });

export const deleteScene = (id: string) => api<null>(byId(id), { method: 'DELETE' });

export function importScene(file: File): Promise<ImportResult> {
  const body = new FormData();
  body.append('file', file);
  return api<ImportResult>('/scenes/import', { method: 'POST', body });
}

/** The export's download URL (the browser saves it: the server names the file). */
export function exportUrl(id: string, format: 'flwr' | 'far', includeSecrets: boolean): string {
  const query = new URLSearchParams({ format });
  if (includeSecrets) query.set('include_secrets', 'true');
  return `/api${byId(id)}/export?${query}`;
}

/** Files the import takes. */
export const IMPORT_ACCEPT = '.flwr,.far,.zip,.tar,.tgz,.tar.gz';

/**
 * Events per second from successive `status` samples. Counters restart with
 * their nodes, so a drop counts as a restart from zero.
 */
export class Rates {
  private last = new Map<string, { time: number; emitted: number }>();
  readonly current = new Map<string, number>();

  update(event: StatusEvent) {
    const seen = new Set<string>();
    for (const s of event.scenes) {
      seen.add(s.scene_id);
      const before = this.last.get(s.scene_id);
      if (before && event.time > before.time) {
        const delta = s.emitted >= before.emitted ? s.emitted - before.emitted : s.emitted;
        this.current.set(s.scene_id, (delta * 1000) / (event.time - before.time));
      }
      this.last.set(s.scene_id, { time: event.time, emitted: s.emitted });
    }
    for (const id of [...this.last.keys()]) {
      if (!seen.has(id)) {
        this.last.delete(id);
        this.current.delete(id);
      }
    }
  }
}

export function formatRate(perSecond: number | undefined): string {
  if (perSecond === undefined) return '—';
  if (perSecond === 0) return '0/s';
  if (perSecond < 10) return `${perSecond.toFixed(1)}/s`;
  if (perSecond < 10_000) return `${Math.round(perSecond)}/s`;
  return `${(perSecond / 1000).toFixed(1)}k/s`;
}
