// A plugin's launch-args schema as a form: the JSON Schema subset the form
// renders (types, enums, nested objects, arrays of scalars; anything else as
// JSON text) plus the flwrs annotations (docs/plugin-spec.md §3): secrets
// (`writeOnly`), uploads and delivery (`x-flwrs-launch`), code
// (`x-flwrs-code`), groups, order, advanced and widgets.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface Schema {
  type?: string | string[];
  title?: string;
  description?: string;
  default?: Json;
  enum?: Json[];
  const?: Json;
  properties?: Record<string, Schema>;
  required?: string[];
  items?: Schema;
  oneOf?: Schema[];
  minimum?: number;
  maximum?: number;
  exclusiveMinimum?: number;
  exclusiveMaximum?: number;
  minLength?: number;
  maxLength?: number;
  pattern?: string;
  format?: string;
  contentMediaType?: string;
  writeOnly?: boolean;
  $ref?: string;
  'x-flwrs-launch'?: { via?: 'arg' | 'env' | 'file'; alias?: string; file_name?: string; source?: 'upload' };
  'x-flwrs-code'?: { language: string; params?: string[]; signature?: string; definitions?: string };
  'x-flwrs-group'?: string;
  'x-flwrs-order'?: number;
  'x-flwrs-advanced'?: boolean;
  'x-flwrs-widget'?: 'duration' | 'cron' | 'file-path' | 'code';
  'x-flwrs-listen'?: boolean;
  /** The platforms the property applies to (`macos`, `linux-aarch64`). */
  'x-flwrs-platforms'?: string[];
  /** A package file an editor fills the empty field with. */
  'x-flwrs-template'?: string;
  'x-flwrs-graph'?: { catalog: string };
  [key: string]: unknown;
}

/** Whether two JSON values are equal, whatever the order of object keys. */
export function sameJson(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const list = b as unknown[];
    return a.length === list.length && a.every((v, i) => sameJson(v, list[i]));
  }
  const x = a as Record<string, unknown>;
  const y = b as Record<string, unknown>;
  const keys = Object.keys(x);
  return keys.length === Object.keys(y).length && keys.every((k) => k in y && sameJson(x[k], y[k]));
}

/** Follows local `$ref`s (`#/…`) against the schema's root, cycle-safe. */
export function resolve(root: Schema, schema: Schema): Schema {
  let current = schema;
  const seen = new Set<string>();
  while (current.$ref && current.$ref.startsWith('#') && !seen.has(current.$ref)) {
    seen.add(current.$ref);
    const target = current.$ref
      .slice(1)
      .split('/')
      .filter(Boolean)
      .reduce<unknown>((at, part) => (at as Record<string, unknown> | undefined)?.[part.replace(/~1/g, '/').replace(/~0/g, '~')], root);
    if (!target || typeof target !== 'object') break;
    const { $ref: _, ...rest } = current;
    current = { ...(target as Schema), ...rest };
  }
  return current;
}

/** The schema's main type (`null` left out of a type list). */
export function typeOf(schema: Schema): string | undefined {
  if (Array.isArray(schema.type)) return schema.type.find((t) => t !== 'null');
  if (schema.type) return schema.type;
  if (schema.properties) return 'object';
  if (schema.enum?.length) return typeof schema.enum[0] === 'string' ? 'string' : undefined;
  return undefined;
}

export type Kind = 'string' | 'integer' | 'number' | 'boolean' | 'enum' | 'object' | 'list' | 'code' | 'document' | 'json';

