// A graph setting (`x-flwrs-graph`) as an editor sees it: what its catalog
// says about node types and ports, and the edits, as plain functions on the
// stored JSON. Every edit returns a new value and leaves everything it isn't
// about alone: unknown keys and key order survive a round trip.

export type Json = unknown;
export type JsonObject = Record<string, Json>;

export interface Condition {
  pointer: string;
  present?: boolean;
  equals?: Json;
  not_equals?: Json;
  default?: Json;
}

export interface CatalogPort {
  name?: string;
  type?: string;
  type_from?: { pointer: string; map: Record<string, string> };
  when?: Condition;
}

export interface CatalogNodeType {
  title: string;
  category?: string;
  description?: string;
  settings: string;
  defaults?: JsonObject;
  max?: number;
  inputs?: CatalogPort[];
  outputs?: CatalogPort[];
  file_settings?: string[];
  proposal?: { field: string; file_setting: string; at: string };
  link_badges?: { label: string; when: Condition }[];
}

export interface GraphCatalog {
  format: number;
  port_types: Record<string, { title: string }>;
  files_field?: string;
  connects?: { from: string; to: string; badge?: string }[];
  node_types: Record<string, CatalogNodeType>;
  groups?: { node_types: string[]; families: string[]; settings: string };
  /** The top-level setting that turns the plugin's live channels on. */
  live_setting?: string;
}

export interface GraphLink extends JsonObject {
  from: string;
  to: string;
}

export interface GraphValue extends JsonObject {
  nodes: Record<string, JsonObject>;
  links?: GraphLink[];
  groups?: JsonObject[];
}

export interface Port {
  name: string;
  type: string;
}

export interface Point {
  x: number;
  y: number;
}

/** What's stored, as a graph; anything else starts empty. */
export function asGraph(value: Json): GraphValue {
  const v = value && typeof value === 'object' && !Array.isArray(value) ? (value as JsonObject) : {};
  const nodes = v.nodes && typeof v.nodes === 'object' && !Array.isArray(v.nodes) ? (v.nodes as Record<string, JsonObject>) : {};
  return { ...v, nodes };
}

export const linksOf = (g: GraphValue): GraphLink[] => (Array.isArray(g.links) ? g.links : []);

export function getPointer(value: Json, pointer: string): Json {
  let at = value;
  for (const raw of pointer.split('/').slice(1)) {
    if (!at || typeof at !== 'object') return undefined;
    at = (at as JsonObject)[raw.replace(/~1/g, '/').replace(/~0/g, '~')];
  }
  return at;
}

/** A plain copy (values may be reactive proxies, which `structuredClone` refuses). */
const cloneJson = <T,>(value: T): T => JSON.parse(JSON.stringify(value)) as T;

const same = (a: Json, b: Json) => JSON.stringify(a) === JSON.stringify(b);

export function holds(c: Condition, node: JsonObject): boolean {
  const value = getPointer(node, c.pointer) ?? c.default;
  if (c.present !== undefined) return (value !== undefined) === c.present;
  if ('equals' in c) return same(value, c.equals);
  return !same(value, c.not_equals);
}

function resolvePorts(list: CatalogPort[] | undefined, node: JsonObject): Port[] {
  const out: Port[] = [];
  for (const p of list ?? []) {
    if (p.when && !holds(p.when, node)) continue;
    const type = p.type ?? p.type_from?.map[String(getPointer(node, p.type_from.pointer))];
    // A type read from a setting that isn't chosen yet: no port for now.
    if (type) out.push({ name: p.name ?? type, type });
  }
  return out;
}

/** A node's ports, as its current settings make them. */
export function portsOf(catalog: GraphCatalog, node: JsonObject): { inputs: Port[]; outputs: Port[] } {
  const type = catalog.node_types[String(node.type)];
  return { inputs: resolvePorts(type?.inputs, node), outputs: resolvePorts(type?.outputs, node) };
}

export function parseEnd(end: string): { node: string; port: string } | null {
  const dot = end.lastIndexOf('.');
  return dot > 0 && dot < end.length - 1 ? { node: end.slice(0, dot), port: end.slice(dot + 1) } : null;
}

export const portTypesConnect = (catalog: GraphCatalog, from: string, to: string) =>
  from === to || (catalog.connects ?? []).some((c) => c.from === from && c.to === to);

