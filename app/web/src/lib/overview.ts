// The overview page's data: `GET /api/status/stream`'s `overview` events,
// and the history the browser keeps of them while the page is open (no
// server-side history): event rates from successive samples, and memory
// and disk use as a share of their limits.

import type { SceneSummary } from './scenes';

export interface Usage {
  used_bytes: number;
  limit_bytes: number;
}

export interface HubConnections {
  connected: number;
  pending: number;
  refused_busy: number;
  failed_handshakes: number;
}

/** An `overview` event (`OverviewEvent`: `GET /api/status` + its time). */
export interface OverviewEvent {
  time: number;
  hub_address: string;
  hub_connections: HubConnections;
  queued_memory: Usage;
  spill_memory: Usage;
  spill_disk: Usage;
  scenes: SceneSummary[];
}

/** One point of the history. */
export interface Sample {
  time: number;
  /** Events per second, all scenes. */
  produced: number;
  delivered: number;
  dropped: number;
  /** Percent of each limit. */
  queued: number;
  spillMemory: number;
  spillDisk: number;
}

/** How much history the page keeps: the longest window. */
export const WINDOW_MS = 60 * 60 * 1000;

/** The windows the page offers. */
export const WINDOWS = [
  { id: '1m', label: '1 min', ms: 60_000 },
  { id: '5m', label: '5 min', ms: 5 * 60_000 },
  { id: '15m', label: '15 min', ms: 15 * 60_000 },
  { id: '30m', label: '30 min', ms: 30 * 60_000 },
  { id: '1h', label: '1 hour', ms: 60 * 60_000 },
] as const;
export type WindowId = (typeof WINDOWS)[number]['id'];

/** The narrowest zoom. */
export const MIN_ZOOM_MS = 30_000;

const percent = (u: Usage) => (u.limit_bytes > 0 ? (100 * u.used_bytes) / u.limit_bytes : 0);

/** The samples of the last `WINDOW_MS`, from successive overview events. */
export class History {
  samples: Sample[] = [];
  private last = new Map<string, { emitted: number; received: number; dropped: number }>();
  private lastTime = 0;

  /** Adds an event; returns the new sample (the first event only primes the counters). */
  add(event: OverviewEvent): Sample | null {
    const dt = (event.time - this.lastTime) / 1000;
    const primed = this.lastTime > 0 && dt > 0;
    let produced = 0;
    let delivered = 0;
    let dropped = 0;
    const seen = new Map<string, { emitted: number; received: number; dropped: number }>();
    for (const s of event.scenes) {
      const now = { emitted: s.emitted, received: s.received, dropped: s.dropped };
      seen.set(s.scene_id, now);
      const before = this.last.get(s.scene_id);
      if (!before) continue;
      // A counter that went down restarted with its node: count from zero.
      const delta = (a: number, b: number) => (a >= b ? a - b : a);
      produced += delta(now.emitted, before.emitted);
      delivered += delta(now.received, before.received);
      dropped += delta(now.dropped, before.dropped);
    }
    this.last = seen;
    this.lastTime = event.time;
    if (!primed) return null;
    const sample: Sample = {
      time: event.time,
      produced: produced / dt,
      delivered: delivered / dt,
      dropped: dropped / dt,
      queued: percent(event.queued_memory),
      spillMemory: percent(event.spill_memory),
      spillDisk: percent(event.spill_disk),
    };
    this.samples.push(sample);
    const from = event.time - WINDOW_MS;
    let cut = 0;
    while (cut < this.samples.length && this.samples[cut].time < from) cut++;
    if (cut) this.samples.splice(0, cut);
    return sample;
  }
}

/** A round axis maximum at or above `value` (1, 2, 2.5, 5 × 10ⁿ). */
export function niceMax(value: number): number {
  if (!(value > 0)) return 1;
  const exp = 10 ** Math.floor(Math.log10(value));
  for (const step of [1, 2, 2.5, 5, 10]) if (step * exp >= value) return step * exp;
  return 10 * exp;
}
