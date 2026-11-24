// Scene groups: named sets of nodes with a policy, in one of three families.
// Membership is stored on the node (`config.groups`); a group's member list
// is derived. Plain functions, so the rules can be checked without a browser.

export interface SceneGroup {
  id: string;
  family: string;
  policy: Record<string, unknown>;
}

/** A node as far as membership goes. */
export interface GroupMember {
  id: string;
  config: Record<string, unknown>;
  change?: string | null;
}

export interface NodeThreads {
  /** `null`: the plugin decides. */
  threads: number | null;
  source: 'node' | 'group' | 'manifest' | 'server' | 'plugin';
  group?: string;
}

export interface NodeDevice {
  kind: string;
  device: string;
  field: string;
}

export const FAMILIES = [
  { id: 'resources', label: 'Resources' },
  { id: 'execution', label: 'Execution' },
  { id: 'placement', label: 'Placement' },
] as const;

/** The families a scene accepts; others can only arrive in an imported file. */
export const SCENE_FAMILIES: readonly string[] = ['resources'];

export const isEditable = (g: Pick<SceneGroup, 'family'>) => SCENE_FAMILIES.includes(g.family);
export const familyLabel = (family: string) => FAMILIES.find((f) => f.id === family)?.label ?? family;

/** How many colours groups cycle through (`--group-1` … in the tokens). */
export const GROUP_COLORS = 8;

export function groupsOf(config: Record<string, unknown>): string[] {
  return Array.isArray(config.groups) ? config.groups.filter((g): g is string => typeof g === 'string') : [];
}

/** The config with this membership; the key is left out when empty. */
export function withGroups<T extends Record<string, unknown>>(config: T, ids: string[]): T {
  const { groups: _, ...rest } = config;
  return (ids.length ? { ...rest, groups: ids } : rest) as T;
}

/** Each group's members; nodes removed in the draft don't count. */
export function membersByGroup(nodes: readonly GroupMember[]): Map<string, Set<string>> {
  const out = new Map<string, Set<string>>();
  for (const n of nodes) {
    if (n.change === 'removed') continue;
    for (const g of groupsOf(n.config)) {
      if (!out.has(g)) out.set(g, new Set());
      out.get(g)!.add(n.id);
    }
  }
  return out;
}

function hash(text: string): number {
  let h = 2166136261;
  for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 16777619);
  return h >>> 0;
}

/**
 * A colour slot (1-based) per group: from its id, so it stays when other
 * groups come and go; groups that would share one take the next free slot
 * while there is one.
 */
export function groupColors(ids: readonly string[]): Map<string, number> {
  const out = new Map<string, number>();
  const used = new Set<number>();
  for (const id of [...ids].sort()) {
    const preferred = hash(id) % GROUP_COLORS;
    let slot = preferred;
    for (let i = 0; i < GROUP_COLORS; i++) {
      const candidate = (preferred + i) % GROUP_COLORS;
      if (!used.has(candidate)) {
        slot = candidate;
        break;
      }
    }
    used.add(slot);
    out.set(id, slot + 1);
  }
  return out;
}

const isSubset = (a: Set<string>, b: Set<string>) => [...a].every((x) => b.has(x));

export interface Nesting {
  parent: string | null;
  /** 0 = outermost. */
  depth: number;
  /** How many levels of groups are inside it; 0 = none. */
  height: number;
}

/**
 * Groups as a forest, per family: a group's parent is the smallest other
 * group holding all its members (of two with the same members, the one whose
 * id sorts first is the outer). Empty groups stand alone.
 */
export function nesting(groups: readonly SceneGroup[], members: Map<string, Set<string>>): Map<string, Nesting> {
  const size = (id: string) => members.get(id)?.size ?? 0;
  const encloses = (outer: SceneGroup, inner: SceneGroup) => {
    if (outer.id === inner.id || outer.family !== inner.family || !size(inner.id)) return false;
    const a = members.get(inner.id)!;
    const b = members.get(outer.id);
    if (!b || !isSubset(a, b)) return false;
    return b.size > a.size || outer.id < inner.id;
  };
  const out = new Map<string, Nesting>();
  for (const g of groups) {
    const parents = groups.filter((p) => encloses(p, g));
    parents.sort((x, y) => size(x.id) - size(y.id) || (x.id < y.id ? 1 : -1));
    out.set(g.id, { parent: parents[0]?.id ?? null, depth: parents.length, height: 0 });
  }
  for (const g of groups) {
    let level = 1;
    for (let p = out.get(g.id)!.parent; p; p = out.get(p)!.parent) {
      const entry = out.get(p)!;
      entry.height = Math.max(entry.height, level++);
    }
  }
  return out;
}

