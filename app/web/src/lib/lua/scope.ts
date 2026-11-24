// The scope a Lua code field sees.

import type { SchemaField } from '../sceneView';
import { META, sandboxScope } from './builtins';
import { addDefinitions, signatureParams } from './luacats';
import { plain, schemaType, type LuaType, type Scope } from './types';

/** An edge mapping's Lua (filter, rules, script): `payload` (the source's schema) and `meta`. */
export function mappingScope(source: SchemaField[] | null): Scope {
  const scope = sandboxScope();
  const payload = source ? schemaType(source, 'In') : plain('table');
  if (source) scope.classes.set('In', payload);
  scope.globals.set('payload', { name: 'payload', type: payload, doc: 'The event’s payload (read-only), shaped by the source node’s schema.' });
  scope.globals.set('meta', { name: 'meta', type: META, doc: 'The event’s metadata (read-only).' });
  return scope;
}

export interface CodeField {
  language: string;
  params?: string[];
  signature?: string;
}

/**
 * A node setting's code (`x-flwrs-code`): the sandbox, the plugin's
 * definitions file (its own globals and classes), `In` / `Out` as the node's
 * payload schemas, and the parameters typed by the field's `signature`.
 */
export function codeFieldScope(
  code: CodeField,
  definitions: string | undefined,
  schemas: { in: SchemaField[] | null; out: SchemaField[] | null } | null,
): Scope {
  const scope = sandboxScope();
  if (schemas?.in) scope.classes.set('In', schemaType(schemas.in, 'In'));
  if (schemas?.out) scope.classes.set('Out', schemaType(schemas.out, 'Out'));
  if (definitions) addDefinitions(scope, definitions);
  const types = signatureParams(code.signature);
  for (const name of code.params ?? []) {
    const text = types.get(name) ?? 'any';
    const bare = text.replace(/\?$/, '').split('|')[0].trim();
    // `Name[]`: a list of a class (iterated with ipairs, indexed).
    const list = /^(\w+)\[\]$/.exec(bare);
    const type: LuaType = scope.classes.has(bare)
      ? { kind: 'named', name: bare }
      : list && scope.classes.has(list[1])
        ? { kind: 'list', element: { kind: 'named', name: list[1] } }
        : plain(text);
    scope.globals.set(name, { name, type, doc: `Parameter of the code: ${name}: ${text}` });
  }
  return scope;
}
