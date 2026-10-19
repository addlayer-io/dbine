// A small Markdown renderer for the assistant's answers and the notebooks'
// text cells. Everything is escaped first: no raw HTML ever gets through,
// only the tags this file writes. Links are kept only for http(s) and
// mailto addresses (any other scheme, `javascript:` included, shows as
// plain text). Pure functions, no Vue: node:test runs it
// (markdown.test.mjs).
//
// Supported: headings (# … ######), paragraphs, **bold** / __bold__,
// *italic* / _italic_, ~~strikethrough~~, `inline code`, fenced code
// (``` or ~~~), ordered and unordered lists (nested by indentation),
// blockquotes, horizontal rules, [links](https://…) and simple pipe tables.

export interface MarkdownOptions {
  /** A single line break inside a paragraph is a <br> (chat answers);
   *  otherwise a space, as in Jupyter and GitHub. */
  breaks?: boolean;
  /** Headings as `<p class="md-h">` (a chat answer has no document outline). */
  flatHeadings?: boolean;
}

/** Text safe in HTML, attributes included. */
export function esc(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

/** Only these links are kept. */
export function safeHref(url: string): string | null {
  const u = url.trim();
  // No control characters or spaces hidden in the scheme ("java\tscript:").
  if (/[\u0000-\u001f\u007f\s]/.test(u)) return null;
  return /^(https?:\/\/|mailto:)/i.test(u) ? u : null;
}

/** `esc` undone (an attribute's value back to its text). */
function unesc(s: string): string {
  return s.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&');
}

/** Where a link goes, as the assistant's answers show it beside the
 *  label: the host (the address, for mailto:). */
export function linkDestination(href: string): string {
  if (/^mailto:/i.test(href)) return href.slice(7).split('?')[0];
  try {
    return new URL(href).host || href;
  } catch {
    return href;
  }
}

/** Rendered Markdown with each link's destination in sight: the host after
 *  the label and the whole address as its tooltip, so a label can't hide
 *  where it goes (the assistant's answers, which can carry text from
 *  anywhere). */
export function withDestinations(html: string): string {
  return html.replace(/<a href="([^"]*)" rel="noopener noreferrer" target="_blank">([\s\S]*?)<\/a>/g, (_, href: string, label: string) =>
    `<a href="${href}" title="${href}" rel="noopener noreferrer" target="_blank">${label}</a> <span class="md-link-host">(${esc(linkDestination(unesc(href)))})</span>`);
}

/** `s` with each `open…close` pair wrapped in `tag`, as the lazy regex
 *  `open(?=\S)([\s\S]*?\S)close` would (with `left`/`right` for the
 *  characters allowed before the opener and after the closer), in linear
 *  time: the regex retries every later opener after a failed one, which is
 *  quadratic on a line such as ` **a **a …`. A closer only depends on its own
 *  position, so once an opener finds none, no later opener can either. */
function pairs(s: string, mark: string, tag: string, left?: RegExp, right?: RegExp): string {
  const n = mark.length;
  const nonSpace = (c: string | undefined) => c !== undefined && /\S/.test(c);
  let out = '';
  let last = 0; // the end of the previous match: where the next one may begin
  let i = s.indexOf(mark);
  while (i !== -1) {
    // `(^|[^\w])`: the character before is part of the match, so it must
    // come after the previous one.
    const leftOk = !left || (i === 0 && last === 0) || (i > last && left.test(s[i - 1]));
    if (leftOk && nonSpace(s[i + n])) {
      let j = s.indexOf(mark, i + n + 1);
      while (j !== -1 && !(nonSpace(s[j - 1]) && (!right || j + n >= s.length || right.test(s[j + n])))) j = s.indexOf(mark, j + 1);
      if (j === -1) break;
      out += s.slice(last, i) + `<${tag}>` + s.slice(i + n, j) + `</${tag}>`;
      last = j + n;
      i = s.indexOf(mark, last);
      continue;
    }
    i = s.indexOf(mark, i + 1);
  }
  return out + s.slice(last);
}

