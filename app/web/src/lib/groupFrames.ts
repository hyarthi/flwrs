// The outlines of a family's groups on the canvas, in flow coordinates:
// Bubble Sets around the members, routed around other nodes. `FrameEngine`
// recomputes only the groups a change touches, within a time budget per
// animation frame; during a drag, what doesn't fit gets a padded convex hull
// until the drag ends.

import { BubbleSets, rect, type PointPath } from 'bubblesets-js';

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface Point {
  x: number;
  y: number;
}

export interface Frame {
  id: string;
  /** SVG path data. */
  d: string;
  points: Point[];
  box: Rect;
  /** A hull standing in for the full outline. */
  rough: boolean;
}

export interface FrameGroup {
  id: string;
  /** Levels of groups nested inside: an outer frame gets more margin. */
  height: number;
}

export interface FrameInput {
  groups: readonly FrameGroup[];
  members: ReadonlyMap<string, ReadonlySet<string>>;
  rects: ReadonlyMap<string, Rect>;
}

/** Margin around members, and what each level of nesting adds. */
const MARGIN = 14;
const NEST_STEP = 12;

export const centerOf = (r: Rect): Point => ({ x: r.x + r.width / 2, y: r.y + r.height / 2 });

export function pointInPolygon(p: Point, polygon: readonly Point[]): boolean {
  let inside = false;
  for (let i = 0, j = polygon.length - 1; i < polygon.length; j = i++) {
    const a = polygon[i];
    const b = polygon[j];
    if (a.y > p.y !== b.y > p.y && p.x < ((b.x - a.x) * (p.y - a.y)) / (b.y - a.y) + a.x) inside = !inside;
  }
  return inside;
}

