// Syntax errors for the editor, from the server (`POST /api/lua/check`:
// the check saving runs, real Lua 5.4). A server without Lua says so once,
// and isn't asked again.

import type { Diagnostic } from '@codemirror/lint';
import type { Text } from '@codemirror/state';
import { api } from '../api';

let unavailable = false;

export async function checkLua(code: string, params: string[], doc: Text): Promise<Diagnostic[]> {
  if (unavailable || !code.trim()) return [];
  try {
    const result = await api<{ available: boolean; errors: { line: number | null; message: string }[] }>('/lua/check', {
      method: 'POST',
      json: { code, params },
    });
    if (!result.available) {
      unavailable = true;
      return [];
    }
    return result.errors.map((e) => {
      // `near <eof>` errors point past the end: the last line then.
      const line = doc.line(Math.min(Math.max(e.line ?? 1, 1), doc.lines));
      return { from: line.from, to: line.to, severity: 'error', message: e.message, source: 'Lua' };
    });
  } catch {
    return [];
  }
}
