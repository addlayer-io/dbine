// Release notes for the update dialog (UpdateNotice.vue). GitHub writes them
// in Markdown; the dialog shows headings, bullets and paragraphs as plain
// text. Never as HTML: the notes come from the network.

export interface NoteBlock {
  kind: 'heading' | 'item' | 'para';
  text: string;
  /** Nested bullet. */
  nested?: boolean;
}

/** Inline marks out: links keep their text, emphasis and code their content. */
function inline(s: string): string {
  return s
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
    .replace(/<[^>]+>/g, '')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/(\*\*|__)(.+?)\1/g, '$2')
    .replace(/(^|[^\w*])\*([^*\s][^*]*?)\*(?![\w*])/g, '$1$2')
    .replace(/(^|[^\w_])_([^_\s][^_]*?)_(?![\w_])/g, '$1$2')
    .trim();
}

export function noteBlocks(md: string): NoteBlock[] {
  const blocks: NoteBlock[] = [];
  let para: string[] = [];
  const flush = () => {
    if (para.length) blocks.push({ kind: 'para', text: inline(para.join(' ')) });
    para = [];
  };
  for (const line of md.replace(/\r\n?/g, '\n').split('\n')) {
    if (!line.trim()) { flush(); continue; }
    let m: RegExpExecArray | null;
    if ((m = /^ {0,3}#{1,6}\s+(.*?)(\s+#+)?\s*$/.exec(line))) {
      flush();
      blocks.push({ kind: 'heading', text: inline(m[1]) });
    } else if (/^ {0,3}([-*_])( *\1){2,} *$/.test(line)) {
      flush();
    } else if ((m = /^(\s*)(?:[-*+]|\d+[.)])\s+(.*)$/.exec(line))) {
      flush();
      blocks.push({ kind: 'item', text: inline(m[2]), nested: m[1].length >= 2 || undefined });
    } else if (/^\s/.test(line) && !para.length && blocks.at(-1)?.kind === 'item') {
      // A bullet's text that wraps onto the next line.
      const last = blocks[blocks.length - 1];
      last.text = `${last.text} ${inline(line)}`;
    } else {
      para.push(line.trim());
    }
  }
  flush();
  return blocks.filter((b) => b.text);
}
