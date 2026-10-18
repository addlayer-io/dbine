import { computed, reactive, ref } from 'vue';
import { useSettingsStore } from '../stores/settings';
import { eventKey, findConflicts, formatKey, isMac, isReservedKey, normalizeKey, toAccelerator, type KeyScope } from './keybindings';

// The app's actions in one registry: what each one is called, its default
// keys, where they act, and who runs it right now. The keyboard dispatchers
// (App.vue for the window, CodeEditor for the editors, the macOS menu) and
// Configuración › Atajos de teclado read it; the command palette lists it.
//
// - Definitions (`COMMANDS`) are static data: id, label key, group, scope
//   and default keys. More can be added at runtime with `defineCommand`.
// - Handlers are registered by whoever can run the action at the moment
//   (`registerCommand`, which returns the function that removes it). Several
//   may be registered for one id (one per open results pane, the focused
//   editor…): the newest whose `when()` holds runs.
// - The user's keys are overrides kept in the synced settings
//   (`keys.bindings`: id → key, or null for "no keys"); `bindingOf` is the
//   effective one and is reactive, so everything follows a change at once.

/** `database` / `connection` / `settings` / `help`: the command palette's (composables/workbenchCommands.ts). */
export type CommandGroup = 'general' | 'view' | 'editor' | 'results' | 'database' | 'connection' | 'settings' | 'help';

export interface CommandDef {
  id: string;
  /** i18n key of the action's name ("shortcuts:cmd.view.explorer"). */
  label: string;
  group: CommandGroup;
  /** `global`: the window's key handler runs it; `editor`: the focused code
   *  editor does (its keys reach nothing else). */
  scope: KeyScope;
  /** Default keys, CodeMirror style ("Mod-Enter"); null: none. */
  key: string | null;
  /** Default keys on macOS when they differ (null: none there). */
  mac?: string | null;
  /** Shown in the shortcuts list and the palette, but its keys can't change
   *  (none yet; kept for actions whose keys the system owns). */
  fixed?: boolean;
}

/** Every keyboard shortcut of the app (window and editor). */
export const COMMANDS: CommandDef[] = reactive([
  // General
  { id: 'file.newQuery', label: 'shortcuts:cmd.file.newQuery', group: 'general', scope: 'global', key: 'Mod-n' },
  { id: 'file.newWindow', label: 'shortcuts:cmd.file.newWindow', group: 'general', scope: 'global', key: 'Mod-Shift-n' },
  { id: 'file.open', label: 'shortcuts:cmd.file.open', group: 'general', scope: 'global', key: 'Mod-o' },
  { id: 'file.closeTab', label: 'shortcuts:cmd.file.closeTab', group: 'general', scope: 'global', key: 'Mod-w' },
  { id: 'file.save', label: 'shortcuts:cmd.file.save', group: 'general', scope: 'editor', key: 'Mod-s' },
  { id: 'app.settings', label: 'shortcuts:cmd.app.settings', group: 'general', scope: 'global', key: 'Mod-,' },
  // Only the macOS menu quits with keys; elsewhere the window's close does.
  { id: 'app.quit', label: 'shortcuts:cmd.app.quit', group: 'general', scope: 'global', key: null, mac: 'Mod-q' },
  // View
  { id: 'view.explorer', label: 'shortcuts:cmd.view.explorer', group: 'view', scope: 'global', key: 'Mod-b' },
  { id: 'view.output', label: 'shortcuts:cmd.view.output', group: 'view', scope: 'global', key: 'Mod-j' },
  { id: 'view.ai', label: 'shortcuts:cmd.view.ai', group: 'view', scope: 'global', key: 'Mod-i' },
  // Editor
  { id: 'editor.run', label: 'shortcuts:cmd.editor.run', group: 'editor', scope: 'editor', key: 'Mod-Enter' },
  { id: 'editor.runStatement', label: 'shortcuts:cmd.editor.runStatement', group: 'editor', scope: 'editor', key: 'Mod-Shift-Enter' },
  { id: 'editor.planEstimated', label: 'shortcuts:cmd.editor.planEstimated', group: 'editor', scope: 'editor', key: 'Mod-l' },
  { id: 'editor.planActual', label: 'shortcuts:cmd.editor.planActual', group: 'editor', scope: 'editor', key: 'Mod-Shift-l' },
  { id: 'editor.format', label: 'shortcuts:cmd.editor.format', group: 'editor', scope: 'editor', key: 'Alt-Shift-f' },
  { id: 'editor.comment', label: 'shortcuts:cmd.editor.comment', group: 'editor', scope: 'editor', key: 'Mod-/' },
  { id: 'editor.find', label: 'shortcuts:cmd.editor.find', group: 'editor', scope: 'editor', key: 'Mod-f' },
  { id: 'editor.gotoLine', label: 'shortcuts:cmd.editor.gotoLine', group: 'editor', scope: 'editor', key: 'Mod-Alt-g' },
  { id: 'editor.autocomplete', label: 'shortcuts:cmd.editor.autocomplete', group: 'editor', scope: 'editor', key: 'Ctrl-Space' },
  { id: 'editor.toggleVim', label: 'shortcuts:cmd.editor.toggleVim', group: 'editor', scope: 'global', key: null },
  // Results
  { id: 'results.export', label: 'shortcuts:cmd.results.export', group: 'results', scope: 'global', key: 'Mod-e' },
]);