/** Whether `to` is reachable from `from` over the links. */
function reaches(g: GraphValue, from: string, to: string): boolean {
  const next = new Map<string, string[]>();
  for (const l of linksOf(g)) {
    const a = parseEnd(l.from)?.node;
    const b = parseEnd(l.to)?.node;
    if (a && b) next.set(a, [...(next.get(a) ?? []), b]);
  }
  const seen = new Set<string>();
  const stack = [from];
  while (stack.length) {
    const at = stack.pop()!;
    if (at === to) return true;
    if (seen.has(at)) continue;
    seen.add(at);
    stack.push(...(next.get(at) ?? []));
  }
  return false;
}

/** Why this link can't be made; `null` = it can. */
export function linkProblem(catalog: GraphCatalog, g: GraphValue, from: string, to: string): string | null {
  const a = parseEnd(from);
  const b = parseEnd(to);
  if (!a || !b || !g.nodes[a.node] || !g.nodes[b.node]) return 'That port isn’t there.';
  if (a.node === b.node) return 'A node can’t link to itself.';
  const out = portsOf(catalog, g.nodes[a.node]).outputs.find((p) => p.name === a.port);
  const input = portsOf(catalog, g.nodes[b.node]).inputs.find((p) => p.name === b.port);
  if (!out || !input) return 'That port isn’t there.';
  if (!portTypesConnect(catalog, out.type, input.type)) {
    const title = (t: string) => catalog.port_types[t]?.title ?? t;
    return `${title(out.type)} can’t go into ${title(input.type)}.`;
  }
  if (linksOf(g).some((l) => l.to === to)) return `${b.node}.${b.port} is already linked.`;
  if (reaches(g, b.node, a.node)) return 'That would make a cycle.';
  return null;
}

/** What a link's box shows: the port type, for-each and the target's sampling. */
export function linkBadges(catalog: GraphCatalog, g: GraphValue, link: GraphLink): string[] {
  const a = parseEnd(link.from);
  const b = parseEnd(link.to);
  if (!a || !b) return [];
  const source = g.nodes[a.node];
  const target = g.nodes[b.node];
  if (!source || !target) return [];
  const out = portsOf(catalog, source).outputs.find((p) => p.name === a.port);
  const input = portsOf(catalog, target).inputs.find((p) => p.name === b.port);
  const badges: string[] = [];
  if (out) badges.push(catalog.port_types[out.type]?.title ?? out.type);
  const rule = out && input && (catalog.connects ?? []).find((c) => c.from === out.type && c.to === input.type);
  if (rule?.badge) badges.push(rule.badge);
  for (const badge of catalog.node_types[String(target.type)]?.link_badges ?? []) {
    if (!holds(badge.when, target)) continue;
    const value = getPointer(target, badge.when.pointer);
    badges.push(badge.label.replace('{value}', typeof value === 'object' ? '' : String(value)));
  }
  return badges;
}

/** A free id: `base`, else `base_2`, … (ids are lowercase with `_`). */
export function freeId(taken: Iterable<string>, base: string): string {
  const used = new Set(taken);
  const clean = base.toLowerCase().replace(/[^a-z0-9_]/g, '_').replace(/^[^a-z]+/, '') || 'node';
  if (!used.has(clean)) return clean;
  for (let i = 2; ; i++) if (!used.has(`${clean}_${i}`)) return `${clean}_${i}`;
}

/** Why another node of this type can't be added. */
export function addProblem(catalog: GraphCatalog, g: GraphValue, type: string): string | null {
  const kind = catalog.node_types[type];
  if (!kind) return `There’s no node type ${type}.`;
  const count = Object.values(g.nodes).filter((n) => n.type === type).length;
  return kind.max !== undefined && count >= kind.max ? `A graph can have ${kind.max} ${kind.title} node${kind.max === 1 ? '' : 's'}.` : null;
}

export function addNode(catalog: GraphCatalog, g: GraphValue, type: string): { graph: GraphValue; id: string } {
  const id = freeId(Object.keys(g.nodes), type);
  const defaults = catalog.node_types[type]?.defaults ?? {};
  return { id, graph: { ...g, nodes: { ...g.nodes, [id]: { type, ...cloneJson(defaults) } } } };
}

export function setNode(g: GraphValue, id: string, node: JsonObject): GraphValue {
  return { ...g, nodes: { ...g.nodes, [id]: node } };
}

