// A small type model of what a Lua code field can see, for completion and
// hover docs: tables with fields, functions with signatures, classes by
// name (from LuaCATS `---@class`), and plain types. Built from the
// sandbox's libraries (`builtins.ts`), a plugin's LuaCATS definitions
// (`luacats.ts`) and payload schemas (`schemaType`).

import type { SchemaField } from '../sceneView';

export interface Param {
  name: string;
  type: string;
  optional?: boolean;
  doc?: string;
}

export interface Signature {
  params: Param[];
  returns: string[];
}

export type LuaType =
  | { kind: 'table'; fields: Map<string, Member>; name?: string }
  | { kind: 'function'; sig: Signature }
  /** A class (or alias) by name, resolved in the scope's classes. */
  | { kind: 'named'; name: string }
  /** An array (index → element) or map (key → value). */
  | { kind: 'list'; element: LuaType; map?: boolean }
  | { kind: 'plain'; name: string };

export interface Member {
  name: string;
  type: LuaType;
  doc?: string;
}

export interface Scope {
  /** Globals, function parameters included. */
  globals: Map<string, Member>;
  /** Classes (`---@class`) and aliases by name. */
  classes: Map<string, LuaType>;
}

export const plain = (name: string): LuaType => ({ kind: 'plain', name });
export const table = (members: Member[], name?: string): LuaType => ({
  kind: 'table',
  fields: new Map(members.map((m) => [m.name, m])),
  name,
});
export const fn = (params: Param[], returns: string[] = []): LuaType => ({ kind: 'function', sig: { params, returns } });

/** `name?` / `name` → a parameter. */
export function param(spec: string, type = 'any', doc?: string): Param {
  const optional = spec.endsWith('?');
  return { name: optional ? spec.slice(0, -1) : spec, type, optional, doc };
}

/** A type's name as LuaCATS would write it. */
export function typeName(t: LuaType): string {
  switch (t.kind) {
    case 'plain':
      return t.name;
    case 'named':
      return t.name;
    case 'table':
      return t.name ?? 'table';
    case 'list':
      return t.map ? `table<string, ${typeName(t.element)}>` : `${typeName(t.element)}[]`;
    case 'function':
      return signatureText('fun', t.sig);
  }
}

export function signatureText(name: string, sig: Signature): string {
  const params = sig.params.map((p) => `${p.name}${p.optional ? '?' : ''}: ${p.type}`).join(', ');
  return `${name}(${params})${sig.returns.length ? `: ${sig.returns.join(', ')}` : ''}`;
}

/** Follows names to their class (a few steps at most). */
export function resolveType(scope: Scope, t: LuaType | undefined): LuaType | undefined {
  let current = t;
  for (let i = 0; i < 8 && current?.kind === 'named'; i++) {
    const target: LuaType | undefined = scope.classes.get(current.name.replace(/\?$/, '').split('|')[0].trim());
    if (!target) return current;
    current = target;
  }
  return current;
}

/** A payload schema as the Lua table a code field gets. */
export function schemaType(fields: SchemaField[], name?: string): LuaType {
  return table(fields.map((f) => ({ name: f.key, type: fieldType(f), doc: fieldDoc(f) })), name);
}

function fieldType(f: SchemaField): LuaType {
  switch (f.type) {
    case 'STRING':
      return plain('string');
    case 'BYTES':
      return plain('string (bytes)');
    case 'BOOL':
      return plain('boolean');
    case 'DATETIME':
      return plain('integer (epoch ms)');
    case 'I32':
    case 'U32':
    case 'I64':
    case 'U64':
      return plain('integer');
    case 'F32':
    case 'F64':
      return plain('number');
    case 'OBJECT':
      return schemaType(f.fields ?? []);
    case 'ARRAY':
      return { kind: 'list', element: f.element ? fieldType(f.element) : plain('any') };
    case 'MAP':
      return { kind: 'list', element: f.element ? fieldType(f.element) : plain('any'), map: true };
    default:
      return f.type.startsWith('PACKED_') ? { kind: 'list', element: plain(/F(32|64)$/.test(f.type) ? 'number' : 'integer') } : plain('any');
  }
}

function fieldDoc(f: SchemaField): string | undefined {
  const parts = [f.description, f.content_type ? `Content type: ${f.content_type}` : undefined, `Schema type: ${f.type}`];
  return parts.filter(Boolean).join('\n');
}
