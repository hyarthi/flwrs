// Completion and hover docs for Lua code fields (CodeMirror), from a
// `Scope`: globals and parameters, members after `.` (tables, classes,
// payload schemas; `[…]` steps into arrays and maps), string methods after
// `:`, keywords, and the code's own locals.

import type { Completion, CompletionContext, CompletionResult } from '@codemirror/autocomplete';
import { KEYWORDS, STRING_METHODS } from './builtins';
import { resolveType, signatureText, typeName, type LuaType, type Member, type Scope, type Signature } from './types';

/** A chain like `payload.items[1].name` before the cursor, split in steps. */
const CHAIN = /([A-Za-z_]\w*(?:\s*(?:\.\s*[A-Za-z_]\w*|\[[^\]\n]*\]|:\s*[A-Za-z_]\w*))*)\s*([.:])\s*(\w*)$/;

type Step = { name: string } | { index: true };

function steps(chain: string): { root: string; rest: Step[] } {
  const parts = chain.match(/[A-Za-z_]\w*|\[[^\]]*\]/g) ?? [];
  const [root = '', ...rest] = parts;
  return { root, rest: rest.map((p) => (p.startsWith('[') ? { index: true } : { name: p })) };
}

/** What a chain refers to, as far as the scope knows. */
export function resolveChain(scope: Scope, chain: string): LuaType | undefined {
  const { root, rest } = steps(chain);
  let t = resolveType(scope, scope.globals.get(root)?.type);
  for (const step of rest) {
    if (!t) return undefined;
    if ('index' in step) t = t.kind === 'list' ? resolveType(scope, t.element) : undefined;
    else t = t.kind === 'table' ? resolveType(scope, t.fields.get(step.name)?.type) : undefined;
  }
  return t;
}

function membersOf(scope: Scope, t: LuaType | undefined): Member[] {
  const r = resolveType(scope, t);
  return r?.kind === 'table' ? [...r.fields.values()] : [];
}

