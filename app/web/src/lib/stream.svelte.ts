// One live stream (Server-Sent Events) per view: `openStream` connects to an
// API path, hands each named event's JSON to its handler, and reconnects.
// The browser retries dropped connections on its own; when the server
// refuses one (a 401 after the session ended, a restart), the stream checks
// the session first and then retries with a backoff (1 s doubling to 15 s).
// Close it when the view goes away (an `$effect`'s cleanup).

import { session } from './session.svelte';

export type StreamState = 'connecting' | 'live' | 'reconnecting';

export interface Stream {
  readonly state: StreamState;
  close(): void;
}

class LiveStream implements Stream {
  state = $state<StreamState>('connecting');
  private source: EventSource | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private delay = 1000;
  private closed = false;

  constructor(
    private path: string,
    private handlers: Record<string, (data: any) => void>,
  ) {
    this.open();
  }

  private open() {
    if (this.closed) return;
    const source = new EventSource(`/api${this.path}`, { withCredentials: true });
    this.source = source;
    source.onopen = () => {
      this.state = 'live';
      this.delay = 1000;
    };
    for (const [name, handler] of Object.entries(this.handlers)) {
      source.addEventListener(name, (event) => {
        try {
          handler(JSON.parse((event as MessageEvent).data));
        } catch (e) {
          console.error(`stream ${this.path}: bad ${name} event`, e);
        }
      });
    }
    source.onerror = () => {
      if (this.closed) return;
      this.state = 'reconnecting';
      // CONNECTING: the browser retries by itself. CLOSED: it gave up.
      if (source.readyState === EventSource.CLOSED) this.retry();
    };
  }

  private retry() {
    this.source?.close();
    this.source = null;
    this.timer = setTimeout(async () => {
      this.timer = null;
      if (this.closed) return;
      // Logged out meanwhile? Then the app goes to the login page instead.
      await session.load();
      if (session.status !== 'in') return;
      this.open();
    }, this.delay);
    this.delay = Math.min(this.delay * 2, 15_000);
  }

  close() {
    this.closed = true;
    if (this.timer) clearTimeout(this.timer);
    this.source?.close();
    this.source = null;
  }
}

export function openStream(path: string, handlers: Record<string, (data: any) => void>): Stream {
  return new LiveStream(path, handlers);
}