/** Emphasis on already escaped text (it has no `<`, so the tags are ours). */
function emphasis(s: string): string {
  s = pairs(s, '**', 'strong');
  s = pairs(s, '__', 'strong', /[^\w]/, /[^\w]/);
  s = pairs(s, '~~', 'del');
  // A single `*` or `_` can't contain another, so these scan each gap once.
  return s
    .replace(/(^|[^\w*])\*(?=[^\s*])([^*]*?[^\s*])\*(?![\w*])/g, '$1<em>$2</em>')
    .replace(/(^|[^\w])_(?=[^\s_])([^_]*?[^\s_])_(?!\w)/g, '$1<em>$2</em>');
}

interface Link { index: number; end: number; label: string; url: string }

/** `[label](url "title")` links, left to right, as the regex
 *  `\[([^\]\n]+)\]\(\s*<?([^()\s<>]+)>?(?:\s+"[^"]*")?\s*\)` finds them, in
 *  linear time (the regex rescans to the `]` from every `[`). */
function links(s: string): Link[] {
  const out: Link[] = [];
  const ws = (c: string | undefined) => c !== undefined && /\s/.test(c);
  const urlChar = (c: string | undefined) => c !== undefined && !/[()\s<>]/.test(c);
  const skipWs = (p: number) => { while (ws(s[p])) p++; return p; };
  // The `]` or line break that ends the labels starting before it.
  let term = -1;
  const tails = new Map<number, { end: number; url: string } | null>();
  // After `](` at `k`: the rest of the link, or null.
  const tail = (k: number) => {
    if (tails.has(k)) return tails.get(k)!;
    let p = skipWs(k + 2);
    if (s[p] === '<') p++;
    const u = p;
    while (urlChar(s[p])) p++;
    let r: { end: number; url: string } | null = null;
    if (p > u) {
      const url = s.slice(u, p);
      if (s[p] === '>') p++;
      // With a title, else without.
      const t = skipWs(p);
      if (t > p && s[t] === '"') {
        const q = s.indexOf('"', t + 1);
        if (q !== -1) {
          const e = skipWs(q + 1);
          if (s[e] === ')') r = { end: e + 1, url };
        }
      }
      if (!r) {
        const e = skipWs(p);
        if (s[e] === ')') r = { end: e + 1, url };
      }
    }
    tails.set(k, r);
    return r;
  };
  let i = s.indexOf('[');
  while (i !== -1) {
    if (term <= i) {
      term = i + 1;
      while (term < s.length && s[term] !== ']' && s[term] !== '\n') term++;
    }
    const r = term > i + 1 && s[term] === ']' && s[term + 1] === '(' ? tail(term) : null;
    if (r) {
      out.push({ index: i, end: r.end, label: s.slice(i + 1, term), url: r.url });
      i = s.indexOf('[', r.end);
    } else {
      i = s.indexOf('[', i + 1);
    }
  }
  return out;
}

/** Text without code spans: links, then emphasis. */
function textSpan(s: string): string {
  let out = '';
  let last = 0;
  for (const m of links(s)) {
    out += emphasis(esc(s.slice(last, m.index)));
    const href = safeHref(m.url);
    const label = emphasis(esc(m.label));
    out += href ? `<a href="${esc(href)}" rel="noopener noreferrer" target="_blank">${label}</a>` : label;
    last = m.end;
  }
  return out + emphasis(esc(s.slice(last)));
}

/** Longer text is shown escaped, without inline Markdown. */
export const INLINE_MAX = 100_000;

