// Text with web addresses in it (a lineage line's detail: a Databricks job's or a Power BI report's
// address), cut in pieces with each address on its own, so it shows as a link that opens in the
// system's browser. Only http(s) addresses are links; anything else stays text.

export interface TextPart {
  text: string;
  /** The address, when the piece is one. */
  url?: string;
}

const ADDRESS = /https?:\/\/[^\s<>"'`]+/gi;

/** An address without what ends the sentence around it: a final period or comma, and a closing
 *  parenthesis the address didn't open ("(https://…/jobs/1)"). */
function trimmed(url: string): string {
  let u = url.replace(/[.,;:!?]+$/, '');
  const count = (c: string) => u.split(c).length - 1;
  while (u.endsWith(')') && count(')') > count('(')) u = u.slice(0, -1).replace(/[.,;:!?]+$/, '');
  return u;
}

export function linkParts(text: string): TextPart[] {
  const out: TextPart[] = [];
  let at = 0;
  for (const m of text.matchAll(ADDRESS)) {
    const start = m.index ?? 0;
    const url = trimmed(m[0]);
    if (start > at) out.push({ text: text.slice(at, start) });
    out.push({ text: url, url });
    at = start + url.length;
  }
  if (at < text.length) out.push({ text: text.slice(at) });
  return out;
}