/** Links of nodes that went away go with them; other links stay as they are. */
export function removeNodes(g: GraphValue, ids: Iterable<string>): GraphValue {
  const gone = new Set(ids);
  const nodes = Object.fromEntries(Object.entries(g.nodes).filter(([id]) => !gone.has(id)));
  if (!Array.isArray(g.links)) return { ...g, nodes };
  const links = g.links.filter((l) => !gone.has(parseEnd(l.from)?.node ?? '') && !gone.has(parseEnd(l.to)?.node ?? ''));
  return { ...g, nodes, links };
}

export function addLink(g: GraphValue, from: string, to: string): GraphValue {
  return { ...g, links: [...linksOf(g), { from, to }] };
}

export function removeLinks(g: GraphValue, gone: readonly GraphLink[]): GraphValue {
  if (!Array.isArray(g.links)) return g;
  const key = (l: GraphLink) => `${l.from}>${l.to}`;
  const set = new Set(gone.map(key));
  return { ...g, links: g.links.filter((l) => !set.has(key(l))) };
}

/** Why a node can't take this id. */
export function idProblem(g: GraphValue, id: string, next: string): string | null {
  if (!/^[a-z][a-z0-9_]{0,63}$/.test(next)) return 'Lowercase letters, digits and _, starting with a letter.';
  return next !== id && g.nodes[next] ? 'Another node has this id.' : null;
}

/** Renames a node in place (its position among the nodes stays), and in the links. */
export function renameNode(g: GraphValue, id: string, next: string): GraphValue {
  if (id === next) return g;
  const nodes = Object.fromEntries(Object.entries(g.nodes).map(([k, v]) => [k === id ? next : k, v]));
  const end = (e: string) => {
    const p = parseEnd(e);
    return p?.node === id ? `${next}.${p.port}` : e;
  };
  if (!Array.isArray(g.links)) return { ...g, nodes };
  return { ...g, nodes, links: g.links.map((l) => ({ ...l, from: end(l.from), to: end(l.to) })) };
}

/** A copy of some nodes and the links between them. */
export interface Copied {
  flwrs: 'graph-nodes';
  version: 1;
  nodes: Record<string, JsonObject>;
  links: GraphLink[];
  positions: Record<string, Point>;
}

export function copyNodes(g: GraphValue, ids: Iterable<string>, positions: Record<string, Point>): Copied {
  const set = new Set(ids);
  // Group membership is a deliberate edit: a copy starts outside the groups.
  const plain = ({ groups: _, ...rest }: JsonObject): JsonObject => cloneJson(rest);
  const nodes = Object.fromEntries(Object.entries(g.nodes).filter(([id]) => set.has(id)).map(([id, n]) => [id, plain(n)]));
  const inside = (e: string) => set.has(parseEnd(e)?.node ?? '');
  return {
    flwrs: 'graph-nodes',
    version: 1,
    nodes,
    links: linksOf(g).filter((l) => inside(l.from) && inside(l.to)),
    positions: Object.fromEntries(Object.entries(positions).filter(([id]) => set.has(id))),
  };
}

export function isCopied(value: Json): value is Copied {
  const v = value as Copied | null;
  return !!v && v.flwrs === 'graph-nodes' && v.version === 1 && !!v.nodes && typeof v.nodes === 'object' && Array.isArray(v.links);
}

/**
 * Pastes nodes under free ids, a little apart. Types the catalog doesn't
 * know, and types at their limit, are skipped; `skipped` names them.
 */
export function pasteNodes(
  catalog: GraphCatalog,
  g: GraphValue,
  copied: Copied,
  offset: number,
): { graph: GraphValue; ids: Map<string, string>; positions: Record<string, Point>; skipped: string[] } {
  let graph = g;
  const ids = new Map<string, string>();
  const positions: Record<string, Point> = {};
  const skipped: string[] = [];
  for (const [id, node] of Object.entries(copied.nodes)) {
    const type = String(node.type);
    if (addProblem(catalog, graph, type)) {
      skipped.push(id);
      continue;
    }
    const next = freeId(Object.keys(graph.nodes), id);
    graph = setNode(graph, next, cloneJson(node));
    ids.set(id, next);
    const at = copied.positions?.[id];
    if (at) positions[next] = { x: at.x + offset, y: at.y + offset };
  }
  for (const l of copied.links) {
    const a = parseEnd(l.from);
    const b = parseEnd(l.to);
    const from = a && ids.get(a.node);
    const to = b && ids.get(b.node);
    if (!from || !to) continue;
    const link = { from: `${from}.${a.port}`, to: `${to}.${b.port}` };
    if (!linkProblem(catalog, graph, link.from, link.to)) graph = addLink(graph, link.from, link.to);
  }
  return { graph, ids, positions, skipped };
}