/** A line's inline Markdown, as HTML. */
export function inline(s: string): string {
  if (s.length > INLINE_MAX) return esc(s);
  // Code spans, as CommonMark: a run of backticks opens one, closed by the
  // next run of the same length; one that never closes is literal text.
  // Each run is looked at once (a regex rescans from every backtick).
  const runs: [number, number][] = [];
  for (let k = s.indexOf('`'); k !== -1; ) {
    let e = k;
    while (s[e] === '`') e++;
    runs.push([k, e - k]);
    k = s.indexOf('`', e);
  }
  // For each length, the runs of that length in order, and how far along
  // the search is.
  const byLen = new Map<number, number[]>();
  runs.forEach(([, len], r) => { let l = byLen.get(len); if (!l) byLen.set(len, (l = [])); l.push(r); });
  const next = new Map<number, number>();
  let out = '';
  let last = 0;
  for (let r = 0; r < runs.length; r++) {
    const [start, len] = runs[r];
    if (start < last) continue; // inside the previous span
    const list = byLen.get(len)!;
    let p = next.get(len) ?? 0;
    while (p < list.length && list[p] <= r) p++;
    next.set(len, p);
    if (p === list.length) continue;
    const close = runs[list[p]][0];
    const code = s.slice(start + len, close);
    out += textSpan(s.slice(last, start));
    out += `<code>${esc(code.trim() ? code.replace(/^ (.*) $/, '$1') : code)}</code>`;
    last = close + len;
  }
  return out + textSpan(s.slice(last));
}

// -- blocks ------------------------------------------------------------------------------

