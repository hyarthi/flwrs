// Who is using the UI: checked once at start (`GET /api/auth/session`), set
// by logging in, cleared by logging out or by any 401 from the API.

import { api, ApiError, setUnauthorizedHandler } from './api';
import { location, navigate } from './router.svelte';

/** `GET /api/auth/session` (`SessionInfo` on the server). */
export interface SessionInfo {
  provider: string;
  subject: string;
  roles: string[];
  expire_time?: string | null;
  can_log_out: boolean;
}

/** `GET /api/auth/providers`. */
export interface AuthInfo {
  providers: { id: string; label: string }[];
  loopback_only: boolean;
}

/**
 * - `loading`: the first check is running.
 * - `in`: a session, a bearer token or a loopback client.
 * - `out`: needs to log in.
 * - `refused`: not let in at all (no key configured, not on loopback).
 * - `unreachable`: the check failed for another reason.
 */
export type SessionStatus = 'loading' | 'in' | 'out' | 'refused' | 'unreachable';

class SessionState {
  status = $state<SessionStatus>('loading');
  info = $state<SessionInfo | null>(null);
  /** Why the server refused, or why the check failed. */
  problem = $state<ApiError | null>(null);

  async load() {
    try {
      this.info = await api<SessionInfo>('/auth/session');
      this.status = 'in';
      this.problem = null;
    } catch (e) {
      this.info = null;
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      this.problem = error;
      this.status = error.status === 401 ? 'out' : error.status === 403 ? 'refused' : 'unreachable';
    }
  }

  async login(key: string) {
    this.info = await api<SessionInfo>('/auth/login', { method: 'POST', json: { key } });
    this.status = 'in';
    this.problem = null;
  }

  async logout() {
    try {
      await api('/auth/logout', { method: 'POST' });
    } finally {
      this.info = null;
      this.status = 'out';
      navigate('/login', { replace: true });
    }
  }

  /** The session ended while the UI was open: back to the login page. */
  expired() {
    if (this.status !== 'in') return;
    this.info = null;
    this.status = 'out';
    navigate(loginPath(location.path + location.search), { replace: true });
  }
}

export const session = new SessionState();

setUnauthorizedHandler(() => session.expired());

/** The login page, coming back to `next` afterwards. */
export function loginPath(next: string): string {
  return next && next !== '/' && !next.startsWith('/login') ? `/login?next=${encodeURIComponent(next)}` : '/login';
}

/** Where to go after logging in: `?next=`, if it's a path on this site. */
export function nextPath(search: string): string {
  const next = new URLSearchParams(search).get('next');
  // Only local paths: never `//host` or `/\host` (open redirects).
  return next && next.startsWith('/') && !next.startsWith('//') && !next.startsWith('/\\') ? next : '/';
}