/** Add a command (the palette's own, another feature's). Ignored when the id
 *  exists. */
export function defineCommand(def: CommandDef) {
  if (!COMMANDS.some((c) => c.id === def.id)) COMMANDS.push(def);
}

export function commandDef(id: string): CommandDef | undefined {
  return COMMANDS.find((c) => c.id === id);
}

// -- handlers ---------------------------------------------------------------------------------

interface Handler { run: () => void; when?: () => boolean }
const handlers = new Map<string, Handler[]>();

/** Who runs `id` now. Returns the function that unregisters it. */
export function registerCommand(id: string, run: () => void, when?: () => boolean): () => void {
  const h: Handler = { run, when };
  handlers.set(id, [...(handlers.get(id) ?? []), h]);
  return () => handlers.set(id, (handlers.get(id) ?? []).filter((x) => x !== h));
}

function handlerFor(id: string): Handler | undefined {
  const list = handlers.get(id) ?? [];
  for (let i = list.length - 1; i >= 0; i--) if (!list[i].when || list[i].when!()) return list[i];
  return undefined;
}

/** Something can run `id` right now. */
export function commandAvailable(id: string): boolean {
  return !!handlerFor(id);
}

/** Run `id`; false when nothing can run it now. */
export function runCommand(id: string): boolean {
  const h = handlerFor(id);
  if (!h) return false;
  h.run();
  return true;
}

// -- bindings ---------------------------------------------------------------------------------

export const BINDINGS_SETTING = 'keys.bindings';
/** "Modo Vim en el editor" (off by default). */
export const VIM_SETTING = 'editor.vim';

function overrides(): Record<string, string | null> {
  try {
    return useSettingsStore().get<Record<string, string | null>>(BINDINGS_SETTING, {});
  } catch {
    return {}; // no Pinia (a test page): defaults only
  }
}

export function defaultBinding(c: CommandDef): string | null {
  const k = isMac && c.mac !== undefined ? c.mac : c.key;
  return k ? normalizeKey(k, isMac) : null;
}

/** The keys `id` runs with now (canonical form), or null. Reactive. */
export function bindingOf(id: string): string | null {
  const c = commandDef(id);
  if (!c) return null;
  const o = overrides();
  if (!c.fixed && id in o) return o[id] ? normalizeKey(o[id]!, isMac) : null;
  return defaultBinding(c);
}

/** The keys as the user reads them ("⇧⌘↵", "Ctrl+Shift+Enter"). */
export function bindingLabel(id: string): string {
  return formatKey(bindingOf(id), isMac);
}

/** As a Tauri menu accelerator (the macOS menu). */
export function bindingAccelerator(id: string): string | undefined {
  return toAccelerator(bindingOf(id), isMac);
}

export function isOverridden(id: string): boolean {
  return id in overrides() && bindingOf(id) !== defaultBinding(commandDef(id)!);
}

/** Give `id` these keys (null: no keys). Back to the default drops the override. */
export async function setBinding(id: string, key: string | null) {
  const c = commandDef(id);
  if (!c || c.fixed) return;
  const next = { ...overrides() };
  const norm = key ? normalizeKey(key, isMac) : null;
  if (norm === defaultBinding(c)) delete next[id];
  else next[id] = norm;
  await useSettingsStore().set(BINDINGS_SETTING, next);
}

export async function resetBinding(id: string) {
  const next = { ...overrides() };
  delete next[id];
  await useSettingsStore().set(BINDINGS_SETTING, next);
}

export async function resetAllBindings() {
  await useSettingsStore().set(BINDINGS_SETTING, {});
}

/** id → the ids it shares keys with (where both act). Reactive. */
export const conflicts = computed(() =>
  findConflicts(COMMANDS.map((c) => ({ id: c.id, key: bindingOf(c.id), scope: c.scope })), isMac));

export function isReserved(id: string): boolean {
  return isReservedKey(bindingOf(id), isMac);
}

/** The key of an event in the bindings' form. */
export function keyOfEvent(e: KeyboardEvent): string | null {
  return eventKey(e, isMac);
}

/** The command of `scope` bound to this event, if any. */
export function commandForEvent(e: KeyboardEvent, scope: KeyScope): string | null {
  if (e.isComposing) return null;
  const k = eventKey(e, isMac);
  if (!k) return null;
  return COMMANDS.find((c) => c.scope === scope && bindingOf(c.id) === k)?.id ?? null;
}

/** The event is `id`'s keys. */
export function matchesCommand(e: KeyboardEvent, id: string): boolean {
  const b = bindingOf(id);
  return !!b && !e.isComposing && eventKey(e, isMac) === b;
}

/** Defaults of `scope`'s commands that were moved to other keys and that no
 *  command uses now: the editor swallows them, so CodeMirror's built-in
 *  binding (⌘/ for comment…) doesn't keep running the action. */
export function freedDefault(e: KeyboardEvent, scope: KeyScope): boolean {
  const k = eventKey(e, isMac);
  if (!k || COMMANDS.some((c) => bindingOf(c.id) === k)) return false;
  return COMMANDS.some((c) => c.scope === scope && defaultBinding(c) === k && bindingOf(c.id) !== k);
}

/** Configuración is recording a new binding: the key handlers stand aside
 *  (and the macOS menu drops its accelerators so ⌘N & co. reach the page). */
export const recordingKeys = ref(false);
