// The server page: `/api/server/…` (info, the config file, saving, restarts).

import { api } from './api';
import type { Schema } from './schemaForm';

export interface ServerInfo {
  version: string;
  features: string[];
  platform: string;
  pid: number;
  start_time: number;
  config_path: string;
  config_edit: boolean;
  read_only_reason: string | null;
  restart_needed: boolean;
  restart_mode: string;
  confirm_deadline: number | null;
  last_revert: { time: number; reason: string } | null;
  step_up: boolean;
  http_address: string;
  hub_address: string;
  hub_tls_address: string | null;
}

export interface Problem {
  section: string | null;
  message: string;
}

export interface SectionView {
  key: string;
  title: string;
  summary: string;
  schema: Schema;
  values: Record<string, unknown>;
  effective: Record<string, unknown> | null;
  sources: Record<string, 'file' | 'env' | 'default'>;
  locked: string[];
  secrets: string[];
}

export interface ConfigView {
  path: string;
  text: string | null;
  secret: string;
  sections: SectionView[];
  problems: Problem[];
  editable: boolean;
  read_only_reason: string | null;
}

export interface CheckResponse {
  problems: Problem[];
  locked: string[];
  restart_needed: boolean;
  values: Record<string, Record<string, unknown>>;
}

export const getInfo = () => api<ServerInfo>('/server/info');
export const getConfig = () => api<ConfigView>('/server/config');
export const checkConfig = (text: string) => api<CheckResponse>('/server/config/check', { method: 'POST', json: { text } });
export const applyForm = (text: string, section: string, values: Record<string, unknown>) =>
  api<{ text: string }>('/server/config/form', { method: 'POST', json: { text, section, values } });
export const saveConfig = (text: string, secretKey: string | null) =>
  api<{ restart_needed: boolean }>('/server/config', { method: 'PUT', json: { text, secret_key: secretKey } });
export const restartServer = (secretKey: string | null) =>
  api<{ confirm: boolean }>('/server/restart', { method: 'POST', json: { secret_key: secretKey } });
export const confirmConfig = () => api<null>('/server/config/confirm', { method: 'POST' });
export const revertConfig = (secretKey: string | null) =>
  api<null>('/server/config/revert', { method: 'POST', json: { secret_key: secretKey } });

export const dismissRevert = () => api<null>('/server/config/reverted', { method: 'DELETE' });

export const problemText = (p: Problem) => (p.section ? `[${p.section}] ${p.message}` : p.message);
