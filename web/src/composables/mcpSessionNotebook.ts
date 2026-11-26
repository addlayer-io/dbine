import type { McpActivity, McpSession } from '../api/mcp';
import type { Notebook, NotebookCell, NotebookTarget } from '../api/types';
import { emptyNotebook, newCell } from './notebookModel';

// An MCP session (docs/mcp.md, "Sessions") as an untitled notebook: what an
// agent did, to go on from there. The queries it read with are SQL cells on
// the connection and database it used; what it only looked at (connections,
// objects, their structure) goes in one line of text; its writes, approved or
// not, are text with the code, so "Run all" never runs them again. No rows:
// the user runs the cells. No Vue, no i18n: the texts come in `text`.

/** The notebook's texts, in the UI's language. */
export interface SessionText {
  /** The first cell's title: the client and when. */
  title: (s: McpSession) => string;
  /** Its counts and connections. */
  summary: (s: McpSession) => string;
  /** What it looked at, as a list. */
  explored: (items: string) => string;
  /** A plan it asked for (`explain`). */
  plan: () => string;
  /** A write that ran, approved at `at`. */
  wrote: (at: string, rows: number | null) => string;
  /** Something the user rejected (`timeout`: left unanswered). */
  rejected: (timeout: boolean) => string;
  /** A call that failed, with the server's error. */
  failed: (error: string) => string;
  /** A query the log didn't keep whole. */
  notKept: (summary: string) => string;
}

const READS = new Set(['run_query', 'explain']);

const split = (tool: string): [string, string | null] => {
  const i = tool.indexOf(':');
  return i < 0 ? [tool, null] : [tool.slice(0, i), tool.slice(i + 1)];
};

/** `code` fenced for markdown, with a fence longer than any run of backticks in it. */
export function fenced(code: string, language = 'sql'): string {
  const longest = Math.max(0, ...[...code.matchAll(/`+/g)].map((m) => m[0].length));
  const fence = '`'.repeat(Math.max(3, longest + 1));
  return `${fence}${language}\n${code}\n${fence}`;
}

const targetOf = (c: McpActivity): NotebookTarget => ({
  connection: c.connection || null,
  database: c.database || null,
  environment: null,
});

const markdown = (source: string): NotebookCell => newCell('markdown', source);

/** What an exploring call looked at, for the list. */
function looked(c: McpActivity): string {
  const where = [c.connection, c.summary].filter(Boolean).join(' › ');
  return where ? `\`${split(c.tool)[0]}\` ${where}` : `\`${split(c.tool)[0]}\``;
}

/** The session's notebook. `time` formats a call's time for the notes. */
export function sessionNotebook(session: McpSession, calls: McpActivity[], text: SessionText, time: (iso: string) => string): Notebook {
  const cells: NotebookCell[] = [markdown(`## ${text.title(session)}\n\n${text.summary(session)}`)];
  let explored: string[] = [];
  const flush = () => {
    if (explored.length) cells.push(markdown(text.explored(explored.map((x) => `- ${x}`).join('\n'))));
    explored = [];
  };
  let first: NotebookTarget | null = null;
  for (const c of calls) {
    const [tool, phase] = split(c.tool);
    if (phase === 'request') continue;
    const refused = phase === 'rejected' || phase === 'timeout';
    if (tool === 'execute') {
      flush();
      const note = refused ? text.rejected(phase === 'timeout')
        : !c.ok ? text.failed(c.error ?? '')
          : text.wrote(time(c.at), c.rows);
      cells.push(markdown(c.code ? `${note}\n\n${fenced(c.code)}` : `${note}\n\n${c.summary}`));
      continue;
    }
    if (!READS.has(tool)) {
      const item = looked(c);
      if (!explored.includes(item)) explored.push(item);
      continue;
    }
    flush();
    if (refused) {
      cells.push(markdown(c.code ? `${text.rejected(phase === 'timeout')}\n\n${fenced(c.code)}` : text.rejected(phase === 'timeout')));
      continue;
    }
    if (!c.code) {
      cells.push(markdown(text.notKept(c.summary)));
      continue;
    }
    if (!c.ok) cells.push(markdown(text.failed(c.error ?? '')));
    else if (tool === 'explain') cells.push(markdown(text.plan()));
    const sql = newCell('sql', c.code);
    sql.target = targetOf(c);
    first ??= sql.target;
    cells.push(sql);
  }
  flush();
  const nb = emptyNotebook(false, first);
  nb.cells = cells;
  return nb;
}
