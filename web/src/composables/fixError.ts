// "Analizar con IA" on a failed query: what is asked, how the answer is read
// and when a proposal no longer fits the editor. Pure (tested in
// fixError.test.mjs). The model never gets rows: only the SQL that failed,
// the engine and the error text.

/** Statements that change data or structure: flagged whatever the model says. */
const DESTRUCTIVE = /\b(drop|delete|truncate|alter|update|insert|merge|create|rename|grant|revoke|flushall|flushdb|del|dropdatabase|deletemany|remove)\b/i;
export function destructive(code: string): boolean {
  // Ignore comments and string literals.
  const bare = code.replace(/--[^\n]*|\/\*[\s\S]*?\*\/|'(?:[^']|'')*'/g, ' ');
  return DESTRUCTIVE.test(bare);
}

const MAX_SQL = 20_000;
const MAX_ERROR = 4_000;

/** The question for the model. The answer's language is the system
 *  prompt's (the UI language); the format is fixed so DBine can read it. */
export function buildFixPrompt(o: { sql: string; error: string; engine?: string }): string {
  const cut = (s: string, n: number) => (s.length > n ? `${s.slice(0, n)}\n…` : s);
  return [
    'The query below failed on the database. Explain the cause in one to three short sentences, in plain language, then give the corrected full query.',
    'Answer in exactly this format and nothing else:',
    '<explanation>the cause</explanation>',
    '```sql',
    'the corrected full query',
    '```',
    'Use one code block only. Do not run or claim to have run anything. If you cannot tell how to fix it, give only the explanation.',
    ...(o.engine ? [`Engine: ${o.engine}`] : []),
    '<failed_query>',
    cut(o.sql, MAX_SQL),
    '</failed_query>',
    '<error>',
    cut(o.error, MAX_ERROR),
    '</error>',
  ].join('\n');
}

export interface FixAnswer {
  explanation: string;
  /** The corrected query, when the answer has one. */
  code: string | null;
  /** The code block is closed (while streaming, the code isn't final yet). */
  complete: boolean;
}

/** Reads an answer, also while it streams: `<explanation>` (or the text
 *  before the first code block) and the first fenced block. */
export function parseFixAnswer(raw: string): FixAnswer {
  // A reasoning model's <think> goes out, finished or not.
  const text = raw.replace(/<think>[\s\S]*?(<\/think>|$)/gi, '');
  const fence = /```[^\n`]*\n?/.exec(text);
  const before = fence ? text.slice(0, fence.index) : text;
  const tag = /<explanation>([\s\S]*?)(<\/explanation>|$)/i.exec(before);
  const explanation = (tag ? tag[1] : before.replace(/<\/?explanation>/gi, '')).trim();
  if (!fence) return { explanation, code: null, complete: true };
  const rest = text.slice(fence.index + fence[0].length);
  const end = rest.indexOf('```');
  if (end < 0) return { explanation, code: null, complete: false };
  const code = rest.slice(0, end).replace(/\s+$/, '').replace(/^\n+/, '');
  return { explanation, code: code.trim() ? code : null, complete: true };
}

/** Is there something to propose: code that differs from what ran. */
export function isChange(original: string, code: string | null): code is string {
  return !!code && code.trim() !== original.trim();
}

/** The proposal fits the editor only while the text it was made for is
 *  still at the same place. */
export function isStale(editorText: string, at: { from: number; to: number; original: string }): boolean {
  return editorText.slice(at.from, at.to) !== at.original;
}

/** The corrected query in the shape of what ran: a model's answer rarely
 *  keeps a trailing newline or the `;` a statement had. */
export function fitEnds(original: string, code: string): string {
  let out = code.replace(/\s+$/, '');
  const trail = /\s*$/.exec(original)?.[0] ?? '';
  const semi = /;\s*$/.test(original);
  if (semi && !out.endsWith(';')) out += ';';
  return out + trail;
}
