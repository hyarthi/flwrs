// A node's live channels (`LIVE_FEED`), as the server streams them: the
// parsers for both kinds of stream and the generic per-node stats shape.
// Nothing here knows a plugin: a JSON channel whose items have `nodes` keyed
// by graph node ids feeds the graph editor's numbers; an image channel feeds
// its preview. Plain TypeScript (no runes), so `node --test` runs it.

export interface LiveChannel {
  channel: string;
  media_type: string;
  /** How the server streams it; null = it can't be shown. */
  display: 'image' | 'json' | null;
  viewers: number;
}

/** A `{mean, p95}` pair, or a plain number. */
export type Metric = number | null | { [key: string]: number | null };

/** One graph node's numbers. A node without an entry wasn't measured. */
export type NodeStats = Record<string, Metric | undefined>;

/** A stats item: `nodes` by graph node id, everything else a summary. */
export interface LiveStats {
  interval_s?: number;
  nodes?: Record<string, NodeStats>;
  [key: string]: unknown;
}

/** A Server-Sent Events stream, fed in chunks of text. */
export class SseParser {
  private text = '';

  push(chunk: string): { event: string; data: string }[] {
    this.text += chunk.replace(/\r\n?/g, '\n');
    const out: { event: string; data: string }[] = [];
    for (;;) {
      const end = this.text.indexOf('\n\n');
      if (end < 0) break;
      const block = this.text.slice(0, end);
      this.text = this.text.slice(end + 2);
      let event = 'message';
      const data: string[] = [];
      for (const line of block.split('\n')) {
        if (line.startsWith(':')) continue; // a keep-alive comment
        const colon = line.indexOf(':');
        const field = colon < 0 ? line : line.slice(0, colon);
        let value = colon < 0 ? '' : line.slice(colon + 1);
        if (value.startsWith(' ')) value = value.slice(1);
        if (field === 'event') event = value;
        else if (field === 'data') data.push(value);
      }
      if (data.length) out.push({ event, data: data.join('\n') });
    }
    if (this.text.length > MAX_PART) throw new Error('an event is too large');
    return out;
  }
}

/** The largest part either parser accepts (above the hub's own limit). */
const MAX_PART = 16 * 1024 * 1024;
const MAX_HEADERS = 4096;

/** The boundary of a `multipart/x-mixed-replace` response. */
export function boundaryOf(contentType: string | null): string | null {
  const m = /boundary="?([^";]+)"?/i.exec(contentType ?? '');
  return m ? m[1] : null;
}

/**
 * A `multipart/x-mixed-replace` stream, fed in chunks of bytes. Each part
 * needs its `Content-Length` (the server always sends one).
 */
export class MultipartParser {
  private buf = new Uint8Array(0);
  private readonly open: Uint8Array;
  private readonly close: Uint8Array;

  constructor(boundary: string) {
    const enc = new TextEncoder();
    this.open = enc.encode(`--${boundary}\r\n`);
    this.close = enc.encode(`\r\n--${boundary}\r\n`);
  }

  push(chunk: Uint8Array): { type: string; data: Uint8Array }[] {
    const next = new Uint8Array(this.buf.length + chunk.length);
    next.set(this.buf);
    next.set(chunk, this.buf.length);
    this.buf = next;
    const out: { type: string; data: Uint8Array }[] = [];
    for (;;) {
      let at = 0;
      if (startsWith(this.buf, this.close, 0)) at = this.close.length;
      else if (startsWith(this.buf, this.open, 0)) at = this.open.length;
      else if (this.buf.length < this.close.length) break;
      else throw new Error('not a part boundary');
      const headEnd = find(this.buf, HEAD_END, at);
      if (headEnd < 0) {
        if (this.buf.length - at > MAX_HEADERS) throw new Error('part headers too long');
        break;
      }
      const head = new TextDecoder().decode(this.buf.subarray(at, headEnd));
      const length = Number(/content-length:\s*(\d+)/i.exec(head)?.[1] ?? NaN);
      if (!Number.isFinite(length) || length > MAX_PART) throw new Error('a part has no usable Content-Length');
      const type = /content-type:\s*([^\r\n]+)/i.exec(head)?.[1].trim() ?? 'application/octet-stream';
      const start = headEnd + HEAD_END.length;
      if (this.buf.length < start + length) break;
      out.push({ type, data: this.buf.slice(start, start + length) });
      this.buf = this.buf.slice(start + length);
    }
    return out;
  }
}

