// Key combinations as text, with no app state: parse, compare, show and
// check for conflicts. `commands.ts` (the action registry) builds on it.
// No imports, so `node --test` runs keybindings.test.mjs on it directly.
//
// A binding is written as CodeMirror writes keys: modifiers then the key,
// joined with `-` ("Mod-Enter", "Shift-Mod-l", "Shift-Alt-f"). `Mod` is ⌘ on
// macOS and Ctrl elsewhere; `Ctrl` on macOS is the Control key. The canonical
// form (`normalizeKey`) orders the modifiers Mod, Ctrl, Alt, Shift and writes
// letters in lower case, so two spellings of the same keys compare equal.

/** macOS uses ⌘ for shortcuts; Windows and Linux use Ctrl (as CodeMirror's `Mod`). */
export const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);

const MOD_ORDER = ['Mod', 'Ctrl', 'Alt', 'Shift'] as const;
type Modifier = (typeof MOD_ORDER)[number];

/** Key names as `KeyboardEvent.key` gives them, for the few that have aliases. */
const ALIASES: Record<string, string> = {
  esc: 'Escape', escape: 'Escape', enter: 'Enter', return: 'Enter', space: 'Space', ' ': 'Space',
  tab: 'Tab', backspace: 'Backspace', delete: 'Delete', del: 'Delete', home: 'Home', end: 'End',
  pageup: 'PageUp', pagedown: 'PageDown', up: 'ArrowUp', down: 'ArrowDown', left: 'ArrowLeft', right: 'ArrowRight',
  arrowup: 'ArrowUp', arrowdown: 'ArrowDown', arrowleft: 'ArrowLeft', arrowright: 'ArrowRight', insert: 'Insert',
};

function keyName(k: string): string {
  if (k.length === 1) return k.toLowerCase();
  const alias = ALIASES[k.toLowerCase()];
  if (alias) return alias;
  if (/^f\d{1,2}$/i.test(k)) return k.toUpperCase();
  return k;
}

/** The canonical spelling of `key`, or null when it has no key at all.
 *  `mac`: Ctrl means the Control key there; elsewhere Ctrl is Mod. */
export function normalizeKey(key: string, mac: boolean): string | null {
  // "Mod--" is Mod + minus: the last part is the key even when it's `-`.
  const parts = key.endsWith('--') ? [...key.slice(0, -2).split('-'), '-'] : key.split('-');
  const name = parts.pop();
  if (!name) return null;
  const mods = new Set<Modifier>();
  for (const p of parts) {
    const m = p.toLowerCase();
    if (m === 'mod' || m === 'cmd' || m === 'meta' || m === 'm') mods.add('Mod');
    else if (m === 'ctrl' || m === 'control' || m === 'c') mods.add(mac ? 'Ctrl' : 'Mod');
    else if (m === 'alt' || m === 'option' || m === 'a') mods.add('Alt');
    else if (m === 'shift' || m === 's') mods.add('Shift');
    else return null;
  }
  return [...MOD_ORDER.filter((m) => mods.has(m)), keyName(name)].join('-');
}

/** `KeyboardEvent.code` → key, for keys whose `key` changes with Shift or
 *  Alt (⌥G is "©" on macOS): letters, digits and punctuation. */
const CODES: Record<string, string> = {
  Comma: ',', Period: '.', Slash: '/', Backslash: '\\', Semicolon: ';', Quote: "'", BracketLeft: '[',
  BracketRight: ']', Minus: '-', Equal: '=', Backquote: '`', Space: 'Space', IntlBackslash: '<',
};

/** The modifiers alone: not a combination yet (while recording one). */
export function isModifierOnly(e: { key: string }): boolean {
  return ['Shift', 'Control', 'Alt', 'Meta', 'OS', 'AltGraph', 'CapsLock', 'Fn'].includes(e.key);
}

type KeyEventLike = Pick<KeyboardEvent, 'key' | 'code' | 'metaKey' | 'ctrlKey' | 'altKey' | 'shiftKey'>;

/** The canonical key of a keydown event. */
export function eventKey(e: KeyEventLike, mac: boolean): string | null {
  // The Windows key isn't one of ours (the system takes it).
  if (isModifierOnly(e) || (!mac && e.metaKey)) return null;
  let name: string;
  const letter = /^Key([A-Z])$/.exec(e.code ?? '');
  const digit = /^(?:Digit|Numpad)(\d)$/.exec(e.code ?? '');
  if (letter) name = letter[1].toLowerCase();
  else if (digit) name = digit[1];
  else if (e.code && CODES[e.code]) name = CODES[e.code];
  else if (e.code === 'NumpadEnter') name = 'Enter';
  else name = e.key;
  if (!name || name === 'Unidentified' || name === 'Dead') return null;
  const mods: string[] = [];
  if (mac ? e.metaKey : e.ctrlKey) mods.push('Mod');
  if (mac && e.ctrlKey) mods.push('Ctrl');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  return normalizeKey([...mods, name].join('-'), mac);
}

