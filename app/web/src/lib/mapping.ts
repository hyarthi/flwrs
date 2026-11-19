// The mapping editor's model: a mapping as editable rows (and back), dot
// paths for schema fields, and sample payloads from a schema.

import type { Mapping, MappingRule, SchemaField } from './sceneView';

export type RuleKind = 'copy' | 'set' | 'lua' | 'delete' | 'decode' | 'encode';

/** A rule as the editor holds it (a `set` value as JSON text). */
export interface RuleRow {
  id: number;
  kind: RuleKind;
  target: string;
  source: string;
  value: string;
  lua: string;
}

let nextId = 1;
export const newRow = (kind: RuleKind, target = '', source = ''): RuleRow => ({
  id: nextId++,
  kind,
  target,
  source,
  value: kind === 'set' ? '""' : '',
  lua: kind === 'lua' ? 'return payload.' : '',
});

export function toRow(rule: MappingRule): RuleRow {
  const kind: RuleKind = rule.decode
    ? 'decode'
    : rule.encode
      ? 'encode'
      : rule.delete
        ? 'delete'
        : rule.lua !== undefined
          ? 'lua'
          : rule.value !== undefined
            ? 'set'
            : 'copy';
  return {
    id: nextId++,
    kind,
    target: rule.target,
    source: rule.source ?? '',
    value: rule.value !== undefined ? JSON.stringify(rule.value) : '',
    lua: rule.lua ?? '',
  };
}

/** A row as a rule; a `set` whose value isn't JSON is an error. */
export function toRule(row: RuleRow): MappingRule | { error: string } {
  const target = row.target.trim();
  switch (row.kind) {
    case 'copy':
      return { target, source: row.source.trim() };
    case 'set':
      try {
        return { target, value: JSON.parse(row.value) };
      } catch {
        return { error: `the value for ${target || 'a rule'} isn’t valid JSON` };
      }
    case 'lua':
      return { target, lua: row.lua };
    case 'delete':
      return { target, delete: true };
    case 'decode':
      return { target, source: row.source.trim(), decode: 'json' };
    case 'encode':
      return { target, source: row.source.trim(), encode: 'json' };
  }
}

/** The editor's state as a mapping (`null` = none: events pass through). */
export interface EditorState {
  mode: 'merge' | 'replace';
  filter: string | null;
  /** Rules, or a script for the whole output. */
  useScript: boolean;
  script: string;
  rows: RuleRow[];
  /** Kept as it was (no form for it). */
  lua: Mapping['lua'];
}

export function fromMapping(m: Mapping | null): EditorState {
  return {
    mode: m?.mode ?? 'merge',
    filter: m?.filter?.lua ?? null,
    useScript: !!m?.script,
    script: m?.script?.lua ?? 'return {\n  \n}',
    rows: (m?.rules ?? []).map(toRow),
    lua: m?.lua,
  };
}

export function toMapping(s: EditorState): { mapping: Mapping | null; errors: string[] } {
  const errors: string[] = [];
  const mapping: Mapping = {};
  if (s.mode === 'replace') mapping.mode = 'replace';
  if (s.filter !== null) mapping.filter = { lua: s.filter };
  if (s.useScript) {
    mapping.script = { lua: s.script };
  } else if (s.rows.length) {
    mapping.rules = [];
    for (const row of s.rows) {
      const rule = toRule(row);
      if ('error' in rule) errors.push(rule.error);
      else mapping.rules.push(rule);
    }
  }
  if (s.lua) mapping.lua = s.lua;
  return { mapping: Object.keys(mapping).length ? mapping : null, errors };
}

/** A key as a dot-path segment: bare when it can be, else `["…"]`. */
export function segment(key: string, first: boolean): string {
  if (/^[A-Za-z0-9_-]+$/.test(key)) return first ? key : `.${key}`;
  return `[${JSON.stringify(key)}]`;
}

/** An example value for a field, for sample payloads. */
export function sampleOf(field: SchemaField): unknown {
  switch (field.type) {
    case 'STRING':
      return field.key || 'text';
    case 'I32':
    case 'U32':
    case 'I64':
    case 'U64':
      return 1;
    case 'F32':
    case 'F64':
      return 1.5;
    case 'BOOL':
      return true;
    case 'DATETIME':
      return new Date().toISOString();
    case 'BYTES':
      // Base64.
      return btoa(field.content_type === 'application/json' ? '{}' : 'bytes');
    case 'ARRAY':
      return field.element ? [sampleOf(field.element)] : [];
    case 'MAP':
      return field.element ? { key: sampleOf(field.element) } : {};
    case 'OBJECT':
      return samplePayload(field.fields ?? []);
    default:
      return field.type.startsWith('PACKED_') ? [1, 2, 3] : null;
  }
}

export function samplePayload(fields: SchemaField[]): Record<string, unknown> {
  return Object.fromEntries(fields.map((f) => [f.key, sampleOf(f)]));
}

export type Side = 'source' | 'target';

/** The drag-and-drop type of a schema field (its data: `{side, path}`). */
export const FIELD_DRAG_TYPE = 'application/x-flwrs-field';

export function readDraggedField(e: DragEvent): { side: Side; path: string } | null {
  try {
    const v = JSON.parse(e.dataTransfer?.getData(FIELD_DRAG_TYPE) ?? '');
    return (v?.side === 'source' || v?.side === 'target') && typeof v.path === 'string' ? v : null;
  } catch {
    return null;
  }
}