// Every block matcher is linear in the line. Fences and headings are found
// by hand: a regex with a variable run followed by a lookahead or optional
// groups ending in `$` retries every split of the run (quadratic, cubic for a
// heading whose `.*?` can't reach the end). The regexes left have no such
// split: `(?![ \t])` stops ITEM's spaces from being handed back to `(.*)`.
const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const ITEM = /^( *)([-*+]|\d{1,9}[.)])[ \t]+(?![ \t])(.*)$/;
const TABLE_SEP = /^ *(?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?$/;

/** Longer lines are shown escaped, as text of their own: no block or
 *  inline Markdown. */
export const LINE_MAX = INLINE_MAX;
const isLong = (l: string) => l.length > LINE_MAX;
const isBlank = (c: string | undefined) => c === ' ' || c === '\t';

/** The indentation before a block's marker: up to three spaces, else -1. */
function lead(l: string): number {
  let k = 0;
  while (l[k] === ' ') k++;
  return k <= 3 ? k : -1;
}

/** Where the run of `c` that starts at `k` ends. */
function runEnd(l: string, k: number, c: string): number {
  while (l[k] === c) k++;
  return k;
}

interface Fence { mark: string; info: string }

/** A fence's opening line: three or more ` or ~ (and no backtick after
 *  them), then the info string's first word. */
function fenceOf(l: string): Fence | null {
  if (isLong(l)) return null;
  const k = lead(l);
  if (k < 0 || (l[k] !== '`' && l[k] !== '~')) return null;
  const e = runEnd(l, k, l[k]);
  if (e - k < 3 || l.indexOf('`', e) !== -1) return null;
  let p = e;
  while (p < l.length && /\s/.test(l[p])) p++;
  let q = p;
  while (q < l.length && !/[\s`]/.test(l[q])) q++;
  return { mark: l.slice(k, e), info: l.slice(p, q) };
}

/** The line closes the fence `mark` opened: as long a run of its character,
 *  then only whitespace. */
function closesFence(l: string, mark: string): boolean {
  const k = lead(l);
  if (k < 0) return false;
  const e = runEnd(l, k, mark[0]);
  return e - k >= mark.length && !l.slice(e).trim();
}

/** An ATX heading: `#` to `######`, then its text without the spaces around
 *  it or a closing run of `#` after a space. */
function headingOf(l: string): { level: number; text: string } | null {
  if (isLong(l)) return null;
  const k = lead(l);
  if (k < 0 || l[k] !== '#') return null;
  const e = runEnd(l, k, '#');
  if (e - k > 6 || (e < l.length && !isBlank(l[e]))) return null;
  // The regex this replaces read the text with `.`, which stops at these:
  // such a line was a paragraph.
  if (/[\u2028\u2029]/.test(l.slice(e))) return null;
  let s = e;
  while (isBlank(l[s])) s++;
  let t = l.length;
  while (t > s && isBlank(l[t - 1])) t--;
  let h = t;
  while (h > s && l[h - 1] === '#') h--;
  if (h < t && h > s && isBlank(l[h - 1])) {
    while (isBlank(l[h - 1])) h--;
    t = h;
  }
  return { level: e - k, text: l.slice(s, t) };
}

function cellsOf(line: string): string[] {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  // `\|` is a pipe inside a cell.
  return s.split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, '|'));
}

/** A table starts here: a header row with pipes and the `---|---` row under it. */
function isTableStart(lines: string[], i: number): boolean {
  return i + 1 < lines.length && lines[i].includes('|') && TABLE_SEP.test(lines[i + 1]) && lines[i + 1].includes('-');
}

function startsBlock(lines: string[], i: number): boolean {
  const l = lines[i];
  return isLong(l) || !!fenceOf(l) || HR.test(l) || !!headingOf(l) || QUOTE.test(l) || ITEM.test(l) || isTableStart(lines, i);
}

/** A line of a list item that starts a block inside it. */
const nestedStart = (l: string) => ITEM.test(l) || !!fenceOf(l) || QUOTE.test(l);

const indentOf = (l: string) => l.length - l.trimStart().length;

function renderLines(lines: string[], o: MarkdownOptions): string {
  const out: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }

    if (isLong(line)) { out.push(`<p>${esc(line.trim())}</p>`); i++; continue; }

    const fence = fenceOf(line);
    if (fence) {
      const { mark } = fence;
      const lang = /^[\w+#.-]+$/.test(fence.info) ? fence.info : '';
      const body: string[] = [];
      i++;
      while (i < lines.length && !closesFence(lines[i], mark)) body.push(lines[i++]);
      i++; // the closing fence (or the end)
      out.push(`<pre><code${lang ? ` class="language-${esc(lang)}"` : ''}>${esc(body.join('\n'))}</code></pre>`);
      continue;
    }

    if (HR.test(line)) { out.push('<hr>'); i++; continue; }

    const h = headingOf(line);
    if (h) {
      const { level } = h;
      const text = inline(h.text);
      out.push(o.flatHeadings ? `<p class="md-h">${text}</p>` : `<h${level}>${text}</h${level}>`);
      i++;
      continue;
    }

    if (QUOTE.test(line)) {
      const body: string[] = [];
      while (i < lines.length && lines[i].trim()) {
        const q = QUOTE.exec(lines[i]);
        // A line without `>` right after a quoted one continues its paragraph.
        if (!q && startsBlock(lines, i)) break;
        body.push(q ? q[1] : lines[i]);
        i++;
      }
      out.push(`<blockquote>${renderLines(body, o)}</blockquote>`);
      continue;
    }

    if (isTableStart(lines, i)) {
      const head = cellsOf(line);
      const align = cellsOf(lines[i + 1]).map((c) => (c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : c.startsWith(':') ? 'left' : ''));
      const attr = (k: number) => (align[k] ? ` style="text-align:${align[k]}"` : '');
      const rows: string[][] = [];
      i += 2;
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) rows.push(cellsOf(lines[i++]));
      const th = head.map((c, k) => `<th${attr(k)}>${inline(c)}</th>`).join('');
      const tb = rows.map((r) => `<tr>${head.map((_, k) => `<td${attr(k)}>${inline(r[k] ?? '')}</td>`).join('')}</tr>`).join('');
      out.push(`<table><thead><tr>${th}</tr></thead>${tb ? `<tbody>${tb}</tbody>` : ''}</table>`);
      continue;
    }

    const item = ITEM.exec(line);
    if (item) {
      const base = item[1].length;
      const ordered = /\d/.test(item[2]);
      const items: string[][] = [];
      while (i < lines.length) {
        const l = lines[i];
        const m = ITEM.exec(l);
        if (m && m[1].length <= base + 1 && /\d/.test(m[2]) === ordered) {
          items.push([m[3]]);
          i++;
          continue;
        }
        if (m && m[1].length <= base + 1) break; // a list of the other kind
        if (!l.trim()) {
          // A blank line ends the list unless the next line is still in it.
          const next = lines[i + 1];
          if (next !== undefined && next.trim() && (indentOf(next) > base + 1 || (ITEM.exec(next)?.[1].length ?? -1) === base)) { items[items.length - 1].push(''); i++; continue; }
          break;
        }
        if (indentOf(l) > base) { items[items.length - 1].push(l.slice(Math.min(indentOf(l), base + 2))); i++; continue; }
        // A lazy continuation of the item's text.
        if (startsBlock(lines, i)) break;
        items[items.length - 1].push(l);
        i++;
      }
      const start = ordered ? parseInt(item[2], 10) : 1;
      const tag = ordered ? 'ol' : 'ul';
      const lis = items.map((body) => {
        const nested = body.slice(1).some((l) => nestedStart(l) || !l.trim());
        if (!nested) return `<li>${inline(body.map((l) => l.trim()).join(o.breaks ? '\n' : ' ')).replace(/\n/g, '<br>')}</li>`;
        // The first line is its text; the rest, blocks of their own.
        const firstEnd = body.findIndex((l, k) => k > 0 && (!l.trim() || nestedStart(l)));
        const text = body.slice(0, firstEnd).map((l) => l.trim()).join(' ');
        return `<li>${inline(text)}${renderLines(body.slice(firstEnd), o)}</li>`;
      }).join('');
      out.push(`<${tag}${ordered && start !== 1 ? ` start="${start}"` : ''}>${lis}</${tag}>`);
      continue;
    }

    // A paragraph: up to a blank line or the start of another block.
    const para: string[] = [line.trim()];
    i++;
    while (i < lines.length && lines[i].trim() && !startsBlock(lines, i)) para.push(lines[i++].trim());
    out.push(`<p>${para.map(inline).join(o.breaks ? '<br>' : ' ')}</p>`);
  }
  return out.join('');
}