/** How the form shows a (resolved) property. */
export function kindOf(schema: Schema): Kind {
  if (schema.enum?.length && schema.enum.every((v) => typeof v === 'string' || typeof v === 'number')) return 'enum';
  if (schema['x-flwrs-code'] || schema['x-flwrs-widget'] === 'code') return 'code';
  // A document as an object or its text (e.g. an OpenAPI document, JSON or
  // YAML): edited as text.
  const types = Array.isArray(schema.type) ? schema.type : [];
  if (types.includes('object') && types.includes('string') && !Object.keys(schema.properties ?? {}).length) return 'document';
  const type = typeOf(schema);
  switch (type) {
    case 'string':
    case 'integer':
    case 'number':
    case 'boolean':
      return type;
    case 'object':
      // A free-form object (e.g. a payload schema) is edited as JSON.
      return schema.properties && Object.keys(schema.properties).length ? 'object' : 'json';
    case 'array': {
      const items = schema.items ? typeOf(schema.items) : undefined;
      return items === 'string' || items === 'integer' || items === 'number' ? 'list' : 'json';
    }
    default:
      return 'json';
  }
}

/** The context key of the platform a form's node runs on (a getter; `null`: unknown, nothing hidden). */
export const PLATFORM_CONTEXT = 'flwrs.platform';

/** Whether a property applies on `platform` (`<os>-<arch>`); unknown = yes. */
export function appliesOn(schema: Schema, platform: string | null): boolean {
  const list = schema['x-flwrs-platforms'];
  if (!platform || !list?.length) return true;
  return list.includes(platform) || list.includes(platform.split('-')[0]);
}

export interface Variant {
  value: string;
  label: string;
  schema: Schema;
}

/** A `oneOf` of objects told apart by one `const` string property, as a choice and its fields. */
export function variantsOf(root: Schema, schema: Schema): { tag: string; options: Variant[] } | null {
  if (!schema.oneOf?.length) return null;
  const branches = schema.oneOf.map((b) => resolve(root, b));
  if (!branches.every((b) => b.properties)) return null;
  const tag = Object.keys(branches[0].properties!).find((key) =>
    branches.every((b) => {
      const p = b.properties![key];
      return p && typeof resolve(root, p).const === 'string';
    }),
  );
  if (!tag) return null;
  return {
    tag,
    options: branches.map((b) => {
      const value = resolve(root, b.properties![tag]).const as string;
      return { value, label: b.title ?? value, schema: b };
    }),
  };
}

export interface Field {
  key: string;
  schema: Schema;
  required: boolean;
  /** `writeOnly`: the value is never shown. */
  secret: boolean;
  /** `"source": "upload"`: the value names one of the node's uploads. */
  upload: boolean;
  /** An upload field holding a list of names (an array of strings). */
  uploadList: boolean;
  /** Set by the plugin's configurator (local nodes). */
  configured: boolean;
  advanced: boolean;
  group: string | null;
}

/** The top-level properties as form fields, in groups, in order. */
export function topFields(
  root: Schema,
  configured: string[],
  platform: string | null = null,
  values: Record<string, unknown> = {},
): { group: string | null; advanced: boolean; fields: Field[] }[] {
  const required = new Set(root.required ?? []);
  // A value already set stays visible, so it can be seen and cleared.
  const entries = Object.entries(root.properties ?? {}).filter(
    ([key, raw]) => values[key] !== undefined || appliesOn(resolve(root, raw), platform),
  );
  const fields: Field[] = entries.map(([key, raw]) => {
    const schema = resolve(root, raw);
    return {
      key,
      schema,
      required: required.has(key),
      secret: schema.writeOnly === true,
      upload: schema['x-flwrs-launch']?.source === 'upload',
      uploadList: schema['x-flwrs-launch']?.source === 'upload' && schema.type === 'array',
      configured: configured.includes(key),
      advanced: schema['x-flwrs-advanced'] === true,
      group: schema['x-flwrs-group'] ?? null,
    };
  });
  const order = (f: Field) => f.schema['x-flwrs-order'] ?? Number.MAX_SAFE_INTEGER;
  fields.sort((a, b) => order(a) - order(b) || Number(b.required) - Number(a.required));
  // Groups appear where their first field would; advanced fields go last.
  const groups: { group: string | null; advanced: boolean; fields: Field[] }[] = [];
  for (const advanced of [false, true]) {
    for (const field of fields.filter((f) => f.advanced === advanced)) {
      let group = groups.find((g) => g.group === field.group && g.advanced === advanced);
      if (!group) groups.push((group = { group: field.group, advanced, fields: [] }));
      group.fields.push(field);
    }
  }
  return groups;
}

