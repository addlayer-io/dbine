import { shallowRef } from 'vue';

// VS Code's quick pick: a floating list at the top of the window with a
// filter box (↑↓, Enter, Esc). One at a time; components/QuickPick.vue
// shows it (the status bar mounts it). `openQuickPick` resolves with the
// chosen item, or null when dismissed; `openQuickInput` turns the same box
// into a one-line input with validation (VS Code's input box).

export interface QuickPickItem {
  id: string;
  label: string;
  /** Dimmed text after the label. */
  description?: string;
  /** Marked as the one in use (a check, and selected when it opens). */
  current?: boolean;
  disabled?: boolean;
  /** An Element Plus icon before the label (`ei-grid`). */
  icon?: string;
  /** A shortcut, shown at the right (already formatted: ⇧⌘P). */
  keybinding?: string;
  /** The label's characters to highlight (what the filter matched). */
  matches?: number[];
  /** Starts a group: its name at the right and a line above (not on the first row). */
  group?: string;
}

export interface QuickPickOptions {
  placeholder: string;
  /** A getter, so a list that's still loading fills in while it's open. */
  items: () => QuickPickItem[];
  /** Still loading (a progress line under the box). */
  busy?: () => boolean;
  /** Shown when nothing is left after filtering. */
  empty?: string;
  /** Text already in the box when it opens. */
  value?: string;
  /** Filters and orders the items for the text typed (default: the label
   *  or the description contains it). */
  filter?: (items: QuickPickItem[], text: string) => QuickPickItem[];
  /** Called as the text changes; returning true hands over to another quick
   *  pick (VS Code's `>` in "Ir a…" opens the commands). */
  onText?: (text: string) => boolean;
}

export interface QuickInputOptions {
  placeholder: string;
  /** Explains what to type (under the box). */
  prompt: string;
  value?: string;
  /** An error message, or null when the text can be accepted. */
  validate?: (text: string) => string | null;
}

export type QuickPickRequest =
  | { kind: 'pick'; options: QuickPickOptions; resolve: (item: QuickPickItem | null) => void }
  | { kind: 'input'; options: QuickInputOptions; resolve: (text: string | null) => void };

/** The open quick pick (or input), if any. */
export const quickPick = shallowRef<QuickPickRequest | null>(null);

function dismissOpen() {
  const open = quickPick.value;
  quickPick.value = null;
  open?.resolve(null);
}

export function openQuickPick(options: QuickPickOptions): Promise<QuickPickItem | null> {
  dismissOpen();
  return new Promise((resolve) => { quickPick.value = { kind: 'pick', options, resolve }; });
}

export function openQuickInput(options: QuickInputOptions): Promise<string | null> {
  dismissOpen();
  return new Promise((resolve) => { quickPick.value = { kind: 'input', options, resolve }; });
}
