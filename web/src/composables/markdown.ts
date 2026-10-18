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

/** Emphasis on already escaped text (it has no `<`, so the tags are ours). */
function emphasis(s: string): string {
  return s
    .replace(/\*\*(?=\S)([\s\S]*?\S)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^\w])__(?=\S)([\s\S]*?\S)__(?!\w)/g, '$1<strong>$2</strong>')
    .replace(/~~(?=\S)([\s\S]*?\S)~~/g, '<del>$1</del>')
    .replace(/(^|[^\w*])\*(?=[^\s*])([^*]*?[^\s*])\*(?![\w*])/g, '$1<em>$2</em>')
    .replace(/(^|[^\w])_(?=[^\s_])([^_]*?[^\s_])_(?!\w)/g, '$1<em>$2</em>');
}

const LINK = /\[([^\]\n]+)\]\(\s*<?([^()\s<>]+)>?(?:\s+"[^"]*")?\s*\)/g;

/** Text without code spans: links, then emphasis. */
function textSpan(s: string): string {
  let out = '';
  let last = 0;
  for (const m of s.matchAll(LINK)) {
    out += emphasis(esc(s.slice(last, m.index)));
    const href = safeHref(m[2]);
    const label = emphasis(esc(m[1]));
    out += href ? `<a href="${esc(href)}" rel="noopener noreferrer" target="_blank">${label}</a>` : label;
    last = m.index! + m[0].length;
  }
  return out + emphasis(esc(s.slice(last)));
}

/** A line's inline Markdown, as HTML. */
export function inline(s: string): string {
  let out = '';
  let last = 0;
  for (const m of s.matchAll(/(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)/g)) {
    out += textSpan(s.slice(last, m.index));
    out += `<code>${esc(m[2].trim() ? m[2].replace(/^ (.*) $/, '$1') : m[2])}</code>`;
    last = m.index! + m[0].length;
  }
  return out + textSpan(s.slice(last));
}

// -- blocks ------------------------------------------------------------------------------

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^\s`]*)[^`]*$/;
const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const ITEM = /^( *)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;
const TABLE_SEP = /^ *\|? *:?-+:? *(\| *:?-+:? *)*\|? *$/;

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
