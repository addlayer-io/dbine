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

// The lookahead keeps these linear: `\s*…[^`]*$` retried every split of a
// long run of spaces.
const FENCE = /^ {0,3}(`{3,}|~{3,})(?=[^`]*$)\s*([^\s`]*)/;
const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const ITEM = /^( *)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;
const TABLE_SEP = /^ *(?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?$/;

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
  return FENCE.test(l) || HR.test(l) || HEADING.test(l) || QUOTE.test(l) || ITEM.test(l) || isTableStart(lines, i);
}

const indentOf = (l: string) => l.length - l.trimStart().length;

function renderLines(lines: string[], o: MarkdownOptions): string {
  const out: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }

    const fence = FENCE.exec(line);
    if (fence) {
      const mark = fence[1];
      const lang = /^[\w+#.-]+$/.test(fence[2]) ? fence[2] : '';
      const body: string[] = [];
      i++;
      while (i < lines.length && !new RegExp(`^ {0,3}${mark[0] === '`' ? '`' : '~'}{${mark.length},}\\s*$`).test(lines[i])) body.push(lines[i++]);
      i++; // the closing fence (or the end)
      out.push(`<pre><code${lang ? ` class="language-${esc(lang)}"` : ''}>${esc(body.join('\n'))}</code></pre>`);
      continue;
    }

    if (HR.test(line)) { out.push('<hr>'); i++; continue; }

    const h = HEADING.exec(line);
    if (h) {
      const level = h[1].length;
      const text = inline(h[2] ?? '');
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
        const nested = body.slice(1).some((l) => ITEM.test(l) || FENCE.test(l) || QUOTE.test(l) || !l.trim());
        if (!nested) return `<li>${inline(body.map((l) => l.trim()).join(o.breaks ? '\n' : ' ')).replace(/\n/g, '<br>')}</li>`;
        // The first line is its text; the rest, blocks of their own.
        const firstEnd = body.findIndex((l, k) => k > 0 && (!l.trim() || ITEM.test(l) || FENCE.test(l) || QUOTE.test(l)));
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
  const re = /```([\w+#.-]*)[^\n]*\n([\s\S]*?)(```|$)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(md))) {
    if (m.index > last) out.push({ kind: 'text', html: renderMarkdown(md.slice(last, m.index), o) });
    out.push({ kind: 'code', lang: m[1] || '', code: m[2].replace(/\n$/, ''), open: m[3] !== '```' });
    last = re.lastIndex;
    if (m[3] !== '```') break;
  }
  if (last < md.length) out.push({ kind: 'text', html: renderMarkdown(md.slice(last), o) });
  return out.filter((p) => p.kind === 'code' || p.html);
}
