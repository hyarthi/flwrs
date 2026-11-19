// What every flwrs Lua sandbox has (flwrs-types `lua.rs`): Lua 5.4's base
// functions without `load`, `loadfile`, `dofile`, `collectgarbage`,
// `require` and `print`; the `string`, `table`, `math` and `utf8`
// libraries; and flwrs's `json`. Globals can be read, not assigned.

import { fn, param as p, plain, table, type Member, type Scope } from './types';

const f = (name: string, doc: string, params: ReturnType<typeof p>[], returns: string[] = []): Member => ({
  name,
  type: fn(params, returns),
  doc,
});

const BASE: Member[] = [
  f('assert', 'Raises an error if `v` is false or nil; else returns its arguments.', [p('v'), p('message?', 'string')], ['any']),
  f('error', 'Raises an error with `message`.', [p('message'), p('level?', 'integer')]),
  f('ipairs', 'Iterates over `t[1]`, `t[2]`, … up to the first nil.', [p('t', 'table')], ['fun(): integer, any']),
  f('pairs', 'Iterates over every key and value of `t`.', [p('t', 'table')], ['fun(): any, any']),
  f('next', 'The key after `key` in `t` and its value (nil at the end).', [p('t', 'table'), p('key?')], ['any', 'any']),
  f('pcall', 'Calls `f` in protected mode: true and its results, or false and the error.', [p('f', 'function'), p('...')], ['boolean', 'any']),
  f('xpcall', 'As pcall, with a message handler.', [p('f', 'function'), p('handler', 'function'), p('...')], ['boolean', 'any']),
  f('select', '`select("#", ...)` counts the arguments; `select(n, ...)` returns them from the n-th.', [p('n', 'integer|"#"'), p('...')], ['any']),
  f('tonumber', 'A number from a string (or nil).', [p('v'), p('base?', 'integer')], ['number|nil']),
  f('tostring', 'A value as a string.', [p('v')], ['string']),
  f('type', 'The type of a value: "nil", "number", "string", "boolean", "table", "function", …', [p('v')], ['string']),
  f('rawequal', 'Equality without metamethods.', [p('a'), p('b')], ['boolean']),
  f('rawget', '`t[k]` without metamethods.', [p('t', 'table'), p('k')], ['any']),
  f('rawlen', 'The length of `v` without metamethods.', [p('v', 'table|string')], ['integer']),
  f('rawset', '`t[k] = v` without metamethods.', [p('t', 'table'), p('k'), p('v')], ['table']),
  f('getmetatable', 'The metatable of `v`.', [p('v')], ['table|nil']),
  f('setmetatable', 'Sets the metatable of `t`.', [p('t', 'table'), p('mt', 'table|nil')], ['table']),
  { name: '_VERSION', type: plain('string'), doc: 'The Lua version, "Lua 5.4".' },
];

const STRING: Member[] = [
  f('byte', 'The byte values of `s[i..j]`.', [p('s', 'string'), p('i?', 'integer'), p('j?', 'integer')], ['integer']),
  f('char', 'A string from byte values.', [p('...', 'integer')], ['string']),
  f('find', 'Finds `pattern` in `s` (from `init`): start, end and captures, or nil. `plain` turns patterns off.', [p('s', 'string'), p('pattern', 'string'), p('init?', 'integer'), p('plain?', 'boolean')], ['integer|nil', 'integer|nil']),
  f('format', 'A formatted string (`%d`, `%s`, `%q`, `%5.2f`, …).', [p('fmt', 'string'), p('...')], ['string']),
  f('gmatch', 'An iterator over the matches of `pattern` in `s`.', [p('s', 'string'), p('pattern', 'string')], ['fun(): string']),
  f('gsub', 'Replaces matches of `pattern` (a string, table or function as the replacement); also returns the count.', [p('s', 'string'), p('pattern', 'string'), p('repl', 'string|table|function'), p('n?', 'integer')], ['string', 'integer']),
  f('len', 'The length of `s` in bytes.', [p('s', 'string')], ['integer']),
  f('lower', '`s` in lower case.', [p('s', 'string')], ['string']),
  f('match', 'The captures of the first match of `pattern` (or the whole match), or nil.', [p('s', 'string'), p('pattern', 'string'), p('init?', 'integer')], ['string|nil']),
  f('rep', '`s` repeated `n` times, with `sep` between.', [p('s', 'string'), p('n', 'integer'), p('sep?', 'string')], ['string']),
  f('reverse', '`s` reversed.', [p('s', 'string')], ['string']),
  f('sub', 'The substring `s[i..j]` (negative: from the end).', [p('s', 'string'), p('i', 'integer'), p('j?', 'integer')], ['string']),
  f('upper', '`s` in upper case.', [p('s', 'string')], ['string']),
  f('pack', 'Packs values into a binary string by `fmt`.', [p('fmt', 'string'), p('...')], ['string']),
  f('unpack', 'Unpacks values from a binary string by `fmt`.', [p('fmt', 'string'), p('s', 'string'), p('pos?', 'integer')], ['any']),
  f('packsize', 'The size of a `string.pack` result for `fmt`.', [p('fmt', 'string')], ['integer']),
];

