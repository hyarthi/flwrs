// A line diff for reviewing a text change: the longest common subsequence,
// shown as hunks with a little context.

export type DiffLine = { kind: 'same' | 'add' | 'del'; text: string; old?: number; new?: number };
export type Hunk = DiffLine[];

/** The lines of `a` → `b` as kept / removed / added. */
export function diffLines(a: string, b: string): DiffLine[] {
  const x = a.split('\n');
  const y = b.split('\n');
  // Trim the common head and tail (most edits are small).
  let head = 0;
  while (head < x.length && head < y.length && x[head] === y[head]) head++;
  let tail = 0;
  while (tail < x.length - head && tail < y.length - head && x[x.length - 1 - tail] === y[y.length - 1 - tail]) tail++;
  const xs = x.slice(head, x.length - tail);
  const ys = y.slice(head, y.length - tail);
  const n = xs.length;
  const m = ys.length;
  const lcs: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = xs[i] === ys[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const out: DiffLine[] = x.slice(0, head).map((text, i) => ({ kind: 'same', text, old: i + 1, new: i + 1 }));
  let i = 0;
  let j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && xs[i] === ys[j]) {
      out.push({ kind: 'same', text: xs[i], old: head + i + 1, new: head + j + 1 });
      i++;
      j++;
    } else if (i < n && (j === m || lcs[i + 1][j] >= lcs[i][j + 1])) {
      // Removals before additions, as diffs usually show them.
      out.push({ kind: 'del', text: xs[i], old: head + i + 1 });
      i++;
    } else {
      out.push({ kind: 'add', text: ys[j], new: head + j + 1 });
      j++;
    }
  }
  for (let k = 0; k < tail; k++) {
    out.push({ kind: 'same', text: x[x.length - tail + k], old: x.length - tail + k + 1, new: y.length - tail + k + 1 });
  }
  return out;
}

/** The changed lines with `context` unchanged lines around them, as hunks. */
export function hunks(lines: DiffLine[], context = 2): Hunk[] {
  const keep = new Array<boolean>(lines.length).fill(false);
  lines.forEach((l, i) => {
    if (l.kind === 'same') return;
    for (let k = Math.max(0, i - context); k <= Math.min(lines.length - 1, i + context); k++) keep[k] = true;
  });
  const out: Hunk[] = [];
  let current: Hunk | null = null;
  lines.forEach((l, i) => {
    if (!keep[i]) {
      current = null;
      return;
    }
    if (!current) {
      current = [];
      out.push(current);
    }
    current.push(l);
  });
  return out;
}
