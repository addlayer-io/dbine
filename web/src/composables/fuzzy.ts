// Fuzzy matching for the quick pick (VS Code's): the query's letters in
// order, not necessarily together, ignoring case and accents. A match
// scores more when its letters are together and start words; `indices`
// are the label's characters to highlight. A query with spaces is several
// words, each matched on its own (in any order).

export interface FuzzyMatch {
  score: number;
  indices: number[];
}

/** A character without its accent, in lower case. */
function fold(c: string): string {
  return c.normalize('NFD')[0].toLowerCase();
}

function isWordStart(text: string, i: number): boolean {
  if (i === 0) return true;
  const prev = text[i - 1];
  if (/[\s.:_\-/›·(]/.test(prev)) return true;
  // camelCase and digits after letters.
  return /[a-z]/.test(prev) && /[A-Z]/.test(text[i]);
}

/** The best match of one word (no spaces) in `text`, or null. */
function matchWord(word: string, text: string, folded: string[]): FuzzyMatch | null {
  const q = [...word].map(fold);
  let best: FuzzyMatch | null = null;
  // Each place the first letter appears is a possible start; the rest is
  // matched greedily from there, preferring word starts.
  for (let start = folded.indexOf(q[0]); start !== -1; start = folded.indexOf(q[0], start + 1)) {
    const indices = [start];
    let pos = start;
    let ok = true;
    for (let k = 1; k < q.length; k++) {
      let next = -1;
      // A word start ahead beats the nearest letter, unless the nearest is right after.
      for (let i = pos + 1; i < folded.length; i++) {
        if (folded[i] !== q[k]) continue;
        if (i === pos + 1) { next = i; break; }
        if (next === -1) next = i;
        if (isWordStart(text, i)) { next = i; break; }
      }
      if (next === -1) { ok = false; break; }
      indices.push(next);
      pos = next;
    }
    if (!ok) break;
    let score = 0;
    for (let k = 0; k < indices.length; k++) {
      const i = indices[k];
      score += 1;
      if (isWordStart(text, i)) score += 8;
      if (k > 0 && i === indices[k - 1] + 1) score += 5;
      else if (k > 0) score -= Math.min(3, i - indices[k - 1] - 1);
    }
    if (start === 0) score += 6;
    if (!best || score > best.score) best = { score, indices };
  }
  return best;
}

/** The match of `query` in `text` (null when some word isn't there). */
export function fuzzyMatch(query: string, text: string): FuzzyMatch | null {
  const words = query.trim().split(/\s+/).filter(Boolean);
  if (!words.length) return { score: 0, indices: [] };
  const folded = [...text].map(fold);
  let score = 0;
  const all = new Set<number>();
  for (const w of words) {
    const m = matchWord(w, text, folded);
    // Letters scattered all over (no word starts, nothing together) aren't a match.
    if (!m || m.score < w.length) return null;
    score += m.score;
    for (const i of m.indices) all.add(i);
  }
  // Shorter texts first among equal matches.
  return { score: score - text.length / 100, indices: [...all].sort((a, b) => a - b) };
}
