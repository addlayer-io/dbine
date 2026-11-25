/**
 * A formatter failure as one short message: the parser's first lines (where
 * and what it didn't expect), never its list of expected tokens, which can run
 * to thousands of characters.
 */
export function formatErrorSummary(message: string, maxLength = 240): string {
  const lines = message
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean);
  const first = lines.slice(0, 2).join(' · ');
  const cut = first.length > maxLength || lines.length > 2;
  return cut ? `${first.slice(0, maxLength).trimEnd()}…` : first;
}