function option(m: Member, boost = 0): Completion {
  const isFn = m.type.kind === 'function';
  return {
    label: m.name,
    type: isFn ? 'function' : m.type.kind === 'table' ? 'namespace' : 'property',
    detail: isFn ? signatureText('', (m.type as Extract<LuaType, { kind: 'function' }>).sig).replace(/^\(/, '(') : typeName(m.type),
    info: m.doc,
    boost,
  };
}

/** Names the code itself declares: locals, function parameters, loop variables. */
export function localNames(code: string): string[] {
  const names = new Set<string>();
  for (const m of code.matchAll(/\blocal\s+(?:function\s+)?([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)/g)) {
    for (const n of m[1].split(',')) names.add(n.trim());
  }
  for (const m of code.matchAll(/\bfunction\b[^(]*\(([^)]*)\)/g)) {
    for (const n of m[1].split(',')) if (/^[A-Za-z_]\w*$/.test(n.trim())) names.add(n.trim());
  }
  for (const m of code.matchAll(/\bfor\s+([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s*(?:=|in\b)/g)) {
    for (const n of m[1].split(',')) names.add(n.trim());
  }
  return [...names];
}

const CHAIN_TEXT = String.raw`[A-Za-z_]\w*(?:\s*(?:\.\s*[A-Za-z_]\w*|\[[^\]\n]*\]|:\s*[A-Za-z_]\w*))*`;

/** A type from a return type's text: a class name links to it. */
function fromText(scope: Scope, text: string | undefined): LuaType | undefined {
  if (!text) return undefined;
  const bare = text.replace(/\?$/, '').split('|')[0].trim();
  if (scope.classes.has(bare)) return { kind: 'named', name: bare };
  return bare === 'any' || bare === 'nil' ? undefined : { kind: 'plain', name: text };
}

/** What an expression evaluates to, roughly: a chain, or a call's first return type. */
function typeOfExpression(scope: Scope, expr: string): LuaType | undefined {
  const e = expr.trim().replace(/\s+or\s+[\s\S]*$/, '');
  const call = new RegExp(`^(${CHAIN_TEXT})\\s*\\(`).exec(e);
  if (call) {
    const f = memberOf(scope, call[1]);
    return f?.type.kind === 'function' ? fromText(scope, f.type.sig.returns[0]) : undefined;
  }
  if (new RegExp(`^${CHAIN_TEXT}$`).test(e)) return resolveChain(scope, e);
  if (/^["']/.test(e)) return { kind: 'plain', name: 'string' };
  if (/^-?\d/.test(e)) return { kind: 'plain', name: /^-?\d+$/.test(e) ? 'integer' : 'number' };
  if (/^(true|false)$/.test(e)) return { kind: 'plain', name: 'boolean' };
  return undefined;
}

/**
 * The scope with the code's locals that have a known type (`local x =
 * payload.items`, `for i, v in ipairs(payload.items)`), in order, as of the
 * end of `code` (later assignments win). Also the names it added.
 */
export function withLocals(scope: Scope, code: string): { scope: Scope; locals: Set<string> } {
  const globals = new Map(scope.globals);
  const next: Scope = { globals, classes: scope.classes };
  const locals = new Set<string>();
  const set = (name: string, type: LuaType | undefined, doc: string) => {
    locals.add(name);
    if (type) globals.set(name, { name, type, doc });
    else globals.delete(name);
  };
  const statement = new RegExp(
    String.raw`\blocal\s+([A-Za-z_]\w*)(?:\s*<\w+>)?\s*=\s*([^\n;]+)|\bfor\s+([A-Za-z_]\w*)\s*,\s*([A-Za-z_]\w*)\s+in\s+(i?pairs)\s*\(\s*(${CHAIN_TEXT})\s*\)`,
    'g',
  );
  for (const m of code.matchAll(statement)) {
    if (m[1]) {
      set(m[1], typeOfExpression(next, m[2]), `local ${m[1]} = ${m[2].trim()}`);
    } else {
      const over = resolveType(next, resolveChain(next, m[6]));
      const element = over?.kind === 'list' ? over.element : undefined;
      set(m[3], { kind: 'plain', name: m[5] === 'ipairs' ? 'integer' : over?.kind === 'list' && over.map ? 'string' : 'any' }, `loop key over ${m[6]}`);
      set(m[4], element, `loop value over ${m[6]}`);
    }
  }
  return { scope: next, locals };
}

/** The member a chain names (`json.encode`, `s:upper` → string's `upper`). */
function memberOf(scope: Scope, chain: string): Member | undefined {
  const { root, rest } = steps(chain);
  const last = rest.at(-1);
  if (!last) return scope.globals.get(root);
  if (!('name' in last)) return undefined;
  const cut = chain.lastIndexOf(last.name);
  const sep = chain.slice(0, cut).trimEnd().slice(-1);
  const owner = chain.slice(0, cut).replace(/\s*[.:]\s*$/, '');
  if (sep === ':') {
    const m = STRING_METHODS.find((s) => s.name === last.name);
    return m && m.type.kind === 'function' ? { ...m, type: { ...m.type, sig: { ...m.type.sig, params: m.type.sig.params.slice(1) } } } : undefined;
  }
  const r = resolveType(scope, resolveChain(scope, owner));
  return r?.kind === 'table' ? r.fields.get(last.name) : undefined;
}

/**
 * Signature help: the call the cursor is in (`store("k", |`), its
 * signature and which parameter the cursor is at.
 */
export function signatureAt(
  base: Scope,
  code: string,
  pos: number,
): { name: string; sig: Signature; active: number; doc?: string } | null {
  let depth = 0;
  let commas = 0;
  let open = -1;
  for (let i = pos - 1; i >= 0 && pos - i < 4000; i--) {
    const c = code[i];
    if (c === ')' || c === ']' || c === '}') depth++;
    else if (c === '(' || c === '[' || c === '{') {
      if (depth === 0) {
        if (c === '(') open = i;
        break;
      }
      depth--;
    } else if (c === ',' && depth === 0) commas++;
    else if ((c === '"' || c === "'") && depth === 0) {
      // Skip back over a string literal on the same line.
      const start = code.lastIndexOf(c, i - 1);
      if (start >= 0 && !code.slice(start, i).includes('\n')) i = start;
    }
  }
  if (open < 0) return null;
  const head = new RegExp(`(${CHAIN_TEXT})\\s*$`).exec(code.slice(0, open));
  if (!head || KEYWORDS.includes(head[1])) return null;
  const { scope } = withLocals(base, code.slice(0, open));
  const member = memberOf(scope, head[1]);
  if (!member || member.type.kind !== 'function') return null;
  const sig = member.type.sig;
  const variadic = sig.params.findIndex((p) => p.name === '...');
  const active = variadic >= 0 && commas >= variadic ? variadic : Math.min(commas, Math.max(sig.params.length - 1, 0));
  return { name: head[1].replace(/\s+/g, ''), sig, active, doc: member.doc };
}

/** A CodeMirror completion source over the scope `get()` returns now. */
export function luaCompletions(get: () => Scope | null) {
  return (context: CompletionContext): CompletionResult | null => {
    const base = get();
    if (!base) return null;
    const { scope, locals: typedLocals } = withLocals(base, context.state.doc.sliceString(0, context.pos));
    const line = context.state.doc.lineAt(context.pos);
    const before = line.text.slice(0, context.pos - line.from);
    // Not inside a comment or a string (roughly: an odd number of quotes).
    if (/--/.test(before.replace(/(["'])(?:\\.|(?!\1).)*\1/g, ''))) return null;
    if (((before.match(/"/g)?.length ?? 0) % 2) || ((before.match(/'/g)?.length ?? 0) % 2)) return null;

    const access = CHAIN.exec(before);
    if (access) {
      const [, chain, sep, partial] = access;
      const from = context.pos - partial.length;
      if (sep === ':') {
        // Methods: strings have string's functions (`s:upper()`).
        const t = resolveChain(scope, chain);
        const isString = !t || (t.kind === 'plain' && t.name.startsWith('string'));
        // As methods, the string is the first argument.
        const methods = STRING_METHODS.map((m) =>
          m.type.kind === 'function' ? { ...m, type: { ...m.type, sig: { ...m.type.sig, params: m.type.sig.params.slice(1) } } } : m,
        );
        return isString ? { from, options: methods.map((m) => option(m)), validFor: /^\w*$/ } : null;
      }
      const options = membersOf(scope, resolveChain(scope, chain)).map((m) => option(m));
      return options.length ? { from, options, validFor: /^\w*$/ } : null;
    }

    const word = context.matchBefore(/[A-Za-z_]\w*/);
    if (!word && !context.explicit) return null;
    const from = word?.from ?? context.pos;
    const globals = [...scope.globals.values()].map((m) =>
      option(m, typedLocals.has(m.name) ? 3 : m.type.kind === 'function' || m.type.kind === 'table' ? 0 : 2),
    );
    const locals = localNames(context.state.doc.toString())
      .filter((n) => !scope.globals.has(n))
      .map((n): Completion => ({ label: n, type: 'variable', boost: 3 }));
    const keywords = KEYWORDS.map((k): Completion => ({ label: k, type: 'keyword', boost: -1 }));
    return { from, options: [...locals, ...globals, ...keywords], validFor: /^\w*$/ };
  };
}

/** The chain ending at `pos` (`payload.items[1].na|me` → `payload.items[1].name`). */
export function chainAt(text: string, pos: number): { chain: string; from: number; to: number } | null {
  let end = pos;
  while (end < text.length && /\w/.test(text[end])) end++;
  const head = text.slice(0, end);
  const m = /([A-Za-z_]\w*(?:\s*(?:\.\s*[A-Za-z_]\w*|\[[^\]\n]*\]|:\s*[A-Za-z_]\w*))*)$/.exec(head);
  if (!m || end === pos && !/\w/.test(text[pos - 1] ?? '')) return null;
  return { chain: m[1], from: end - m[1].length, to: end };
}

/** Hover text for the name under `pos`: its type or signature, and its docs. */
export function hoverText(base: Scope, code: string, pos: number): { from: number; to: number; text: string } | null {
  const at = chainAt(code, pos);
  if (!at) return null;
  const { scope } = withLocals(base, code.slice(0, at.from));
  const member = memberOf(scope, at.chain);
  if (!member) return null;
  const head =
    member.type.kind === 'function'
      ? signatureText(at.chain.replace(/\s+/g, ''), member.type.sig)
      : `${at.chain.replace(/\s+/g, '')}: ${typeName(member.type)}`;
  const params =
    member.type.kind === 'function'
      ? member.type.sig.params.filter((p) => p.doc).map((p) => `${p.name}: ${p.doc}`)
      : [];
  return { from: at.from, to: at.to, text: [head, member.doc, ...params].filter(Boolean).join('\n') };
}
