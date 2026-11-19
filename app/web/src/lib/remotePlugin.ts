// The remote plugin wizard's model: what the user edits (identity, payload
// fields, node settings, appearance) ↔ a remote plugin definition (a
// manifest with `launch: "remote"` and the files it references), as
// `POST /api/plugins/remote` takes it. Manifest fields the wizard doesn't
// edit (e.g. `runtime_defaults`, `x-*`) are kept as they were.

import type { NodeType } from './sceneView';
import type { Definition, DefinitionFile } from './plugins';

export const FIELD_TYPES = [
  'STRING', 'BOOL', 'I32', 'I64', 'U32', 'U64', 'F32', 'F64', 'DATETIME', 'BYTES',
  'OBJECT', 'ARRAY', 'MAP',
  'PACKED_F32', 'PACKED_F64', 'PACKED_I32', 'PACKED_I64', 'PACKED_U32', 'PACKED_U64',
] as const;
export type FieldType = (typeof FIELD_TYPES)[number];

/** One field of a payload schema (an ARRAY's or MAP's `element` has no key). */
export interface FieldRow {
  key: string;
  type: FieldType;
  description: string;
  content_type: string;
  /** ARRAY elements, MAP values. */
  element: FieldRow | null;
  /** OBJECT fields. */
  fields: FieldRow[];
}

export interface Side {
  /** Each node sets its own schema in its settings (at `pointer`). */
  perNode: boolean;
  pointer: string;
  /** Fields as rows, or the SchemaDefinition as JSON text. */
  mode: 'fields' | 'json';
  fields: FieldRow[];
  json: string;
}

export interface WizardState {
  id: string;
  name: string;
  version: string;
  type: NodeType;
  description: string;
  in: Side;
  out: Side;
  /** The launch-args JSON Schema as text ('' = none). */
  settings: string;
  /** Top-level settings a remote configurator endpoint sets. */
  configured: string[];
  categories: string;
  color: string;
  docs: string;
  icon: { name: string; file: DefinitionFile } | null;
  /** Manifest fields kept as they were. */
  extra: Record<string, unknown>;
}

export const ID_PATTERN = /^[a-z0-9][a-z0-9_-]{0,63}$/;
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/;
const SCHEMA_FILE = { in: 'schema/in.json', out: 'schema/out.json' } as const;
const SETTINGS_FILE = 'schema/launch_args.json';
const DOCS_FILE = 'README.md';
/** Manifest fields the wizard writes; everything else is kept in `extra`. */
const OWN = new Set([
  'manifest_version', 'id', 'name', 'version', 'type', 'description', 'launch',
  'payload_schemas', 'launch_args_schema', 'configurator', 'ui',
]);

export const newRow = (type: FieldType = 'STRING'): FieldRow => ({
  key: '',
  type,
  description: '',
  content_type: '',
  element: type === 'ARRAY' || type === 'MAP' ? newRow() : null,
  fields: [],
});

const newSide = (side: 'in' | 'out'): Side => ({
  perNode: false,
  pointer: `/${side}_schema`,
  mode: 'fields',
  fields: [],
  json: '',
});

export function emptyState(): WizardState {
  return {
    id: '',
    name: '',
    version: '1.0.0',
    type: 'SOURCE',
    description: '',
    in: newSide('in'),
    out: newSide('out'),
    settings: '',
    configured: [],
    categories: '',
    color: '',
    docs: '',
    icon: null,
    extra: {},
  };
}

/** The payload sides a type has. */
export const sidesOf = (type: NodeType): ('in' | 'out')[] =>
  type === 'SOURCE' ? ['out'] : type === 'SINK' ? ['in'] : ['in', 'out'];

// ---- field rows ↔ SchemaDefinition (proto3 JSON) ----

type Def = Record<string, unknown>;

function rowToDef(row: FieldRow, keyed: boolean): Def {
  const def: Def = { type: row.type };
  if (keyed) def.key = row.key;
  if (row.description.trim()) def.description = row.description.trim();
  if (row.type === 'BYTES' && row.content_type.trim()) def.content_type = row.content_type.trim();
  if ((row.type === 'ARRAY' || row.type === 'MAP') && row.element) def.nested_type_definition = rowToDef(row.element, false);
  if (row.type === 'OBJECT') def.object_fields = row.fields.map((f) => rowToDef(f, true));
  return def;
}

export const rowsToSchema = (rows: FieldRow[]) => ({ fields: rows.map((r) => rowToDef(r, true)) });

