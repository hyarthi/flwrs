// Run with `npm test` (Node's test runner, which reads the TypeScript itself).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  MultipartParser,
  SseParser,
  boundaryOf,
  isActive,
  metricRows,
  nodeLine,
  staleAfter,
  summaryOf,
  turnsOn,
} from '../src/lib/liveFeed.ts';

const enc = new TextEncoder();

function part(boundary: string, type: string, data: Uint8Array): Uint8Array {
  const head = enc.encode(`Content-Type: ${type}\r\nContent-Length: ${data.length}\r\n\r\n`);
  const tail = enc.encode(`\r\n--${boundary}\r\n`);
  const out = new Uint8Array(head.length + data.length + tail.length);
  out.set(head);
  out.set(data, head.length);
  out.set(tail, head.length + data.length);
  return out;
}

test('multipart parts come out whole, however the bytes are split', () => {
  const b = 'flwrs-live-1';
  const frames = [new Uint8Array([0xff, 0xd8, 13, 10, 13, 10, 1]), new Uint8Array([0xff, 0xd8, 2, 3])];
  const stream = [enc.encode(`--${b}\r\n`), ...frames.map((f) => part(b, 'image/jpeg', f))];
  const all = new Uint8Array(stream.reduce((n, s) => n + s.length, 0));
  let at = 0;
  for (const s of stream) {
    all.set(s, at);
    at += s.length;
  }
  for (const size of [1, 3, 7, all.length]) {
    const p = new MultipartParser(b);
    const got: Uint8Array[] = [];
    for (let i = 0; i < all.length; i += size) got.push(...p.push(all.subarray(i, i + size)).map((x) => x.data));
    assert.deepEqual(got, frames, `chunks of ${size}`);
  }
});

test('multipart refuses what isn’t a part', () => {
  assert.throws(() => new MultipartParser('b').push(enc.encode('garbage that is long enough\r\n')));
  assert.equal(boundaryOf('multipart/x-mixed-replace; boundary=abc'), 'abc');
  assert.equal(boundaryOf('text/plain'), null);
});

test('SSE events across chunks, comments skipped', () => {
  const p = new SseParser();
  assert.deepEqual(p.push(': keep-alive\n\nevent: fra'), []);
  assert.deepEqual(p.push('me\ndata: {"a":1}\n\nevent: frame\r\ndata: 2\r\n\r\n'), [
    { event: 'frame', data: '{"a":1}' },
    { event: 'frame', data: '2' },
  ]);
});

test('the stats shape: lines, rows, not measured, activity', () => {
  const stats = {
    version: 1,
    interval_s: 1,
    frames: { captured_fps: 30, process_ms: { mean: 13.24, p95: 21 }, latency_ms: null },
    nodes: { det: { fps: 14.5, runs: 29, errors: 2, latency_ms: { mean: 9.8, p95: null } }, idle: { fps: 0, runs: 0 } },
  };
  assert.equal(nodeLine(stats.nodes.det), '14.5 fps · 9.8 ms · 2 err');
  assert.equal(nodeLine(undefined), null);
  assert.equal(isActive(stats.nodes.det), true);
  assert.equal(isActive(stats.nodes.idle), false);
  assert.equal(isActive(undefined), false);
  assert.deepEqual(metricRows(stats.nodes.det).map((r) => `${r.label}=${r.value}`), [
    'Fps=14.5',
    'Runs=29',
    'Errors=2',
    'Latency (ms) mean=9.8',
    'Latency (ms) p95=—',
  ]);
  const summary = summaryOf(stats);
  assert.deepEqual(summary.map((s) => s.title), ['Frames']);
  assert.ok(summary[0].rows.some((r) => r.label === 'Latency (ms)' && r.value === '—'));
});

test('a setting turns live on when some boolean in it is true', () => {
  assert.equal(turnsOn({ stats: false, preview: { enabled: false, fps: 5 } }), false);
  assert.equal(turnsOn({ stats: false, preview: { enabled: true } }), true);
  assert.equal(turnsOn(undefined), false);
  assert.equal(staleAfter(200), 4000);
  assert.equal(staleAfter(2000), 6000);
});
