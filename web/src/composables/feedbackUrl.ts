// "Enviar comentarios": the prefilled GitHub issue-form URL. The user creates
// the issue in the browser; the app sends nothing and holds no token. The
// labels come from the form (.github/ISSUE_TEMPLATE/<kind>.yml), never from
// the URL. Issue forms prefill their fields by id from the query string.
//
// Pure (no imports) so `node --test` can run feedbackUrl.test.mjs on it.

export type FeedbackKind = 'bug' | 'enhancement' | 'driver-request' | 'question';

export const FEEDBACK_KINDS: FeedbackKind[] = ['bug', 'enhancement', 'driver-request', 'question'];

const NEW_ISSUE = 'https://github.com/addlayer-io/dbine/issues/new';
/** GitHub answers 414 a bit past 8 KB. */
export const MAX_URL = 7500;
export const MAX_TITLE = 120;
const TRUNCATED = '\n\n…(truncated)';

/** The only things the diagnostics can carry. There's no field for hosts,
 *  users, databases, connection names or queries, and nothing else is read. */
export interface FeedbackDiagnostics {
  app_version: string;
  os: string;
  arch: string;
  locale: string;
  /** The active connection's driver id (`postgres`), if any. */
  engine?: string | null;
  /** The active connection's server version, if any. */
  server_version?: string | null;
}

export interface FeedbackInput {
  kind: FeedbackKind;
  /** The issue title; for `driver-request`, the engine's name. */
  title: string;
  description: string;
  /** Steps to reproduce (bug only). */
  steps?: string;
  diagnostics?: FeedbackDiagnostics | null;
}

export interface FeedbackUrl {
  url: string;
  /** The description (or the steps) were cut to fit the URL. */
  truncated: boolean;
}

/** Control characters out (tabs and newlines stay), lone surrogates
 *  replaced (`encodeURIComponent` throws on them), CRLF made LF. */
function clean(s: string): string {
  return s
    .replace(/\r\n?/g, '\n')
    // eslint-disable-next-line no-control-regex
    .replace(/[\u0000-\u0008\u000B-\u001F\u007F-\u009F]/g, '')
    .replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, '�');
}

/** One line, at most `max` characters (code points, so emoji aren't split). */
function line(s: string, max: number): string {
  return [...clean(s).replace(/\s+/g, ' ').trim()].slice(0, max).join('');
}

/** The diagnostics block, exactly as it goes in the issue (the dialog shows it). */
export function diagnosticsText(d: FeedbackDiagnostics): string {
  const rows = [
    `DBine: ${line(d.app_version, 40)}`,
    `OS: ${line(d.os, 60)}${d.arch ? ` (${line(d.arch, 20)})` : ''}`,
    `Language: ${line(d.locale, 16)}`,
  ];
  const engine = d.engine && /^[a-z0-9_-]{1,40}$/.test(d.engine) ? d.engine : '';
  if (engine) {
    rows.push(`Engine: ${engine}`);
    // Only its first line: some engines' version text goes on (build, compiler).
    const version = d.server_version ? line(d.server_version.split('\n')[0], 120) : '';
    if (version) rows.push(`Server: ${version}`);
  }
  return rows.join('\n');
}

export function templateFor(kind: FeedbackKind): string {
  return `${kind}.yml`;
}

function query(params: [string, string][]): string {
  return params.filter(([, v]) => v).map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join('&');
}

/** Cut `text` to the longest prefix (with the truncation note) that keeps
 *  `build(text)` within `max`, or `null` when not even an empty one fits. */
function fit(text: string, max: number, build: (t: string) => string): string | null {
  const chars = [...text];
  if (build('').length > max) return null;
  let lo = 0;
  let hi = chars.length;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    if (build(chars.slice(0, mid).join('') + TRUNCATED).length <= max) lo = mid;
    else hi = mid - 1;
  }
  return lo ? chars.slice(0, lo).join('') + TRUNCATED : '';
}

/** The issue form's URL, prefilled. Longer than `max`, the description is
 *  cut first and then the steps, each with a "…(truncated)" note. */
export function buildFeedbackUrl(input: FeedbackInput, max = MAX_URL): FeedbackUrl {
  const name = line(input.title, MAX_TITLE);
  const head: [string, string][] = [['template', templateFor(input.kind)]];
  if (input.kind === 'driver-request') {
    head.push(['title', `Support for ${name}`], ['engine', name]);
  } else {
    head.push(['title', name]);
  }
  const diagnostics: [string, string][] = input.diagnostics ? [['diagnostics', diagnosticsText(input.diagnostics)]] : [];
  let description = clean(input.description).trim();
  let steps = input.kind === 'bug' ? clean(input.steps ?? '').trim() : '';
  const url = (d: string, s: string) =>
    `${NEW_ISSUE}?${query([...head, ['description', d], ['steps', s], ...diagnostics])}`;

  let full = url(description, steps);
  if (full.length <= max) return { url: full, truncated: false };

  const d = fit(description, max, (t) => url(t, steps));
  if (d !== null) description = d;
  else {
    // The steps alone are too long: they're cut, the description keeps
    // what's left.
    description = '';
    const s = fit(steps, max, (t) => url('', t));
    steps = s ?? '';
    description = fit(clean(input.description).trim(), max, (t) => url(t, steps)) ?? '';
  }
  full = url(description, steps);
  return { url: full, truncated: true };
}