function boxOf(points: readonly Point[]): Rect {
  let [minX, minY, maxX, maxY] = [Infinity, Infinity, -Infinity, -Infinity];
  for (const p of points) {
    minX = Math.min(minX, p.x);
    minY = Math.min(minY, p.y);
    maxX = Math.max(maxX, p.x);
    maxY = Math.max(maxY, p.y);
  }
  return { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
}

const pathOf = (points: readonly Point[]) =>
  points.length ? `M${points.map((p) => `${Math.round(p.x)} ${Math.round(p.y)}`).join('L')}Z` : '';

const overlaps = (a: Rect, b: Rect) =>
  a.x <= b.x + b.width && b.x <= a.x + a.width && a.y <= b.y + b.height && b.y <= a.y + a.height;

/** The convex hull of the rectangles, each grown by `pad`. */
export function hullOutline(id: string, rects: readonly Rect[], pad: number): Frame {
  const corners = rects.flatMap((r) => [
    { x: r.x - pad, y: r.y - pad },
    { x: r.x + r.width + pad, y: r.y - pad },
    { x: r.x + r.width + pad, y: r.y + r.height + pad },
    { x: r.x - pad, y: r.y + r.height + pad },
  ]);
  corners.sort((a, b) => a.x - b.x || a.y - b.y);
  const cross = (o: Point, a: Point, b: Point) => (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);
  const half = (list: Point[]) => {
    const out: Point[] = [];
    for (const p of list) {
      while (out.length >= 2 && cross(out[out.length - 2], out[out.length - 1], p) <= 0) out.pop();
      out.push(p);
    }
    out.pop();
    return out;
  };
  const points = [...half(corners), ...half([...corners].reverse())];
  return { id, d: pathOf(points), points, box: boxOf(points), rough: true };
}

/** The full outline; a hull when it can't enclose every member. */
export function bubbleOutline(id: string, members: readonly Rect[], others: readonly Rect[], height: number): Frame {
  const pad = MARGIN + height * NEST_STEP;
  const fallback = () => ({ ...hullOutline(id, members, pad), rough: false });
  try {
    const sets = new BubbleSets({ nodeR0: pad + 2, nodeR1: pad + 36, morphBuffer: 10 });
    sets.pushMember(...members.map((r) => rect(r.x, r.y, r.width, r.height)));
    sets.pushNonMember(...others.map((r) => rect(r.x, r.y, r.width, r.height)));
    const raw: PointPath = sets.compute();
    if (raw.length < 3) return fallback();
    const path = raw.sample(8).simplify(0).bSplines().simplify(0);
    const points = path.points.map((p) => ({ x: p.x, y: p.y }));
    if (points.length < 3 || !members.every((r) => pointInPolygon(centerOf(r), points))) return fallback();
    return { id, d: path.toString(1), points, box: boxOf(points), rough: false };
  } catch {
    return fallback();
  }
}

const sameRect = (a: Rect | undefined, b: Rect | undefined) =>
  !!a && !!b && a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height;

export class FrameEngine {
  readonly frames = new Map<string, Frame>();
  private input: FrameInput = { groups: [], members: new Map(), rects: new Map() };
  private signatures = new Map<string, string>();
  private pending = new Set<string>();
  private rough = new Set<string>();

  /** Takes the current state; returns whether any group needs recomputing. */
  sync(input: FrameInput): boolean {
    const before = this.input.rects;
    const moved: Rect[] = [];
    const movedIds = new Set<string>();
    for (const [id, r] of input.rects) {
      const old = before.get(id);
      if (sameRect(old, r)) continue;
      movedIds.add(id);
      moved.push(r);
      if (old) moved.push(old);
    }
    for (const [id, old] of before) {
      if (input.rects.has(id)) continue;
      movedIds.add(id);
      moved.push(old);
    }

    const signatures = new Map<string, string>();
    for (const g of input.groups) {
      const members = [...(input.members.get(g.id) ?? [])].filter((m) => input.rects.has(m)).sort();
      if (!members.length) continue;
      const signature = `${g.height}|${members.join(',')}`;
      signatures.set(g.id, signature);
      const frame = this.frames.get(g.id);
      if (
        !frame ||
        this.signatures.get(g.id) !== signature ||
        members.some((m) => movedIds.has(m)) ||
        moved.some((r) => overlaps(r, frame.box))
      ) {
        this.pending.add(g.id);
      }
    }
    for (const id of [...this.frames.keys()]) {
      if (signatures.has(id)) continue;
      this.frames.delete(id);
      this.rough.delete(id);
    }
    for (const id of [...this.pending]) if (!signatures.has(id)) this.pending.delete(id);
    const removedAny = this.signatures.size !== signatures.size;
    this.signatures = signatures;
    this.input = input;
    return this.pending.size > 0 || removedAny;
  }

  /** The drag ended: hulls become full outlines. */
  settle(): boolean {
    for (const id of this.rough) this.pending.add(id);
    return this.pending.size > 0;
  }

  get busy(): boolean {
    return this.pending.size > 0;
  }

  /**
   * Recomputes pending groups for up to `budgetMs`. During a drag the rest
   * get a hull at once; otherwise they wait for the next frame.
   */
  step(budgetMs: number, dragging: boolean, now: () => number = () => performance.now()): void {
    const start = now();
    // Hulls first, so a long drag can't leave one group rough for good.
    const order = [...this.pending].sort((a, b) => Number(this.rough.has(b)) - Number(this.rough.has(a)));
    for (const id of order) {
      const group = this.input.groups.find((g) => g.id === id);
      if (!group) {
        this.pending.delete(id);
        continue;
      }
      const memberIds = this.input.members.get(id) ?? new Set<string>();
      const members: Rect[] = [];
      const others: Rect[] = [];
      for (const [nodeId, r] of this.input.rects) (memberIds.has(nodeId) ? members : others).push(r);
      if (now() - start < budgetMs) {
        this.frames.set(id, bubbleOutline(id, members, others, group.height));
        this.rough.delete(id);
        this.pending.delete(id);
      } else if (dragging) {
        this.frames.set(id, hullOutline(id, members, MARGIN + group.height * NEST_STEP));
        this.rough.add(id);
        this.pending.delete(id);
      } else {
        return;
      }
    }
  }
}

/** How far (flow px) a modifier-drag must move a node to take it out of a group. */
const MIN_LEAVE_DISTANCE = 48;

/** What a finished drag of one node means for its membership. */
export type DragOutcome = { join: string } | { leave: string } | null;

/**
 * A plain drag joins the innermost frame the node's centre ended in, unless
 * it's a member or started inside that frame already (a nudge isn't a
 * decision). A modifier-drag never joins: it leaves the outermost of the
 * node's groups it ended outside of. That's judged by the frame around the
 * group's other members (`without`), since a member's own frame stretches to
 * wherever it was left; a group with no other members uses its frame.
 */
export function dragOutcome(args: {
  /** The frames when the drag began. */
  frames: readonly Frame[];
  start: Point;
  end: Point;
  /** The node's groups. */
  current: readonly string[];
  /** Whether nodes can join or leave this group here. */
  editable: (id: string) => boolean;
  /** Members per group, to tell inner from outer. */
  size: (id: string) => number;
  modifier: boolean;
  /** The group's frame without this node; `null` = it has no other members. */
  without?: (id: string) => Frame | null;
}): DragOutcome {
  const { frames, start, end, current, editable, size, modifier, without } = args;
  if (modifier) {
    // A nudge isn't a decision here either.
    if (Math.hypot(end.x - start.x, end.y - start.y) < MIN_LEAVE_DISTANCE) return null;
    const left = frames
      .filter((f) => current.includes(f.id) && editable(f.id) && !pointInPolygon(end, (without?.(f.id) ?? f).points))
      .sort((a, b) => size(b.id) - size(a.id));
    return left.length ? { leave: left[0].id } : null;
  }
  const hit = frames
    .filter((f) => pointInPolygon(end, f.points))
    .sort((a, b) => size(a.id) - size(b.id) || a.box.width * a.box.height - b.box.width * b.box.height)[0];
  if (!hit || current.includes(hit.id) || !editable(hit.id) || pointInPolygon(start, hit.points)) return null;
  return { join: hit.id };
}