/** Pairs of groups in one family that overlap without one holding the other. */
export function crossings(groups: readonly SceneGroup[], members: Map<string, Set<string>>): [string, string][] {
  const out: [string, string][] = [];
  groups.forEach((a, i) => {
    for (const b of groups.slice(i + 1)) {
      if (a.family !== b.family) continue;
      const x = members.get(a.id);
      const y = members.get(b.id);
      if (!x || !y) continue;
      if ([...x].some((m) => y.has(m)) && !isSubset(x, y) && !isSubset(y, x)) out.push([a.id, b.id]);
    }
  });
  return out;
}

function withMembership(members: Map<string, Set<string>>, nodeId: string, ids: readonly string[]) {
  const out = new Map([...members].map(([g, set]) => [g, new Set(set)]));
  for (const set of out.values()) set.delete(nodeId);
  for (const g of ids) {
    if (!out.has(g)) out.set(g, new Set());
    out.get(g)!.add(nodeId);
  }
  return out;
}

/**
 * A node's groups after joining `target`: also the groups enclosing it, so
 * nesting holds. A join that would still cross another group of the family
 * is refused, with the reason.
 */
export function joinGroup(
  groups: readonly SceneGroup[],
  members: Map<string, Set<string>>,
  nodeId: string,
  current: readonly string[],
  target: string,
): { ids: string[]; problem: string | null } {
  const group = groups.find((g) => g.id === target);
  if (!group) return { ids: [...current], problem: `There's no group ${target}` };
  const own = members.get(target) ?? new Set<string>();
  const enclosing = own.size
    ? groups.filter((g) => {
        if (g.id === target || g.family !== group.family) return false;
        const set = members.get(g.id);
        return !!set && set.size > own.size && isSubset(own, set);
      })
    : [];
  const ids = [...new Set([...current, target, ...enclosing.map((g) => g.id)])];
  const crossing = crossings(groups, withMembership(members, nodeId, ids)).find(([a, b]) => a === target || b === target || ids.includes(a) || ids.includes(b));
  if (crossing) {
    const other = crossing[0] === target ? crossing[1] : crossing[0];
    return { ids: [...current], problem: `It would overlap group ${other}: groups of one family nest or share no members. Remove it from ${other} first.` };
  }
  return { ids, problem: null };
}

/** A node's groups after leaving `target`: also the groups nested inside it. */
export function leaveGroup(
  groups: readonly SceneGroup[],
  members: Map<string, Set<string>>,
  current: readonly string[],
  target: string,
): string[] {
  const group = groups.find((g) => g.id === target);
  const own = members.get(target);
  const inside = new Set(
    group && own
      ? groups
          .filter((g) => {
            if (g.id === target || g.family !== group.family || !current.includes(g.id)) return false;
            const set = members.get(g.id);
            return !!set && set.size < own.size && isSubset(set, own);
          })
          .map((g) => g.id)
      : [],
  );
  return current.filter((id) => id !== target && !inside.has(id));
}

const GROUP_ID = /^[a-z][a-z0-9_-]*$/;
export const MAX_GROUP_ID = 64;

/** What's wrong with a group id, as the server's rule has it. */
export function groupIdProblem(id: string): string | null {
  if (!id) return 'Give it a name';
  if (id.length > MAX_GROUP_ID) return `At most ${MAX_GROUP_ID} characters`;
  return GROUP_ID.test(id) ? null : 'Lowercase letters, digits, _ and -, starting with a letter';
}

/** A check's messages about a group (they name it as `group <id>` or `groups <a> and <b>`). */
export function groupMessages(messages: readonly string[], id: string): string[] {
  const escaped = id.replace(/[.*+?^${}()|[\]\\-]/g, '\\$&');
  const named = new RegExp(`\\bgroups? (?:\\S+ and )?"?${escaped}"?(?![\\w-])`);
  return messages.filter((m) => named.test(m));
}

