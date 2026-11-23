// A small text diff for the editor's proposals (editorProposal.ts): the
// changes that turn `a` into `b`, as ranges of `a`, found on words,
// spaces and punctuation so a fixed column name is one change, not a line.
// Pure and dependency-free (tested in textDiff.test.mjs).

/** Replace `a[from, to)` with `insert`. */
export interface Chunk { from: number; to: number; insert: string }

interface Token { text: string; at: number }

function tokens(s: string): Token[] {
  const out: Token[] = [];
  for (const m of s.matchAll(/\s+|\w+|[^\w\s]/gu)) out.push({ text: m[0], at: m.index ?? 0 });
  return out;
}

/** Past this many table cells the middle isn't compared token by token:
 *  it becomes one change. */
const MAX_CELLS = 2_500_000;

export function diffChunks(a: string, b: string): Chunk[] {
  if (a === b) return [];
  const x = tokens(a);
  const y = tokens(b);
  let lo = 0;
  while (lo < x.length && lo < y.length && x[lo].text === y[lo].text) lo++;
  let xe = x.length;
  let ye = y.length;
  while (xe > lo && ye > lo && x[xe - 1].text === y[ye - 1].text) { xe--; ye--; }
  const aStart = lo < x.length ? x[lo].at : a.length;
  const bStart = lo < y.length ? y[lo].at : b.length;
  const aEnd = xe < x.length ? x[xe].at : a.length;
  const bEnd = ye < y.length ? y[ye].at : b.length;
  const n = xe - lo;
  const m = ye - lo;
  if (n * m > MAX_CELLS) return [{ from: aStart, to: aEnd, insert: b.slice(bStart, bEnd) }];
  const w = m + 1;
  const lcs = new Uint32Array((n + 1) * w);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i * w + j] = x[lo + i].text === y[lo + j].text
        ? lcs[(i + 1) * w + j + 1] + 1
        : Math.max(lcs[(i + 1) * w + j], lcs[i * w + j + 1]);
    }
  }
  // Runs of changes between equal tokens, as ranges of tokens.
  const runs: { xi: number; xj: number; yi: number; yj: number }[] = [];
  let i = 0;
  let j = 0;
  let open: { xi: number; yi: number } | null = null;
  const close = () => { if (open) { runs.push({ xi: open.xi, xj: i, yi: open.yi, yj: j }); open = null; } };
  while (i < n || j < m) {
    if (i < n && j < m && x[lo + i].text === y[lo + j].text) { close(); i++; j++; continue; }
    open ??= { xi: i, yi: j };
    if (j < m && (i >= n || lcs[i * w + j + 1] >= lcs[(i + 1) * w + j])) j++;
    else i++;
  }
  close();
  // Two changes with only spaces between them read as one.
  const merged: typeof runs = [];
  for (const r of runs) {
    const last = merged[merged.length - 1];
    if (last && r.xi - last.xj === 1 && x[lo + last.xj].text.trim() === '' && y[lo + last.yj].text === x[lo + last.xj].text) {
      last.xj = r.xj;
      last.yj = r.yj;
    } else merged.push({ ...r });
  }
  const xAt = (k: number) => (lo + k < x.length ? x[lo + k].at : a.length);
  const yAt = (k: number) => (lo + k < y.length ? y[lo + k].at : b.length);
  return merged.map((r) => ({ from: xAt(r.xi), to: xAt(r.xj), insert: b.slice(yAt(r.yi), yAt(r.yj)) }));
}

/** `a` with every chunk (or only the `only` ones, by index) applied. */
export function applyChunks(a: string, chunks: Chunk[], only?: Set<number>): string {
  let out = '';
  let at = 0;
  chunks.forEach((c, k) => {
    if (only && !only.has(k)) return;
    out += a.slice(at, c.from) + c.insert;
    at = c.to;
  });
  return out + a.slice(at);
}
