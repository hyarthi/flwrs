// Watching a running node's live channels from the graph editor. Opening a
// channel's stream is what makes the plugin do the work, so a stream is open
// only while it's wanted (its toggle is on), the editor is showing, the page
// is visible and the node is running; anything else closes it at once.
//
// Streams are read with `fetch` (not `<img src>` or `EventSource`), so each
// one can be aborted for sure, a stall is noticed, and a browser keeps at
// most one live view open: a view that opens streams claims them over a
// `BroadcastChannel`, and the others pause (HTTP/1.1 allows a browser ~6
// connections per server; a live view takes two).

import { api } from './api';
import { MultipartParser, SseParser, boundaryOf, staleAfter, type LiveChannel, type LiveStats } from './liveFeed';

export type FeedState = 'off' | 'connecting' | 'waiting' | 'live';

const POLL_MS = 2000;
const WATCHDOG_MS = 500;

class Feed {
  state = $state<FeedState>('off');
  channel: string | null = null;
  private controller: AbortController | null = null;
  /** When the last item came, and the usual gap between items. */
  last = 0;
  private gap = 1000;

  constructor(
    private readonly kind: 'image' | 'json',
    private readonly onItem: (item: Uint8Array | unknown, type: string) => void,
    private readonly onClear: () => void,
  ) {}

  get open(): boolean {
    return this.controller !== null;
  }

  start(path: string, channel: string) {
    if (this.controller && this.channel === channel) return;
    this.stop();
    this.channel = channel;
    const controller = new AbortController();
    this.controller = controller;
    this.state = 'connecting';
    void this.run(path, controller.signal);
  }

  stop() {
    this.controller?.abort();
    this.controller = null;
    this.channel = null;
    this.state = 'off';
    this.last = 0;
    this.onClear();
  }

  /** Clears what's shown if nothing came for too long (a restart, a stall). */
  check(now: number) {
    if (this.state === 'live' && now - this.last > staleAfter(this.gap)) {
      this.state = 'waiting';
      this.onClear();
    }
  }

  private got(item: Uint8Array | unknown, type: string) {
    const now = Date.now();
    if (this.last) this.gap = this.gap * 0.7 + (now - this.last) * 0.3;
    this.last = now;
    this.state = 'live';
    this.onItem(item, type);
  }

  private async run(path: string, signal: AbortSignal) {
    let delay = 500;
    while (!signal.aborted) {
      try {
        const res = await fetch(`/api${path}`, { signal, credentials: 'same-origin', cache: 'no-store' });
        if (!res.ok || !res.body) throw new Error(`status ${res.status}`);
        this.state = 'waiting';
        const reader = res.body.getReader();
        signal.addEventListener('abort', () => void reader.cancel().catch(() => {}), { once: true });
        if (this.kind === 'image') {
          const boundary = boundaryOf(res.headers.get('content-type'));
          if (!boundary) throw new Error('no boundary');
          const parts = new MultipartParser(boundary);
          for (;;) {
            const { done, value } = await reader.read();
            if (done || signal.aborted) break;
            for (const p of parts.push(value)) {
              this.got(p.data, p.type);
              delay = 500;
            }
          }
        } else {
          const events = new SseParser();
          const text = new TextDecoder();
          for (;;) {
            const { done, value } = await reader.read();
            if (done || signal.aborted) break;
            for (const e of events.push(text.decode(value, { stream: true }))) {
              if (e.event !== 'frame') continue;
              try {
                this.got(JSON.parse(e.data).data, 'application/json');
                delay = 500;
              } catch {
                // A bad item is skipped.
              }
            }
          }
        }
      } catch {
        // Ended, refused or cut off: try again below, unless closed.
      }
      if (signal.aborted) return;
      this.state = 'connecting';
      this.onClear();
      await new Promise((r) => setTimeout(r, delay));
      delay = Math.min(delay * 2, 5000);
    }
  }
}

/** Only one view per browser has streams open (see the top). */
const claims = typeof BroadcastChannel === 'undefined' ? null : new BroadcastChannel('flwrs.live-view');

export class LiveView {
  /** The channels the node announced (empty while it isn't connected). */
  channels = $state<LiveChannel[]>([]);
  /** The channels were listed at least once while running. */
  listed = $state(false);
  /** The toggles: what the user wants to watch. */
  statsWanted = $state(false);
  videoWanted = $state(false);
  /** Another tab or window took the live view over. */
  paused = $state(false);
  stats = $state.raw<LiveStats | null>(null);
  /** The newest preview frame, as a blob URL. */
  frame = $state<string | null>(null);

  readonly statsFeed: Feed;
  readonly videoFeed: Feed;

  private readonly id = Math.random().toString(36).slice(2);
  private running = false;
  private showing = false;
  private visible = typeof document === 'undefined' ? true : !document.hidden;
  private poll: ReturnType<typeof setInterval> | null = null;
  private watchdog: ReturnType<typeof setInterval> | null = null;
  private urls: string[] = [];
  private claimed = false;