/** Whether a node's problems (already its own) include a clash over this device. */
export function deviceClash(problems: readonly string[], d: Pick<NodeDevice, 'kind' | 'device'>): boolean {
  const name = `${d.kind} ${JSON.stringify(d.device)}`;
  return problems.some((p) => p.includes(`claim ${name}`) || p.includes(`claims ${name}`));
}

/** A group's policy in a few words. */
export function policySummary(g: SceneGroup): string {
  if (g.family === 'resources') {
    const cpu = (g.policy.limits as { cpu?: number } | undefined)?.cpu;
    return cpu === undefined ? 'no core limit' : `${cpu} ${cpu === 1 ? 'core' : 'cores'}`;
  }
  const parts = Object.entries(g.policy).map(([k, v]) => `${k}: ${typeof v === 'object' ? JSON.stringify(v) : v}`);
  return parts.join(', ') || 'defaults';
}

/** Where a node's core count comes from, in words. */
export function threadsText(t: NodeThreads): string {
  const count = t.threads === null ? 'The plugin decides' : `${t.threads} ${t.threads === 1 ? 'core' : 'cores'}`;
  switch (t.source) {
    case 'node':
      return `${count}: this node’s own setting`;
    case 'group':
      return `${count}: its share of group ${t.group ?? ''}`.trimEnd();
    case 'manifest':
      return `${count}: the plugin’s default`;
    case 'server':
      return `${count}: the server’s default (plugin_threads)`;
    default:
      return count;
  }
}

/** One row of the group editor. */
export interface GroupRow {
  /** The id it has now; `null` = new. */
  original: string | null;
  id: string;
  family: string;
  policy: Record<string, unknown>;
  /** `limits.cpu` as typed (resources). */
  cpu: string;
}

export function toRows(groups: readonly SceneGroup[]): GroupRow[] {
  return groups.map((g) => {
    const cpu = g.family === 'resources' ? (g.policy.limits as { cpu?: number } | undefined)?.cpu : undefined;
    return { original: g.id, id: g.id, family: g.family, policy: g.policy, cpu: cpu === undefined ? '' : String(cpu) };
  });
}

export function rowToGroup(row: GroupRow): SceneGroup {
  if (row.family !== 'resources') return { id: row.id, family: row.family, policy: row.policy };
  const { cpu: _, ...limits } = (row.policy.limits ?? {}) as Record<string, unknown>;
  return {
    id: row.id,
    family: row.family,
    policy: { ...row.policy, limits: row.cpu.trim() === '' ? limits : { ...limits, cpu: Number(row.cpu) } },
  };
}

/** Each row's problem, by index. */
export function rowProblems(rows: readonly GroupRow[]): Map<number, string> {
  const out = new Map<number, string>();
  const seen = new Set<string>();
  rows.forEach((row, i) => {
    const problem = groupIdProblem(row.id);
    if (problem) out.set(i, problem);
    else if (seen.has(row.id)) out.set(i, 'Another group has this name');
    seen.add(row.id);
    if (!out.has(i) && row.family === 'resources' && row.cpu.trim() !== '') {
      const n = Number(row.cpu);
      if (!Number.isInteger(n) || n < 1) out.set(i, 'Cores: a whole number, at least 1');
    }
  });
  return out;
}

export interface GroupEdit {
  groups: SceneGroup[];
  /** The groups differ from what the scene has. */
  changed: boolean;
  /** Old id → new id, for members; `null` = the group was deleted. */
  moves: Map<string, string | null>;
}

export function groupEdit(before: readonly SceneGroup[], rows: readonly GroupRow[]): GroupEdit {
  const groups = rows.map(rowToGroup);
  const moves = new Map<string, string | null>();
  for (const g of before) {
    const row = rows.find((r) => r.original === g.id);
    if (!row) moves.set(g.id, null);
    else if (row.id !== g.id) moves.set(g.id, row.id);
  }
  const canonical = (list: readonly SceneGroup[]) => JSON.stringify(list.map((g) => [g.id, g.family, g.policy]));
  return { groups, moves, changed: canonical(before) !== canonical(groups) };
}

/** A node's membership after an edit; `null` = unchanged. */
export function movedMembership(current: readonly string[], moves: Map<string, string | null>): string[] | null {
  if (!current.some((id) => moves.has(id))) return null;
  const next: string[] = [];
  for (const id of current) {
    const moved = moves.has(id) ? moves.get(id)! : id;
    if (moved && !next.includes(moved)) next.push(moved);
  }
  return next;
}
