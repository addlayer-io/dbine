// Keyboard shortcut helpers shared by the screens that take ⌘S / Ctrl+S.

/** macOS uses ⌘ for shortcuts; Windows and Linux use Ctrl (as CodeMirror's `Mod`). */
export const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);

/** ⌘S on macOS, Ctrl+S on Windows / Linux. */
export function isSaveShortcut(e: KeyboardEvent): boolean {
  const mod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  return mod && !e.altKey && !e.shiftKey && e.key.toLowerCase() === 's';
}

/** The key went to a code editor: CodeMirror runs its own ⌘S (`save`). */
export function inCodeEditor(e: Event): boolean {
  return e.target instanceof Element && !!e.target.closest('.cm-editor');
}

/** A dialog, drawer or message box is showing (Element Plus overlays). */
export function modalOpen(): boolean {
  return [...document.querySelectorAll('.el-overlay')].some((o) => o.getClientRects().length > 0);
}