function defToRow(def: Def): FieldRow {
  const get = (snake: string, camel: string) => def[snake] ?? def[camel];
  const type = (FIELD_TYPES as readonly string[]).includes(String(def.type)) ? (def.type as FieldType) : 'STRING';
  const nested = get('nested_type_definition', 'nestedTypeDefinition') as Def | undefined;
  const fields = (get('object_fields', 'objectFields') as Def[] | undefined) ?? [];
  return {
    key: String(def.key ?? ''),
    type,
    description: String(def.description ?? ''),
    content_type: String(get('content_type', 'contentType') ?? ''),
    element: type === 'ARRAY' || type === 'MAP' ? (nested ? defToRow(nested) : newRow()) : null,
    fields: fields.map(defToRow),
  };
}

/** A SchemaDefinition's fields as rows (null: not one the editor can show). */
export function schemaToRows(schema: unknown): FieldRow[] | null {
  if (!schema || typeof schema !== 'object' || !Array.isArray((schema as Def).fields)) return null;
  return ((schema as Def).fields as Def[]).map(defToRow);
}

/** Problems with a level of rows: missing or repeated keys, nested too. */
export function rowProblems(rows: FieldRow[], path = ''): string[] {
  const problems: string[] = [];
  const seen = new Set<string>();
  rows.forEach((row, i) => {
    const at = path ? `${path}.${row.key || `#${i + 1}`}` : row.key || `#${i + 1}`;
    if (!row.key.trim()) problems.push(`${at}: a field needs a key`);
    else if (seen.has(row.key)) problems.push(`${at}: the key is used twice`);
    seen.add(row.key);
    problems.push(...elementProblems(row, at));
  });
  return problems;
}

function elementProblems(row: FieldRow, at: string): string[] {
  if (row.type === 'OBJECT') return rowProblems(row.fields, at);
  if ((row.type === 'ARRAY' || row.type === 'MAP') && row.element) return elementProblems(row.element, `${at}[]`);
  return [];
}

// ---- the whole definition ----

function sideSchema(side: Side): { schema: unknown | null; error: string | null } {
  if (side.mode === 'json') {
    if (!side.json.trim()) return { schema: null, error: null };
    try {
      return { schema: JSON.parse(side.json), error: null };
    } catch (e) {
      return { schema: null, error: `not valid JSON: ${(e as Error).message}` };
    }
  }
  return { schema: side.fields.length ? rowsToSchema(side.fields) : null, error: null };
}

/** The launch-args schema as an object, or null (an error for bad JSON). */
export function parseSettings(text: string): { schema: Record<string, unknown> | null; error: string | null } {
  if (!text.trim()) return { schema: null, error: null };
  try {
    const value = JSON.parse(text);
    if (!value || typeof value !== 'object' || Array.isArray(value)) return { schema: null, error: 'must be a JSON object (a JSON Schema)' };
    return { schema: value, error: null };
  } catch (e) {
    return { schema: null, error: `not valid JSON: ${(e as Error).message}` };
  }
}

/** The top-level settings a launch-args schema declares. */
export const topSettings = (schema: Record<string, unknown> | null): string[] =>
  Object.keys((schema?.properties as Record<string, unknown> | undefined) ?? {});

