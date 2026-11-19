// Reads the LuaCATS subset flwrs plugins' `definitions` files use (a
// `---@meta` file, as lua-language-server reads it) into a scope:
//
//   ---@class Name            a class; `---@field name[?] type doc` lines follow
//   ---@alias Name type        a name for a type
//   ---Doc text                documents the next function (or class)
//   ---@param name[?] type doc
//   ---@return type doc
//   function name(a, b) end    a global function; `function lib.name()` a
//   lib = {}                   library table member
//
// Anything else is skipped. Type texts are kept as written; a type that's
// a class name links to the class.

import { fn, plain, table, type LuaType, type Member, type Param, type Scope } from './types';

const IDENT = /^[A-Za-z_][\w]*$/;

/** A type text as a type: a class or alias name links, else as written. */
function typeOf(text: string, classes: Set<string>): LuaType {
  const t = text.trim();
  const bare = t.replace(/\?$/, '');
  if (classes.has(bare)) return { kind: 'named', name: bare };
  const list = /^(\w+)\[\]$/.exec(bare);
  if (list && classes.has(list[1])) return { kind: 'list', element: { kind: 'named', name: list[1] } };
  return plain(t);
}

/** Splits a doc line's `type rest` where the type may hold spaces inside braces / parentheses. */
function splitType(text: string): [string, string] {
  let depth = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if ('{(<['.includes(c)) depth++;
    else if ('})>]'.includes(c)) depth--;
    else if (c === ' ' && depth === 0 && !/[|,:]/.test(text[i - 1] ?? '') && !/[|,:]/.test(text[i + 1] ?? '')) {
      return [text.slice(0, i), text.slice(i + 1).trim()];
    }
  }
  return [text, ''];
}

export function addDefinitions(scope: Scope, source: string): void {
  const lines = source.split(/\r?\n/);
  // Class names first, so fields and params can link to classes defined later.
  const classNames = new Set(scope.classes.keys());
  for (const line of lines) {
    const m = /^---@(?:class|alias)\s+([A-Za-z_][\w.]*)/.exec(line.trim());
    if (m) classNames.add(m[1]);
  }

  let doc: string[] = [];
  let params: Param[] = [];
  let returns: string[] = [];
  let currentClass: { name: string; fields: Member[]; doc?: string } | null = null;
  const closeClass = () => {
    if (currentClass) scope.classes.set(currentClass.name, table(currentClass.fields, currentClass.name));
    currentClass = null;
  };
  const reset = () => {
    doc = [];
    params = [];
    returns = [];
  };

  for (const raw of lines) {
    const line = raw.trim();
    let m: RegExpExecArray | null;
    if ((m = /^---@class\s+([A-Za-z_][\w.]*)/.exec(line))) {
      closeClass();
      currentClass = { name: m[1], fields: [], doc: doc.join('\n') || undefined };
      reset();
    } else if ((m = /^---@field\s+([A-Za-z_]\w*)(\??)\s+(.*)$/.exec(line))) {
      const [type, rest] = splitType(m[3]);
      currentClass?.fields.push({ name: m[1], type: typeOf(m[2] ? `${type}?` : type, classNames), doc: rest || undefined });
    } else if ((m = /^---@alias\s+([A-Za-z_][\w.]*)\s+(.*)$/.exec(line))) {
      closeClass();
      scope.classes.set(m[1], typeOf(m[2], classNames));
      reset();
    } else if ((m = /^---@param\s+([A-Za-z_]\w*|\.\.\.)(\??)\s+(.*)$/.exec(line))) {
      const [type, rest] = splitType(m[3]);
      params.push({ name: m[1], type, optional: m[2] === '?', doc: rest || undefined });
    } else if ((m = /^---@return\s+(.*)$/.exec(line))) {
      returns.push(splitType(m[1])[0]);
    } else if (line.startsWith('---@')) {
      // Other annotations (@meta, @type, @deprecated, …): skipped.
    } else if (line.startsWith('---')) {
      doc.push(line.slice(3).trim());
    } else if ((m = /^function\s+([A-Za-z_][\w.]*)\s*\(([^)]*)\)/.exec(line))) {
      closeClass();
      const path = m[1].split('.');
      const names = m[2].split(',').map((s) => s.trim()).filter(Boolean);
      // Parameters documented by @param keep their types; others are `any`.
      const sigParams = names.map((n) => params.find((p) => p.name === n) ?? { name: n, type: 'any' });
      const member: Member = { name: path[path.length - 1], type: fn(sigParams, returns), doc: doc.join('\n') || undefined };
      place(scope, path, member);
      reset();
    } else if ((m = /^([A-Za-z_]\w*)\s*=\s*\{\s*\}/.exec(line))) {
      closeClass();
      if (!scope.globals.has(m[1]) || scope.globals.get(m[1])!.type.kind !== 'table') {
        scope.globals.set(m[1], { name: m[1], type: table([], m[1]), doc: doc.join('\n') || undefined });
      }
      reset();
    } else if (line === '') {
      // A blank line ends a class and a doc block that led nowhere.
      closeClass();
      reset();
    } else if (line.startsWith('--')) {
      // A plain comment: nothing for editors.
    } else {
      closeClass();
      reset();
    }
  }
  closeClass();
}

/** Puts a member at a dotted path (`json.encode` into the `json` table). */
function place(scope: Scope, path: string[], member: Member) {
  if (path.length === 1) {
    scope.globals.set(member.name, member);
    return;
  }
  if (!path.every((p) => IDENT.test(p))) return;
  let owner = scope.globals.get(path[0]);
  if (!owner || owner.type.kind !== 'table') {
    owner = { name: path[0], type: table([], path[0]) };
    scope.globals.set(path[0], owner);
  }
  let fields = (owner.type as Extract<LuaType, { kind: 'table' }>).fields;
  for (const part of path.slice(1, -1)) {
    let next = fields.get(part);
    if (!next || next.type.kind !== 'table') {
      next = { name: part, type: table([], part) };
      fields.set(part, next);
    }
    fields = (next.type as Extract<LuaType, { kind: 'table' }>).fields;
  }
  fields.set(member.name, member);
}

/** `fun(payload: In, meta: Meta): Out|nil` → its parameters' names and types. */
export function signatureParams(signature: string | undefined): Map<string, string> {
  const out = new Map<string, string>();
  const m = signature ? /^fun\s*\(([^)]*)\)/.exec(signature.trim()) : null;
  for (const part of m?.[1].split(',') ?? []) {
    const [name, type] = part.split(':').map((s) => s.trim());
    if (name) out.set(name.replace(/\?$/, ''), type ?? 'any');
  }
  return out;
}