/** Markdown as HTML (escaped first; see the top of the file). */
export function renderMarkdown(md: string, o: MarkdownOptions = {}): string {
  return renderLines(md.replace(/\r\n?/g, '\n').split('\n'), o);
}

// -- the assistant's answers: text and code blocks apart ---------------------------------

/** A chat answer's pieces: rendered text, and code blocks (with their own
 *  actions). `open`: the block is still streaming (no closing fence yet). */
export type MdPart = { kind: 'text'; html: string } | { kind: 'code'; lang: string; code: string; open: boolean };

export function markdownParts(md: string, o: MarkdownOptions = { breaks: true, flatHeadings: true }): MdPart[] {
  const out: MdPart[] = [];
  // A block: ``` and a language, the rest of that line, then up to the next
  // ``` (or the end, still open). By hand: the regex
  // /```([\w+#.-]*)[^\n]*\n([\s\S]*?)(```|$)/g split the language from the
  // rest of the line every way, and rescanned the line from every backtick.
  let last = 0;
  for (let i = md.indexOf('```'); i !== -1; ) {
    const nl = md.indexOf('\n', i + 3);
    // No line break after this opener: none after a later one either.
    if (nl === -1) break;
    let l = i + 3;
    while (l < nl && /[\w+#.-]/.test(md[l])) l++;
    const close = md.indexOf('```', nl + 1);
    if (i > last) out.push({ kind: 'text', html: renderMarkdown(md.slice(last, i), o) });
    const end = close === -1 ? md.length : close;
    out.push({ kind: 'code', lang: md.slice(i + 3, l), code: md.slice(nl + 1, end).replace(/\n$/, ''), open: close === -1 });
    if (close === -1) { last = md.length; break; }
    last = close + 3;
    i = md.indexOf('```', last);
  }
  if (last < md.length) out.push({ kind: 'text', html: renderMarkdown(md.slice(last), o) });
  return out.filter((p) => p.kind === 'code' || p.html);
}
