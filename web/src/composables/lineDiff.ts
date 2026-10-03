// Side-by-side line diff (longest common subsequence), for comparing the
// source of views, procedures and functions. Lines compare with their
// spacing collapsed, like the backend's comparison; `exact` compares them
// as they are (a project file's git diff, where spacing is a change too).

export interface DiffLine {
  left: string | null;
  right: string | null;
  /** `same`, only on one side (`left` / `right`), or both but different (`changed`). */
  kind: 'same' | 'left' | 'right' | 'changed';
}

const norm = (s: string) => s.trim().replace(/\s+/g, ' ');

export function lineDiff(a: string, b: string, opts: { exact?: boolean } = {}): DiffLine[] {
  const x = a.replace(/\r\n/g, '\n').split('\n');
  const y = b.replace(/\r\n/g, '\n').split('\n');
  const nx = opts.exact ? x : x.map(norm);
  const ny = opts.exact ? y : y.map(norm);
  const n = x.length;
  const m = y.length;
  // Too big for the table: line by line.
  if (n * m > 4_000_000) {
    return Array.from({ length: Math.max(n, m) }, (_, i) => {
      const l = x[i] ?? null;
      const r = y[i] ?? null;
      return { left: l, right: r, kind: l === null ? 'right' : r === null ? 'left' : nx[i] === ny[i] ? 'same' : 'changed' };
    });
  }
  const lcs: Uint32Array[] = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = nx[i] === ny[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && nx[i] === ny[j]) {
      out.push({ left: x[i++], right: y[j++], kind: 'same' });
    } else if (j < m && (i >= n || (opts.exact ? lcs[i][j + 1] > lcs[i + 1][j] : lcs[i][j + 1] >= lcs[i + 1][j]))) {
      // (exact: on a tie the removal goes first, so a replaced line pairs up below)
      out.push({ left: null, right: y[j++], kind: 'right' });
    } else {
      out.push({ left: x[i++], right: null, kind: 'left' });
    }
  }
  // A removal followed by an addition reads better as one changed line.
  const merged: DiffLine[] = [];
  for (let k = 0; k < out.length; k++) {
    const d = out[k];
    const next = out[k + 1];
    if (d.kind === 'left' && next?.kind === 'right') {
      merged.push({ left: d.left, right: next.right, kind: 'changed' });
      k++;
    } else {
      merged.push(d);
    }
  }
  return merged;
}
