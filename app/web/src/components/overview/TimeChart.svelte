<script lang="ts" module>
  export interface Series {
    key: string;
    label: string;
    /** A `--series-n` token. */
    color: string;
  }
</script>

<script lang="ts">
  // A small time chart of the samples the page collected, over the time
  // domain `from`..`to` (the page's window, or a zoom): one y-axis, up to
  // three series (2px lines, categorical tokens in fixed order), direct
  // labels at the line ends plus a legend, a crosshair + tooltip on hover,
  // drag to zoom (`onzoom`, at least `MIN_ZOOM_MS`), double-click to reset,
  // and a table view. Gaps in the samples (a reconnect) break the lines.
  import type { Sample } from '../../lib/overview';
  import { MIN_ZOOM_MS, niceMax } from '../../lib/overview';

  let {
    title,
    samples,
    series,
    format,
    cap,
    from,
    to,
    live,
    windowLabel,
    onzoom,
    onreset,
  }: {
    title: string;
    samples: Sample[];
    series: Series[];
    /** A value with its unit, e.g. `12.5/s`. */
    format: (v: number) => string;
    /** The highest y-axis maximum (e.g. 100 for percentages). */
    cap?: number;
    /** The time domain (Unix ms). */
    from: number;
    to: number;
    /** Following now (else a zoom): the axis says "ago" / "now". */
    live: boolean;
    /** The live window, e.g. "15 min". */
    windowLabel: string;
    onzoom: (from: number, to: number) => void;
    onreset: () => void;
  } = $props();

  const HEIGHT = 168;
  const PAD = { top: 10, right: 92, bottom: 22, left: 52 };
  let width = $state(600);
  let showTable = $state(false);
  let hover = $state<number | null>(null);
  /** A drag in progress: where it started and where it is (px in the svg). */
  let drag = $state<{ x0: number; x1: number } | null>(null);

  const value = (s: Sample, key: string) => (s as unknown as Record<string, number>)[key];
  const span = $derived(Math.max(1, to - from));
  const plotW = $derived(Math.max(10, width - PAD.left - PAD.right));
  const plotH = HEIGHT - PAD.top - PAD.bottom;
  const x = (t: number) => PAD.left + ((t - from) / span) * plotW;
  const timeAt = (px: number) => from + ((Math.min(Math.max(px, PAD.left), PAD.left + plotW) - PAD.left) / plotW) * span;

  /** The samples in the domain, plus one on each side so lines reach the edges. */
  const visible = $derived.by(() => {
    let a = samples.findIndex((s) => s.time >= from);
    if (a < 0) return [];
    let b = samples.length - 1;
    while (b > a && samples[b].time > to) b--;
    a = Math.max(0, a - 1);
    b = Math.min(samples.length - 1, b + 1);
    return samples.slice(a, b + 1);
  });
  const inDomain = $derived(visible.filter((s) => s.time >= from && s.time <= to));

  const yMax = $derived.by(() => {
    let max = 0;
    for (const s of inDomain) for (const { key } of series) max = Math.max(max, value(s, key));
    const nice = niceMax(max);
    return cap ? Math.min(cap, nice) : nice;
  });
  const y = (v: number) => PAD.top + plotH - (Math.min(v, yMax) / yMax) * plotH;

  /** A gap longer than this between samples breaks the line. */
  const gap = $derived.by(() => {
    const deltas = samples.slice(1).map((s, i) => s.time - samples[i].time).sort((a, b) => a - b);
    return 3 * (deltas[Math.floor(deltas.length / 2)] ?? 1000);
  });

  function path(key: string): string {
    let d = '';
    visible.forEach((s, i) => {
      const move = i === 0 || s.time - visible[i - 1].time > gap;
      d += `${move ? 'M' : 'L'}${x(s.time).toFixed(1)},${y(value(s, key)).toFixed(1)}`;
    });
    return d;
  }

  const last = $derived(inDomain[inDomain.length - 1] ?? null);

  /** Direct labels at the last sample in view, nudged apart so they don't collide. */
  const labels = $derived.by(() => {
    if (!last) return [];
    const placed = series
      .map((s) => ({ ...s, y: y(value(last, s.key)), text: format(value(last, s.key)) }))
      .sort((a, b) => a.y - b.y);
    // Push apart downwards, then back up so none leaves the plot.
    for (let i = 1; i < placed.length; i++) placed[i].y = Math.max(placed[i].y, placed[i - 1].y + 14);
    const bottom = PAD.top + plotH;
    for (let i = placed.length - 1; i >= 0; i--) {
      const limit = i === placed.length - 1 ? bottom : placed[i + 1].y - 14;
      placed[i].y = Math.min(placed[i].y, limit);
    }
    return placed;
  });

  const localX = (e: PointerEvent) => e.clientX - (e.currentTarget as SVGElement).getBoundingClientRect().left;

  function onmove(e: PointerEvent) {
    const px = localX(e);
    if (drag) {
      drag.x1 = Math.min(Math.max(px, PAD.left), PAD.left + plotW);
      hover = null;
      return;
    }
    if (inDomain.length === 0) return;
    const t = timeAt(px);
    let best = 0;
    for (let i = 1; i < inDomain.length; i++) {
      if (Math.abs(inDomain[i].time - t) < Math.abs(inDomain[best].time - t)) best = i;
    }
    hover = best;
  }

  function ondown(e: PointerEvent) {
    if (e.button !== 0) return;
    const px = localX(e);
    if (px < PAD.left || px > PAD.left + plotW) return;
    e.preventDefault(); // no text selection while dragging
    (e.currentTarget as SVGElement).setPointerCapture(e.pointerId);
    drag = { x0: px, x1: px };
  }

  function onup() {
    if (!drag) return;
    const { x0, x1 } = drag;
    drag = null;
    if (Math.abs(x1 - x0) < 4) return; // a click, not a drag
    let a = timeAt(Math.min(x0, x1));
    let b = timeAt(Math.max(x0, x1));
    if (b - a < MIN_ZOOM_MS) {
      const mid = (a + b) / 2;
      a = mid - MIN_ZOOM_MS / 2;
      b = mid + MIN_ZOOM_MS / 2;
    }
    onzoom(a, b);
  }

  const clock = (t: number) => new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  const hovered = $derived(hover === null ? null : inDomain[hover] ?? null);