const TABLE: Member[] = [
  f('concat', 'The list elements joined with `sep`.', [p('t', 'table'), p('sep?', 'string'), p('i?', 'integer'), p('j?', 'integer')], ['string']),
  f('insert', 'Inserts `value` at `pos` (default: at the end).', [p('t', 'table'), p('pos', 'integer|any'), p('value?')]),
  f('remove', 'Removes and returns the element at `pos` (default: the last).', [p('t', 'table'), p('pos?', 'integer')], ['any']),
  f('sort', 'Sorts the list in place (with `comp(a, b)`: a before b).', [p('t', 'table'), p('comp?', 'function')]),
  f('unpack', 'The list elements `t[i..j]` as values.', [p('t', 'table'), p('i?', 'integer'), p('j?', 'integer')], ['any']),
  f('pack', 'A table of the arguments, with `n` = their count.', [p('...')], ['table']),
  f('move', 'Moves `a1[f..e]` to `a2` (default `a1`) at `t`.', [p('a1', 'table'), p('f', 'integer'), p('e', 'integer'), p('t', 'integer'), p('a2?', 'table')], ['table']),
];

const MATH: Member[] = [
  ...['abs', 'ceil', 'floor', 'sqrt', 'exp', 'sin', 'cos', 'tan', 'asin', 'acos'].map((n) => f(n, `math.${n}(x)`, [p('x', 'number')], ['number'])),
  f('atan', 'The arc tangent of y/x (in radians).', [p('y', 'number'), p('x?', 'number')], ['number']),
  f('log', 'The logarithm of `x` (natural, or in `base`).', [p('x', 'number'), p('base?', 'number')], ['number']),
  f('max', 'The largest argument.', [p('x', 'number'), p('...', 'number')], ['number']),
  f('min', 'The smallest argument.', [p('x', 'number'), p('...', 'number')], ['number']),
  f('fmod', 'The remainder of x / y, rounded towards zero.', [p('x', 'number'), p('y', 'number')], ['number']),
  f('modf', 'The integral and fractional parts of `x`.', [p('x', 'number')], ['number', 'number']),
  f('tointeger', '`x` as an integer if it has an exact one, else nil.', [p('x', 'number')], ['integer|nil']),
  f('type', '"integer", "float", or nil for a non-number.', [p('x')], ['string|nil']),
  f('ult', 'Whether `m < n` as unsigned integers.', [p('m', 'integer'), p('n', 'integer')], ['boolean']),
  f('random', 'A pseudo-random number: a float in [0, 1), or an integer in [1, m] / [m, n].', [p('m?', 'integer'), p('n?', 'integer')], ['number']),
  f('randomseed', 'Seeds the pseudo-random generator.', [p('x?', 'integer')]),
  { name: 'pi', type: plain('number'), doc: 'π' },
  { name: 'huge', type: plain('number'), doc: 'A value larger than any number (infinity).' },
  { name: 'maxinteger', type: plain('integer'), doc: 'The largest integer.' },
  { name: 'mininteger', type: plain('integer'), doc: 'The smallest integer.' },
];

const UTF8: Member[] = [
  f('char', 'A UTF-8 string from code points.', [p('...', 'integer')], ['string']),
  f('codes', 'An iterator over the code points of `s` (position, code point).', [p('s', 'string')], ['fun(): integer, integer']),
  f('codepoint', 'The code points of `s[i..j]`.', [p('s', 'string'), p('i?', 'integer'), p('j?', 'integer')], ['integer']),
  f('len', 'The number of UTF-8 characters in `s[i..j]` (nil and a position if invalid).', [p('s', 'string'), p('i?', 'integer'), p('j?', 'integer')], ['integer|nil']),
  f('offset', 'The byte position of the n-th character.', [p('s', 'string'), p('n', 'integer'), p('i?', 'integer')], ['integer|nil']),
  { name: 'charpattern', type: plain('string'), doc: 'The pattern matching one UTF-8 character.' },
];

const JSON: Member[] = [
  f('encode', 'A value as JSON text (integers exact, NaN and infinities refused).', [p('value')], ['string']),
  f('decode', 'JSON text as a value (tables are plain and writable).', [p('text', 'string')], ['any']),
];

/** The event metadata every code field's `meta` has. */
export const META = table(
  [
    { name: 'event_id', type: plain('string'), doc: 'This handover’s id.' },
    { name: 'trace_id', type: plain('string'), doc: 'The trace the event belongs to (kept through transforms).' },
    { name: 'hops', type: plain('integer'), doc: 'Handovers so far.' },
    { name: 'origin_node_id', type: plain('string'), doc: 'The node that started the trace.' },
    { name: 'create_time', type: plain('integer'), doc: 'When the trace started, epoch milliseconds (UTC).' },
  ],
  'Meta',
);

export const KEYWORDS = [
  'and', 'break', 'do', 'else', 'elseif', 'end', 'false', 'for', 'function', 'goto', 'if', 'in',
  'local', 'nil', 'not', 'or', 'repeat', 'return', 'then', 'true', 'until', 'while',
];

/** A fresh scope with the sandbox's globals. */
export function sandboxScope(): Scope {
  const globals = new Map<string, Member>();
  for (const m of BASE) globals.set(m.name, m);
  const lib = (name: string, members: Member[], doc: string) => globals.set(name, { name, type: table(members, name), doc });
  lib('string', STRING, 'String functions; also as methods: `s:upper()`.');
  lib('table', TABLE, 'Table (list) functions.');
  lib('math', MATH, 'Mathematical functions.');
  lib('utf8', UTF8, 'UTF-8 functions.');
  lib('json', JSON, 'JSON (flwrs): bytes as base64, datetimes as RFC 3339, integers exact.');
  const classes = new Map([['Meta', META]]);
  return { globals, classes };
}

/** `string`'s functions, for `s:method()` on strings. */
export const STRING_METHODS = STRING;