/** The definition, and what's wrong before the server even sees it. */
export function toDefinition(state: WizardState): { definition: Definition; problems: string[] } {
  const problems: string[] = [];
  if (!ID_PATTERN.test(state.id)) problems.push('Id: lowercase letters, digits, - and _, starting with a letter or digit (at most 64)');
  if (!state.name.trim()) problems.push('Name is empty');
  if (!SEMVER.test(state.version)) problems.push('Version: a semantic version such as 1.0.0');
  const files: Record<string, DefinitionFile> = {};
  const manifest: Record<string, unknown> = {
    ...state.extra,
    manifest_version: 2,
    id: state.id,
    name: state.name.trim(),
    version: state.version,
    type: state.type,
    launch: 'remote',
  };
  if (state.description.trim()) manifest.description = state.description.trim();

  const parsed = parseSettings(state.settings);
  if (parsed.error) problems.push(`Node settings: ${parsed.error}`);
  let settings = parsed.schema ? structuredClone(parsed.schema) : null;
  const payload: Record<string, unknown> = {};
  for (const name of sidesOf(state.type)) {
    const side = state[name];
    const { schema, error } = sideSchema(side);
    const label = name === 'in' ? 'Input payload' : 'Output payload';
    if (error) problems.push(`${label}: ${error}`);
    if (side.mode === 'fields') problems.push(...rowProblems(side.fields).map((p) => `${label}: ${p}`));
    if (schema) files[SCHEMA_FILE[name]] = { json: schema };
    if (side.perNode) {
      // The node's own schema sits in its settings at the pointer: make sure
      // the settings declare it (a top-level object property).
      const key = side.pointer.replace(/^\//, '');
      if (!key || key.includes('/')) problems.push(`${label}: the settings key must be a top-level name`);
      settings ??= { type: 'object', properties: {} };
      const props = ((settings.properties ??= {}) as Record<string, unknown>);
      props[key] ??= {
        type: 'object',
        title: name === 'in' ? 'Input schema' : 'Output schema',
        description: `This node's ${name === 'in' ? 'input' : 'output'} payload schema (a SchemaDefinition).`,
      };
      payload[name] = schema ? { from_launch_args: side.pointer, default: SCHEMA_FILE[name] } : { from_launch_args: side.pointer };
    } else if (schema) {
      payload[name] = SCHEMA_FILE[name];
    } else {
      problems.push(`${label}: add at least one field (or let each node set its own)`);
    }
  }
  manifest.payload_schemas = payload;
  if (settings) {
    files[SETTINGS_FILE] = { json: settings };
    manifest.launch_args_schema = SETTINGS_FILE;
    const known = topSettings(settings);
    const configured = state.configured.filter((f) => known.includes(f));
    if (configured.length) manifest.configurator = { fields: configured };
  }

  const ui: Record<string, unknown> = {};
  const categories = state.categories.split(',').map((c) => c.trim()).filter(Boolean);
  if (categories.length) ui.categories = [...new Set(categories)];
  if (state.color) ui.color = state.color;
  if (state.docs.trim()) {
    ui.docs = DOCS_FILE;
    files[DOCS_FILE] = { text: state.docs };
  }
  if (state.icon) {
    const path = `ui/${state.icon.name}`;
    ui.icon = path;
    files[path] = state.icon.file;
  }
  if (Object.keys(ui).length) manifest.ui = ui;
  return { definition: { manifest, files }, problems };
}

/** The wizard's state for an existing definition. */
export function fromDefinition(def: Definition): WizardState {
  const m = def.manifest;
  const state = emptyState();
  const file = (path: unknown) => (typeof path === 'string' ? def.files[path] : undefined);
  const json = (path: unknown): unknown => {
    const f = file(path);
    if (!f) return undefined;
    if ('json' in f) return f.json;
    if ('text' in f) {
      try {
        return JSON.parse(f.text);
      } catch {
        return undefined;
      }
    }
    return undefined;
  };
  state.id = String(m.id ?? '');
  state.name = String(m.name ?? '');
  state.version = String(m.version ?? '');
  state.type = (m.type as NodeType) ?? 'SOURCE';
  state.description = String(m.description ?? '');
  const payload = (m.payload_schemas ?? {}) as Record<string, unknown>;
  for (const name of ['in', 'out'] as const) {
    const source = payload[name];
    const side = state[name];
    let schema: unknown;
    if (typeof source === 'string') schema = json(source);
    else if (source && typeof source === 'object') {
      const s = source as Record<string, unknown>;
      side.perNode = true;
      side.pointer = String(s.from_launch_args ?? side.pointer);
      schema = json(s.default);
    }
    const rows = schema === undefined ? [] : schemaToRows(schema);
    if (rows) side.fields = rows;
    else {
      side.mode = 'json';
      side.json = JSON.stringify(schema, null, 2);
    }
  }
  const settings = json(m.launch_args_schema);
  if (settings) {
    // The per-node schema properties the wizard adds come back with it.
    state.settings = JSON.stringify(settings, null, 2);
  }
  state.configured = (((m.configurator as Record<string, unknown> | undefined)?.fields as string[] | undefined) ?? []).slice();
  const ui = (m.ui ?? {}) as Record<string, unknown>;
  state.categories = ((ui.categories as string[] | undefined) ?? []).join(', ');
  state.color = String(ui.color ?? '');
  const docs = file(ui.docs);
  if (docs && 'text' in docs) state.docs = docs.text;
  const icon = file(ui.icon);
  if (icon && typeof ui.icon === 'string') state.icon = { name: ui.icon.split('/').pop() ?? 'icon.svg', file: icon };
  for (const [k, v] of Object.entries(m)) if (!OWN.has(k)) state.extra[k] = v;
  return state;
}

/** An icon file picked by the user, as a definition file. */
export async function iconFile(file: File): Promise<{ name: string; file: DefinitionFile }> {
  const lower = file.name.toLowerCase();
  const svg = lower.endsWith('.svg') || file.type === 'image/svg+xml';
  if (!svg && !(lower.endsWith('.png') || file.type === 'image/png')) throw new Error('The icon must be an .svg or .png file');
  if (file.size > 1024 * 1024) throw new Error('The icon must be at most 1 MiB');
  if (svg) return { name: 'icon.svg', file: { text: await file.text() } };
  const bytes = new Uint8Array(await file.arrayBuffer());
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return { name: 'icon.png', file: { base64: btoa(binary) } };
}

/** A data URL to preview a definition's icon. */
export function iconPreview(icon: WizardState['icon']): string | null {
  if (!icon) return null;
  if ('text' in icon.file) return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(icon.file.text)}`;
  if ('base64' in icon.file) return `data:image/png;base64,${icon.file.base64}`;
  return null;
}