/** What the graph is missing, by the catalog (before the plugin has seen it). */
export function graphProblems(catalog: GraphCatalog, g: GraphValue): { nodes: Map<string, string[]>; links: Map<number, string[]> } {
  const nodes = new Map<string, string[]>();
  const links = new Map<number, string[]>();
  const note = <K,>(map: Map<K, string[]>, key: K, text: string) => map.set(key, [...(map.get(key) ?? []), text]);
  const linked = new Set(linksOf(g).map((l) => l.to));
  for (const [id, node] of Object.entries(g.nodes)) {
    const kind = catalog.node_types[String(node.type)];
    if (!kind) {
      note(nodes, id, `Unknown node type ${String(node.type)}.`);
      continue;
    }
    for (const p of portsOf(catalog, node).inputs) {
      if (!linked.has(`${id}.${p.name}`)) note(nodes, id, `Input ${p.name} isn’t linked.`);
    }
  }
  for (const [type, kind] of Object.entries(catalog.node_types)) {
    if (kind.max === undefined) continue;
    const of = Object.entries(g.nodes).filter(([, n]) => n.type === type);
    if (of.length > kind.max) for (const [id] of of) note(nodes, id, `A graph can have ${kind.max} of these.`);
  }
  linksOf(g).forEach((l, i) => {
    const a = parseEnd(l.from);
    const b = parseEnd(l.to);
    const source = a && g.nodes[a.node];
    const target = b && g.nodes[b.node];
    if (!source || !target) return note(links, i, 'An end of this link is gone.');
    const out = portsOf(catalog, source).outputs.find((p) => p.name === a.port);
    const input = portsOf(catalog, target).inputs.find((p) => p.name === b.port);
    if (!out || !input) note(links, i, 'A port of this link is gone.');
    else if (!portTypesConnect(catalog, out.type, input.type)) note(links, i, `${out.type} can’t go into ${input.type}.`);
  });
  return { nodes, links };
}

/** The tag a plugin puts on a message about a place in its settings. */
const LOCATION = /\[at (\/[^\]\s]*)\]\s*/;
/** How the server names a place in a node's settings when the schema refuses it. */
const SCHEMA_PLACE = /config\.launch_args(\/[^\s:]+)/;

/** A message without its location tag, for lists that aren't about a place. */
export const withoutLocation = (message: string) => message.replace(LOCATION, '');

/** Messages sorted by the inner node or link their `[at <pointer>]` tag names. */
export function locate(
  messages: readonly string[],
  field: string,
): { nodes: Map<string, string[]>; links: Map<number, string[]>; other: string[] } {
  const nodes = new Map<string, string[]>();
  const links = new Map<number, string[]>();
  const other: string[] = [];
  const prefix = `/${field.replace(/~/g, '~0').replace(/\//g, '~1')}/`;
  for (const message of messages) {
    const found = LOCATION.exec(message);
    const text = found ? message.replace(LOCATION, '') : message;
    const pointer = found?.[1] ?? SCHEMA_PLACE.exec(message)?.[1];
    const path = pointer?.startsWith(prefix) ? pointer.slice(prefix.length).split('/') : null;
    if (path?.[0] === 'nodes' && path[1]) {
      const id = path[1].replace(/~1/g, '/').replace(/~0/g, '~');
      nodes.set(id, [...(nodes.get(id) ?? []), text]);
    } else if (path?.[0] === 'links' && /^\d+$/.test(path[1] ?? '')) {
      const i = Number(path[1]);
      links.set(i, [...(links.get(i) ?? []), text]);
    } else {
      other.push(text);
    }
  }
  return { nodes, links, other };
}

const isEmpty = (v: Json) => v === undefined || v === null || v === '';

/**
 * Fills a node's empty top-level settings from a proposal. A setting that
 * has a value is never touched. Returns the keys it filled.
 */