const HEAD_END = new Uint8Array([13, 10, 13, 10]);

function startsWith(buf: Uint8Array, prefix: Uint8Array, at: number): boolean {
  if (buf.length - at < prefix.length) return false;
  for (let i = 0; i < prefix.length; i++) if (buf[at + i] !== prefix[i]) return false;
  return true;
}

function find(buf: Uint8Array, needle: Uint8Array, from: number): number {
  for (let i = from; i + needle.length <= buf.length; i++) if (startsWith(buf, needle, i)) return i;
  return -1;
}

// ---- the stats shape ----

/** A number as people read it: few digits. */
export function formatNumber(n: number): string {
  if (!Number.isFinite(n)) return '—';
  const abs = Math.abs(n);
  if (abs >= 100 || Number.isInteger(n)) return String(Math.round(n));
  if (abs >= 10) return n.toFixed(1);
  return n.toFixed(abs >= 1 ? 1 : 2);
}

/** `captured_fps` → `Captured fps`. */
export function labelOf(key: string): string {
  const words = key.replace(/_/g, ' ').replace(/\bms\b/, '(ms)');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

function meanOf(m: Metric | undefined): number | null {
  if (typeof m === 'number') return m;
  if (m && typeof m === 'object' && typeof m.mean === 'number') return m.mean;
  return null;
}

/** The short line under a node on the canvas: rate, latency, errors. */
export function nodeLine(s: NodeStats | undefined): string | null {
  if (!s) return null;
  const parts: string[] = [];
  if (typeof s.fps === 'number') parts.push(`${formatNumber(s.fps)} fps`);
  const latency = meanOf(s.latency_ms);
  if (latency !== null) parts.push(`${formatNumber(latency)} ms`);
  if (typeof s.errors === 'number' && s.errors > 0) parts.push(`${s.errors} err`);
  return parts.length ? parts.join(' · ') : null;
}

/** Whether a node did work in the last interval (its links show activity). */
export function isActive(s: NodeStats | undefined): boolean {
  if (!s) return false;
  return (typeof s.runs === 'number' && s.runs > 0) || (typeof s.fps === 'number' && s.fps > 0);
}

/** Every number of an object as rows, one level of nesting flattened. */
export function metricRows(obj: Record<string, unknown> | undefined): { label: string; value: string }[] {
  const rows: { label: string; value: string }[] = [];
  for (const [key, value] of Object.entries(obj ?? {})) {
    if (typeof value === 'number') rows.push({ label: labelOf(key), value: formatNumber(value) });
    else if (value === null) rows.push({ label: labelOf(key), value: '—' });
    else if (typeof value === 'object' && !Array.isArray(value)) {
      for (const [inner, v] of Object.entries(value as Record<string, unknown>)) {
        const label = `${labelOf(key)} ${inner}`;
        if (typeof v === 'number') rows.push({ label, value: formatNumber(v) });
        else if (v === null) rows.push({ label, value: '—' });
      }
    }
  }
  return rows;
}

/** A stats item's summary sections: every object but `nodes`. */
export function summaryOf(stats: LiveStats | null): { title: string; rows: { label: string; value: string }[] }[] {
  if (!stats) return [];
  return Object.entries(stats)
    .filter(([k, v]) => k !== 'nodes' && v && typeof v === 'object' && !Array.isArray(v))
    .map(([k, v]) => ({ title: labelOf(k), rows: metricRows(v as Record<string, unknown>) }))
    .filter((s) => s.rows.length > 0);
}

/** Whether a setting turns anything on: some boolean in it is true. */
export function turnsOn(value: unknown): boolean {
  if (value === true) return true;
  if (value && typeof value === 'object') return Object.values(value).some(turnsOn);
  return false;
}

/** How long without an item before what's shown is cleared as stale. */
export function staleAfter(gapMs: number): number {
  return Math.max(4000, 3 * gapMs);
}
