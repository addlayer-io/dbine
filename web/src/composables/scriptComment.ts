// Text that goes after a line comment (`-- `, `// `) in a script the user can
// run. Warnings and messages carry names and expressions from the server; a
// line break in them would end the comment and leave the rest as a statement.
// Every control character and line separator (NEL, U+2028, U+2029, which
// JavaScript-like shells also read as line ends) becomes '?'.

const BREAKS = /[\p{Cc}\u2028\u2029\u0085]/gu;

export function commentSafe(text: string): string {
  return text.replace(BREAKS, '?');
}

/** One line comment: `marker` ("--", "//"), a space and the text, on one line. */
export function lineComment(marker: string, text: string): string {
  return `${marker} ${commentSafe(text)}`;
}