</script>

<figure class="card chart">
  <figcaption>
    <div class="head">
      <span class="title">{title}</span>
      <button class="btn ghost small" type="button" aria-pressed={showTable} onclick={() => (showTable = !showTable)}>{showTable ? 'Chart' : 'Table'}</button>
    </div>
    <span class="legend" aria-hidden="true">
      {#each series as s (s.key)}<span class="item"><span class="swatch" style:background={`var(${s.color})`}></span>{s.label}</span>{/each}
    </span>
  </figcaption>

  {#if showTable}
    <div class="table-wrap">
      <table class="table">
        <thead><tr><th>Time</th>{#each series as s (s.key)}<th>{s.label}</th>{/each}</tr></thead>
        <tbody>
          {#each inDomain.slice(-20).reverse() as sample (sample.time)}
            <tr><td class="mono">{clock(sample.time)}</td>{#each series as s (s.key)}<td class="mono">{format(value(sample, s.key))}</td>{/each}</tr>
          {:else}
            <tr><td colspan={series.length + 1} class="muted">No samples yet.</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:else}
    <div class="plot" bind:clientWidth={width}>
      <svg
        {width}
        height={HEIGHT}
        role="img"
        aria-label={live ? `${title}, the last ${windowLabel}` : `${title}, ${clock(from)} to ${clock(to)}`}
        class:zooming={drag !== null}
        onpointermove={onmove}
        onpointerdown={ondown}
        onpointerup={onup}
        onpointercancel={() => (drag = null)}
        onpointerleave={() => (hover = null)}
        ondblclick={() => !live && onreset()}
      >
        {#each [0, 0.5, 1] as f (f)}
          <line class="grid" x1={PAD.left} x2={PAD.left + plotW} y1={y(yMax * f)} y2={y(yMax * f)} />
          <text class="axis" x={PAD.left - 6} y={y(yMax * f) + 4} text-anchor="end">{format(yMax * f)}</text>
        {/each}
        <text class="axis" x={PAD.left} y={HEIGHT - 6}>{live ? `${windowLabel} ago` : clock(from)}</text>
        <text class="axis" x={PAD.left + plotW} y={HEIGHT - 6} text-anchor="end">{live ? 'now' : clock(to)}</text>
        <clipPath id={`clip-${title.replace(/\W+/g, '-')}`}>
          <rect x={PAD.left} y={0} width={plotW} height={HEIGHT} />
        </clipPath>
        <g clip-path={`url(#clip-${title.replace(/\W+/g, '-')})`}>
          {#each series as s (s.key)}
            <path class="line" d={path(s.key)} style:stroke={`var(${s.color})`} />
          {/each}
        </g>
        {#if last}
          {#each labels as l (l.key)}
            <circle cx={x(last.time)} cy={y(value(last, l.key))} r="3" style:fill={`var(${l.color})`} />
            <text class="label" x={PAD.left + plotW + 8} y={l.y + 4}>{l.text}</text>
          {/each}
        {/if}
        {#if drag}
          <rect class="selection" x={Math.min(drag.x0, drag.x1)} y={PAD.top} width={Math.abs(drag.x1 - drag.x0)} height={plotH} />
        {/if}
        {#if hovered}
          <line class="crosshair" x1={x(hovered.time)} x2={x(hovered.time)} y1={PAD.top} y2={PAD.top + plotH} />
          {#each series as s (s.key)}
            <circle class="dot" cx={x(hovered.time)} cy={y(value(hovered, s.key))} r="4" style:fill={`var(${s.color})`} />
          {/each}
        {/if}
      </svg>
      {#if samples.length < 2}
        <p class="waiting muted">Collecting samples… (kept while this page is open)</p>
      {:else if inDomain.length === 0}
        <p class="waiting muted">No samples in this span: they’re kept for the last hour while this page is open.</p>
      {/if}
      {#if hovered}
        <div class="tooltip" style:left={`${x(hovered.time) > width / 2 ? Math.max(0, x(hovered.time) - 12 - 170) : x(hovered.time) + 12}px`} role="status">
          <div class="mono muted">{clock(hovered.time)}</div>
          {#each series as s (s.key)}
            <div class="row"><span class="swatch" style:background={`var(${s.color})`}></span>{s.label}<span class="mono v">{format(value(hovered, s.key))}</span></div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</figure>

<style>
  .chart {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: var(--space-4);
    min-width: 0;
  }

  figcaption {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .title {
    font-weight: 600;
  }

  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    color: var(--text-muted);
    font-size: var(--text-sm);
  }

  .item,
  .row {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
  }

  .swatch {
    width: 10px;
    height: 10px;
    border-radius: 2px;
  }

  .small {
    font-size: var(--text-sm);
  }

  .plot {
    position: relative;
    width: 100%;
  }

  svg {
    display: block;
    overflow: visible;
  }

  svg {
    cursor: crosshair;
    user-select: none;
    touch-action: pan-y;
  }

  .selection {
    fill: var(--accent-soft);
    stroke: var(--accent);
    stroke-width: 1;
    opacity: 0.7;
  }

  .grid {
    stroke: var(--chart-grid);
    stroke-width: 1;
  }

  .axis,
  .label {
    fill: var(--text-muted);
    font-size: 11px;
    font-family: var(--font-mono);
  }

  .label {
    fill: var(--text);
  }

  .line {
    fill: none;
    stroke-width: 2;
    stroke-linejoin: round;
    stroke-linecap: round;
  }

  .crosshair {
    stroke: var(--border-strong);
    stroke-width: 1;
  }

  .dot {
    stroke: var(--surface);
    stroke-width: 2;
  }

  .waiting {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    margin: 0;
    font-size: var(--text-sm);
    pointer-events: none;
  }

  .tooltip {
    position: absolute;
    top: 4px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 170px;
    padding: var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface);
    box-shadow: var(--shadow);
    font-size: var(--text-sm);
    pointer-events: none;
  }

  .v {
    margin-left: auto;
    padding-left: var(--space-3);
  }

  .table-wrap {
    max-height: 260px;
    overflow: auto;
  }
</style>