const MAC_SYMBOLS: Record<string, string> = {
  Mod: '⌘', Ctrl: '⌃', Alt: '⌥', Shift: '⇧', Enter: '↵', Escape: '⎋', Backspace: '⌫', Delete: '⌦', Tab: '⇥',
  ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→', Space: 'Space',
};
const PC_NAMES: Record<string, string> = {
  Mod: 'Ctrl', Alt: 'Alt', Shift: 'Shift', Escape: 'Esc', ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→',
};

function splitKey(key: string): { mods: string[]; name: string } {
  const parts = key.endsWith('--') ? [...key.slice(0, -2).split('-'), '-'] : key.split('-');
  const name = parts.pop() ?? '';
  return { mods: parts, name };
}

/** How the binding reads in menus and settings: "⇧⌘↵" on macOS,
 *  "Ctrl+Shift+Enter" elsewhere. */
export function formatKey(key: string | null, mac: boolean): string {
  if (!key) return '';
  const norm = normalizeKey(key, mac);
  if (!norm) return '';
  const { mods, name } = splitKey(norm);
  const shown = name.length === 1 ? name.toUpperCase() : name;
  if (mac) {
    // macOS order: ⌃⌥⇧⌘.
    const order = ['Ctrl', 'Alt', 'Shift', 'Mod'];
    return [...order.filter((m) => mods.includes(m)).map((m) => MAC_SYMBOLS[m]), MAC_SYMBOLS[name] ?? shown].join('');
  }
  return [...mods.map((m) => PC_NAMES[m] ?? m), PC_NAMES[name] ?? shown].join('+');
}

/** The binding as a Tauri menu accelerator ("CmdOrCtrl+Shift+N"), or
 *  undefined when the menu can't show it. */
export function toAccelerator(key: string | null, mac: boolean): string | undefined {
  const norm = key ? normalizeKey(key, mac) : null;
  if (!norm) return undefined;
  const { mods, name } = splitKey(norm);
  const names: Record<string, string> = {
    ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right', Escape: 'Esc',
  };
  const out = mods.map((m) => (m === 'Mod' ? 'CmdOrCtrl' : m === 'Ctrl' ? 'Ctrl' : m));
  out.push(names[name] ?? (name.length === 1 ? name.toUpperCase() : name));
  return out.join('+');
}

// -- conflicts --------------------------------------------------------------------------------

/** Where a binding acts. `global`: anywhere in the window; `editor`: while
 *  a code editor has the focus. A global binding and an editor binding with
 *  the same keys clash inside the editor (the editor wins there). */
export type KeyScope = 'global' | 'editor';

export interface BoundCommand {
  id: string;
  key: string | null;
  scope: KeyScope;
}

function overlap(a: KeyScope, b: KeyScope): boolean {
  return a === b || a === 'global' || b === 'global';
}

/** Commands that share their keys with another one where both act:
 *  id → the ids it clashes with. Commands without keys never clash. */
export function findConflicts(items: BoundCommand[], mac: boolean): Map<string, string[]> {
  const byKey = new Map<string, BoundCommand[]>();
  for (const it of items) {
    const k = it.key ? normalizeKey(it.key, mac) : null;
    if (!k) continue;
    byKey.set(k, [...(byKey.get(k) ?? []), it]);
  }
  const out = new Map<string, string[]>();
  for (const group of byKey.values()) {
    for (const a of group) {
      const others = group.filter((b) => b.id !== a.id && overlap(a.scope, b.scope)).map((b) => b.id);
      if (others.length) out.set(a.id, others);
    }
  }
  return out;
}

/** System and text-editing keys a binding would take away (copy, paste,
 *  undo, quit…): allowed, but the settings warn. */
const RESERVED = ['Mod-c', 'Mod-v', 'Mod-x', 'Mod-z', 'Mod-Shift-z', 'Mod-a', 'Mod-y'];
const RESERVED_MAC = ['Mod-h', 'Mod-m', 'Mod-Alt-h', 'Mod-Ctrl-f'];
const RESERVED_PC = ['Alt-F4'];

export function isReservedKey(key: string | null, mac: boolean): boolean {
  const k = key ? normalizeKey(key, mac) : null;
  if (!k) return false;
  return [...RESERVED, ...(mac ? RESERVED_MAC : RESERVED_PC)].some((r) => normalizeKey(r, mac) === k);
}