export function applyProposal(node: JsonObject, proposal: Json): { node: JsonObject; filled: string[] } {
  if (!proposal || typeof proposal !== 'object' || Array.isArray(proposal)) return { node, filled: [] };
  const next = { ...node };
  const filled: string[] = [];
  for (const [key, value] of Object.entries(proposal as JsonObject)) {
    if (key === 'type' || !isEmpty(next[key])) continue;
    next[key] = cloneJson(value);
    filled.push(key);
  }
  return { node: next, filled };
}

/** The proposal for a file, from what the configurator says about the node's files. */
export function proposalFor(kind: CatalogNodeType, values: JsonObject | null, file: string): Json {
  if (!kind.proposal || !values) return undefined;
  const info = values[kind.proposal.field];
  if (!info || typeof info !== 'object') return undefined;
  return getPointer((info as JsonObject)[file], kind.proposal.at);
}

/** Every file the graph's nodes name, in the catalog's file settings. */
export function filesNamed(catalog: GraphCatalog, g: GraphValue): string[] {
  const out = new Set<string>();
  for (const node of Object.values(g.nodes)) {
    for (const key of catalog.node_types[String(node.type)]?.file_settings ?? []) {
      const v = node[key];
      if (typeof v === 'string' && v) out.add(v);
    }
  }
  return [...out];
}

/** Keeps the list upload setting's names, and adds the ones the graph needs. */
export function withFiles(current: Json, needed: readonly string[]): string[] {
  const list = Array.isArray(current) ? current.filter((n): n is string => typeof n === 'string') : [];
  return [...list, ...needed.filter((n) => !list.includes(n))];
}

export const NODE_GAP = { x: 300, y: 130 };

/**
 * Where unplaced nodes go: a column per depth along the links, below what
 * shares it. Placed nodes stay; positions of nodes no longer there are
 * ignored.
 */
export function autoLayout(g: GraphValue, placed: Record<string, Point>): Record<string, Point> {
  const ids = Object.keys(g.nodes);
  const incoming = new Map<string, string[]>(ids.map((id) => [id, []]));
  for (const l of linksOf(g)) {
    const a = parseEnd(l.from)?.node;
    const b = parseEnd(l.to)?.node;
    if (a && b && incoming.has(a) && incoming.has(b)) incoming.get(b)!.push(a);
  }
  const depth = new Map<string, number>();
  const visit = (id: string, path: Set<string>): number => {
    const known = depth.get(id);
    if (known !== undefined) return known;
    if (path.has(id)) return 0;
    path.add(id);
    const d = Math.max(-1, ...incoming.get(id)!.map((p) => visit(p, path))) + 1;
    path.delete(id);
    depth.set(id, d);
    return d;
  };
  const out: Record<string, Point> = {};
  const rows = new Map<number, number>();
  for (const id of ids) if (placed[id]) out[id] = placed[id];
  for (const id of ids) {
    if (out[id]) continue;
    const d = visit(id, new Set());
    const x = d * NODE_GAP.x;
    let row = rows.get(d) ?? 0;
    const free = (y: number) => !Object.values(out).some((p) => Math.abs(p.x - x) < NODE_GAP.x / 2 && Math.abs(p.y - y) < NODE_GAP.y * 0.75);
    while (!free(row * NODE_GAP.y)) row++;
    out[id] = { x, y: row * NODE_GAP.y };
    rows.set(d, row + 1);
  }
  return out;
}

/** Undo / redo over whole graph values. */
export class History {
  private past: GraphValue[] = [];
  private future: GraphValue[] = [];
  private readonly limit: number;

  constructor(limit = 100) {
    this.limit = limit;
  }

  /** Call with the value as it was, before an edit. */
  record(before: GraphValue) {
    this.past.push(before);
    if (this.past.length > this.limit) this.past.shift();
    this.future = [];
  }

  undo(current: GraphValue): GraphValue | null {
    const value = this.past.pop();
    if (value === undefined) return null;
    this.future.push(current);
    return value;
  }

  redo(current: GraphValue): GraphValue | null {
    const value = this.future.pop();
    if (value === undefined) return null;
    this.past.push(current);
    return value;
  }

  get canUndo() {
    return this.past.length > 0;
  }

  get canRedo() {
    return this.future.length > 0;
  }

  clear() {
    this.past = [];
    this.future = [];
  }
}