/** A property's label: its `title`, else its key in words. */
export function labelOf(key: string, schema: Schema): string {
  if (schema.title) return schema.title;
  const words = key.replace(/[_-]+/g, ' ').trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** A hint from the constraints the form doesn't show otherwise. */
export function constraintHint(schema: Schema): string | null {
  const parts: string[] = [];
  if (schema.minimum !== undefined) parts.push(`≥ ${schema.minimum}`);
  if (schema.exclusiveMinimum !== undefined) parts.push(`> ${schema.exclusiveMinimum}`);
  if (schema.maximum !== undefined) parts.push(`≤ ${schema.maximum}`);
  if (schema.exclusiveMaximum !== undefined) parts.push(`< ${schema.exclusiveMaximum}`);
  if (schema.minLength !== undefined && schema.minLength > 0) parts.push(`at least ${schema.minLength} characters`);
  if (schema.maxLength !== undefined) parts.push(`at most ${schema.maxLength} characters`);
  if (schema['x-flwrs-widget'] === 'duration') parts.push('a duration, e.g. 500ms, 10s, 5m');
  if (schema['x-flwrs-widget'] === 'cron') parts.push('a cron expression, e.g. */5 * * * *');
  if (schema['x-flwrs-listen']) parts.push('host:port');
  return parts.length ? parts.join(' · ') : null;
}

// ---- errors ----

export interface FieldErrors {
  /** By top-level launch-args key. */
  args: Map<string, string[]>;
  runtime: string[];
  remote: string[];
  other: string[];
}

/**
 * Sorts the server's messages about a node (a refused save's details, or
 * the scene check's lines about it) by the part of the form they're about.
 */
export function sortErrors(messages: string[], nodeId?: string): FieldErrors {
  const out: FieldErrors = { args: new Map(), runtime: [], remote: [], other: [] };
  for (const raw of messages) {
    // The check's lines start with `node <id> (<name>): `.
    let message = raw;
    if (nodeId && message.startsWith(`node ${nodeId}`)) {
      const colon = message.indexOf(': ');
      if (colon >= 0) message = message.slice(colon + 2);
    }
    // `launch_args/<key>…: …`, or a missing top-level one: `launch_args: "<key>" is a required property`.
    const arg =
      /^(?:config\.)?launch_args\/([^/:\s]+)/.exec(message) ?? /^(?:config\.)?launch_args: "([^"]+)" is a required property/.exec(message);
    if (arg) {
      const key = arg[1].replace(/~1/g, '/').replace(/~0/g, '~');
      out.args.set(key, [...(out.args.get(key) ?? []), message]);
    } else if (/^config\.runtime\b/.test(message)) {
      out.runtime.push(message);
    } else if (/^config\.(token|remote_id|auth|tls|require_tls|configurator)\b/.test(message)) {
      out.remote.push(message);
    } else {
      out.other.push(message);
    }
  }
  return out;
}

// ---- values ----

/** A copy of a JSON value. */
export const clone = <T>(value: T): T => (value === undefined ? value : JSON.parse(JSON.stringify(value)));

/** A random token for a remote node: 256 bits as hex. */
export function newToken(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  return [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/** Sets `path` in a nested object (`undefined` removes it; empty objects go too). */
export function setPath(target: Record<string, unknown>, path: string[], value: unknown): Record<string, unknown> {
  const copy = { ...target };
  const [head, ...rest] = path;
  if (!rest.length) {
    if (value === undefined) delete copy[head];
    else copy[head] = value;
    return copy;
  }
  const child = setPath((copy[head] as Record<string, unknown>) ?? {}, rest, value);
  if (Object.keys(child).length) copy[head] = child;
  else delete copy[head];
  return copy;
}

export function getPath(target: unknown, path: string[]): unknown {
  return path.reduce<unknown>((at, key) => (at && typeof at === 'object' ? (at as Record<string, unknown>)[key] : undefined), target);
}
