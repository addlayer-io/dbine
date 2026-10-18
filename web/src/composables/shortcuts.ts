// Keyboard shortcut helpers shared by the screens that take ⌘S / Ctrl+S.

import { matchesCommand } from './commands';

export { isMac } from './keybindings';

/** "Guardar": ⌘S on macOS, Ctrl+S on Windows / Linux, unless the user gave
 *  it other keys (Configuración › Atajos de teclado). */
export function isSaveShortcut(e: KeyboardEvent): boolean {
  return matchesCommand(e, 'file.save');
}

/** The key went to a code editor: CodeMirror runs its own ⌘S (`save`). */
export function inCodeEditor(e: Event): boolean {
  return e.target instanceof Element && !!e.target.closest('.cm-editor');
}

/** A dialog, drawer or message box is showing (Element Plus overlays). */
export function modalOpen(): boolean {
  return [...document.querySelectorAll('.el-overlay')].some((o) => o.getClientRects().length > 0);
}