  constructor(
    private readonly sceneId: string,
    private readonly nodeId: string,
  ) {
    this.statsFeed = new Feed(
      'json',
      (item) => (this.stats = item as LiveStats),
      () => (this.stats = null),
    );
    this.videoFeed = new Feed(
      'image',
      (item, type) => {
        const url = URL.createObjectURL(new Blob([item as Uint8Array<ArrayBuffer>], { type }));
        this.urls.push(url);
        this.frame = url;
        // An image that never loads (the panel is hidden) can't hold more.
        while (this.urls.length > 8) URL.revokeObjectURL(this.urls.shift()!);
      },
      () => this.clearFrames(),
    );
  }

  get statsChannel(): LiveChannel | undefined {
    return this.channels.find((c) => c.display === 'json');
  }

  get videoChannel(): LiveChannel | undefined {
    return this.channels.find((c) => c.display === 'image');
  }

  /** Starts watching the page's visibility and other views' claims; returns the stop. */
  attach(): () => void {
    const onVisibility = () => {
      this.visible = !document.hidden;
      this.reconcile();
    };
    const onClaim = (e: MessageEvent) => {
      if (e.data?.claim && e.data.claim !== this.id && this.feedsOpen) {
        this.paused = true;
        this.claimed = false;
        this.reconcile();
      }
    };
    document.addEventListener('visibilitychange', onVisibility);
    claims?.addEventListener('message', onClaim);
    return () => {
      document.removeEventListener('visibilitychange', onVisibility);
      claims?.removeEventListener('message', onClaim);
      this.running = false;
      this.reconcile();
    };
  }

  /** The node is running, and the editor is showing. */
  setContext(running: boolean, showing: boolean) {
    this.running = running;
    this.showing = showing;
    this.reconcile();
  }

  toggleStats() {
    this.statsWanted = !this.statsWanted;
    if (this.statsWanted) this.paused = false;
    this.reconcile();
  }

  toggleVideo() {
    this.videoWanted = !this.videoWanted;
    if (this.videoWanted) this.paused = false;
    this.reconcile();
  }

  resume() {
    this.paused = false;
    this.reconcile();
  }

  /** The preview showed `url`: older frames can go. */
  shown(url: string) {
    while (this.urls.length && this.urls[0] !== url) URL.revokeObjectURL(this.urls.shift()!);
  }

  private get feedsOpen(): boolean {
    return this.statsFeed.open || this.videoFeed.open;
  }

  private clearFrames() {
    for (const url of this.urls) URL.revokeObjectURL(url);
    this.urls = [];
    this.frame = null;
  }

  private path(channel?: string): string {
    const base = `/scenes/by-id/${encodeURIComponent(this.sceneId)}/nodes/by-id/${encodeURIComponent(this.nodeId)}/live`;
    return channel ? `${base}/${encodeURIComponent(channel)}/stream` : base;
  }

  private async list() {
    try {
      const { channels } = await api<{ channels: LiveChannel[] }>(this.path());
      if (!this.running) return;
      this.channels = channels;
      this.listed = true;
    } catch {
      this.channels = [];
    }
    this.reconcile();
  }

  /** Opens what's wanted and allowed, closes everything else. */
  private reconcile() {
    const watching = this.running && this.showing && this.visible;
    if (watching && !this.poll) {
      void this.list();
      this.poll = setInterval(() => void this.list(), POLL_MS);
    } else if (!watching && this.poll) {
      clearInterval(this.poll);
      this.poll = null;
    }
    if (!this.running) {
      this.channels = [];
      this.listed = false;
    }
    const allowed = watching && !this.paused;
    const stats = allowed && this.statsWanted ? this.statsChannel : undefined;
    const video = allowed && this.videoWanted ? this.videoChannel : undefined;
    const wasOpen = this.feedsOpen;
    if (stats) this.statsFeed.start(this.path(stats.channel), stats.channel);
    else if (this.statsFeed.open) this.statsFeed.stop();
    if (video) this.videoFeed.start(this.path(video.channel), video.channel);
    else if (this.videoFeed.open) this.videoFeed.stop();
    if (this.feedsOpen && (!wasOpen || !this.claimed)) {
      this.claimed = true;
      claims?.postMessage({ claim: this.id });
    }
    if (!this.feedsOpen) this.claimed = false;
    if (this.feedsOpen && !this.watchdog) {
      this.watchdog = setInterval(() => {
        const now = Date.now();
        this.statsFeed.check(now);
        this.videoFeed.check(now);
      }, WATCHDOG_MS);
    } else if (!this.feedsOpen && this.watchdog) {
      clearInterval(this.watchdog);
      this.watchdog = null;
    }
  }
}
