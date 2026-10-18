// Editor groups (split editor, as in VS Code): the editor area shows up to
// MAX_GROUPS groups side by side (a row) or one above the other (a column),
// each with its own tab strip and active tab. The tabs stay one flat list in
// the tabs store; each tab names its group (`group`), and a group's tabs are
// that list filtered, in order.
//
// The focused group is the one new tabs go to, and the store's `activeId` is
// *its* active tab, so everything that reads `tabs.active` (status bar, AI,
// history, library…) follows the focused group without knowing about groups.
// The other groups keep their active tab in `EditorGroup.activeId` (the
// focused group's copy there is only written when it loses focus or is saved).
//
// Pure functions over a plain state object (the tabs store passes `this`),
// with no imports, so `node --test` runs them (editorGroups.test.mjs).

export const MAX_GROUPS = 3;
/** 'row': side by side; 'column': one above the other. */
export type SplitDir = 'row' | 'column';

export interface EditorGroup {
  id: string;
  /** Active tab; for the focused group, `Layout.activeId` is the live value. */
  activeId: string | null;
  /** Its share of the editor area (the shares add up to 1). */
  size: number;
}

export interface GroupedTab {
  id: string;
  connectionId: string;
  preview: boolean;
  /** The editor group; absent (saved before split editors) or unknown: the first one. */
  group?: string;
}

export interface Layout<T extends GroupedTab = GroupedTab> {
  tabs: T[];
  activeId: string | null;
  groups: EditorGroup[];
  focused: string;
  split: SplitDir;
  /** Connections whose (Chrome-style) tab group is collapsed. */
  collapsed: string[];
}

/** What's saved; every field but `tabs` may be missing (older versions). */
export interface SavedLayout<T extends GroupedTab = GroupedTab> {
  tabs?: T[];
  activeId?: string | null;
  collapsed?: string[];
  groups?: EditorGroup[];
  focused?: string;
  split?: SplitDir;
}

/** The group a tab is in (the first group when it names none that exists). */
export function groupOf(s: Layout, t: GroupedTab): string {
  return t.group && s.groups.some((g) => g.id === t.group) ? t.group : s.groups[0].id;
}

export function tabsOf<T extends GroupedTab>(s: Layout<T>, gid: string): T[] {
  return s.tabs.filter((t) => groupOf(s, t) === gid);
}

export function activeOf(s: Layout, gid: string): string | null {
  return gid === s.focused ? s.activeId : (s.groups.find((g) => g.id === gid)?.activeId ?? null);
}

export function setActive(s: Layout, gid: string, id: string | null) {
  if (gid === s.focused) s.activeId = id;
  else { const g = s.groups.find((x) => x.id === gid); if (g) g.activeId = id; }
}

/** Whether the tab is the one its group shows. */
export function isShown(s: Layout, t: GroupedTab): boolean {
  return activeOf(s, groupOf(s, t)) === t.id;
}

/** Make `gid` the focused group (new tabs go there; `activeId` is its active tab). */
export function focusGroup(s: Layout, gid: string) {
  if (gid === s.focused || !s.groups.some((g) => g.id === gid)) return;
  const old = s.groups.find((g) => g.id === s.focused);
  if (old) old.activeId = s.activeId;
  s.focused = gid;
  s.activeId = s.groups.find((g) => g.id === gid)!.activeId;
}

/** The groups as saved: the focused one's active tab written in. */
export function savedGroups(s: Layout): EditorGroup[] {
  return s.groups.map((g) => ({ ...g, activeId: activeOf(s, g.id) }));
}

/** Where a new tab of `connectionId` goes in the flat list, inside group
 *  `gid`: next to the group's active tab when it's of that connection, else
 *  at the end of that connection's tabs in the group, else at the group's end. */
export function slotFor(s: Layout, connectionId: string, gid: string = s.focused): number {
  const active = activeOf(s, gid);
  const at = s.tabs.findIndex((t) => t.id === active);
  if (at >= 0 && s.tabs[at].connectionId === connectionId) return at + 1;
  let sameConn = -1;
  let inGroup = -1;
  s.tabs.forEach((t, i) => {
    if (groupOf(s, t) !== gid) return;
    inGroup = i;
    if (t.connectionId === connectionId) sameConn = i;
  });
  if (sameConn >= 0) return sameConn + 1;
  return inGroup >= 0 ? inGroup + 1 : s.tabs.length;
}

/** The tab to show in place of `gone` (just taken out of the list, from
 *  index `at`): its neighbor in the group of the same connection, else a
 *  neighbor not in a collapsed connection group, else any such tab of the
 *  group, else a neighbor. */
export function neighborOf(s: Layout, gone: GroupedTab, at: number, gid: string): string | null {
  const mine = (t: GroupedTab | undefined): t is GroupedTab => !!t && groupOf(s, t) === gid;
  const after = s.tabs.slice(at).find(mine);
  const before = s.tabs.slice(0, at).reverse().find(mine);
  const near = [after, before].filter(mine);
  const shown = (t: GroupedTab) => !s.collapsed.includes(t.connectionId);
  return (near.find((t) => t.connectionId === gone.connectionId) ?? near.find(shown) ?? tabsOf(s, gid).find(shown) ?? near[0])?.id ?? null;
}

/** Remove a group (its tabs, if any, join the group before it). Its share
 *  goes to its neighbor; the focus, if it had it, too. The last group stays. */
export function removeGroup(s: Layout, gid: string) {
  if (s.groups.length < 2) return;
  const i = s.groups.findIndex((g) => g.id === gid);
  if (i < 0) return;
  const [g] = s.groups.splice(i, 1);
  const heir = s.groups[Math.max(0, i - 1)];
  heir.size += g.size;
  for (const t of s.tabs) if (t.group === gid) t.group = heir.id;
  if (s.focused === gid) {
    s.focused = heir.id;
    s.activeId = heir.activeId ?? tabsOf(s, heir.id)[0]?.id ?? null;
  }
}

/** After a tab left group `gid` (closed or moved away) from index `at`:
 *  pick the group's next active tab, and close the group if it's empty. */
export function afterLeaving(s: Layout, gone: GroupedTab, at: number, gid: string) {
  if (activeOf(s, gid) === gone.id) setActive(s, gid, neighborOf(s, gone, at, gid));
  if (!tabsOf(s, gid).length) removeGroup(s, gid);
}

/** Insert a new group after `afterGid`, taking half of that group's share. */
function addGroup(s: Layout, afterGid: string, newId: string): EditorGroup | null {
  if (s.groups.length >= MAX_GROUPS) return null;
  const i = s.groups.findIndex((g) => g.id === afterGid);
  const from = s.groups[i < 0 ? s.groups.length - 1 : i];
  const g: EditorGroup = { id: newId, activeId: null, size: from.size / 2 };
  from.size /= 2;
  s.groups.splice(s.groups.indexOf(from) + 1, 0, g);
  return g;
}

/** Move tab `id` into group `gid`, before tab `beforeId` (null: at the
 *  group's end), and show it there (the group gets the focus). A preview
 *  tab moved is pinned: a group holds at most one preview tab. */
export function moveTab(s: Layout, id: string, gid: string, beforeId: string | null = null) {
  const from = s.tabs.findIndex((t) => t.id === id);
  if (from < 0 || !s.groups.some((g) => g.id === gid) || beforeId === id) return;
  const t = s.tabs[from];
  const src = groupOf(s, t);
  s.tabs.splice(from, 1);
  if (src !== gid) {
    t.preview = false;
    afterLeaving(s, t, from, src);
  }
  // The source group may have gone (and its tabs joined another), so the
  // target is looked up again.
  const target = s.groups.some((g) => g.id === gid) ? gid : groupOf(s, t);
  t.group = target;
  const before = beforeId ? s.tabs.findIndex((x) => x.id === beforeId) : -1;
  let at: number;
  if (before >= 0) at = before;
  else {
    let last = -1;
    s.tabs.forEach((x, i) => { if (groupOf(s, x) === target) last = i; });
    at = last >= 0 ? last + 1 : s.tabs.length;
  }
  s.tabs.splice(at, 0, t);
  focusGroup(s, target);
  s.activeId = t.id;
}

/** Split: a new group after `afterGid` (`dir` lays every group out that way).
 *  With `id`, that tab moves into it, unless it's alone in `afterGid`'s
 *  place already (nothing to split). Without a tab the new group starts
 *  empty and focused: the next tab opens there. Returns the new group's id,
 *  or null when nothing changed (MAX_GROUPS reached, or nothing to move). */
export function splitInto(s: Layout, dir: SplitDir, afterGid: string, newId: string, id: string | null = null): string | null {
  const t = id ? s.tabs.find((x) => x.id === id) : undefined;
  if (id && !t) return null;
  if (t && groupOf(s, t) === afterGid && tabsOf(s, afterGid).length === 1) return null;
  const g = addGroup(s, afterGid, newId);
  if (!g) return null;
  s.split = dir;
  if (t) moveTab(s, t.id, g.id);
  else {
    focusGroup(s, g.id);
    s.activeId = null;
  }
  return s.groups.some((x) => x.id === g.id) ? g.id : null;
}

/** "Dividir a la derecha / abajo" on tab `id` (the focused one by default):
 *  the tab moves to a new group next to its own; a tab alone in its group
 *  leaves it in place and opens an empty group beside it. */
export function splitTab(s: Layout, dir: SplitDir, newId: string, id: string | null = s.activeId): string | null {
  const t = id ? s.tabs.find((x) => x.id === id) : undefined;
  const gid = t ? groupOf(s, t) : s.focused;
  const alone = !t || tabsOf(s, gid).length === 1;
  return splitInto(s, dir, gid, newId, alone ? null : t!.id);
}

/** The divider between groups `i - 1` and `i` was dragged: `frac` is the
 *  divider's place across the two groups (0..1). Each keeps at least 10%. */
export function resizeGroups(s: Layout, i: number, frac: number) {
  const a = s.groups[i - 1];
  const b = s.groups[i];
  if (!a || !b) return;
  const pair = a.size + b.size;
  const min = Math.min(0.1, pair / 2);
  a.size = Math.min(pair - min, Math.max(min, frac * pair));
  b.size = pair - a.size;
}

const fresh = (id: string): EditorGroup => ({ id, activeId: null, size: 1 });

/** The layout from what was saved, whatever version saved it: without
 *  groups (before split editors) every tab is in one group. Groups left
 *  without tabs go, shares are made to add up to 1, the active tabs are
 *  ones of their group, and the collapsed connections still have tabs and
 *  aren't the focused tab's. `firstId` names the group when there's none. */
export function restoreLayout<T extends GroupedTab>(saved: SavedLayout<T>, firstId: string): Layout<T> {
  const tabs = Array.isArray(saved.tabs) ? saved.tabs : [];
  let groups = (Array.isArray(saved.groups) ? saved.groups : [])
    .filter((g) => g && typeof g.id === 'string')
    .slice(0, MAX_GROUPS)
    .map((g) => ({ id: g.id, activeId: g.activeId ?? null, size: Number.isFinite(g.size) && g.size > 0 ? g.size : 1 }));
  if (!groups.length) groups = [fresh(firstId)];
  const s: Layout<T> = {
    tabs, groups, activeId: null, focused: groups[0].id,
    split: saved.split === 'column' ? 'column' : 'row', collapsed: [],
  };
  for (const t of tabs) t.group = groupOf(s, t);
  if (s.groups.length > 1) s.groups = s.groups.filter((g) => tabs.some((t) => t.group === g.id));
  if (!s.groups.length) s.groups = [groups[0]];
  const total = s.groups.reduce((n, g) => n + g.size, 0);
  for (const g of s.groups) g.size /= total;
  // Older saves: the one active tab is the (only) group's.
  if (!Array.isArray(saved.groups) || !saved.groups.length) s.groups[0].activeId = saved.activeId ?? null;
  for (const g of s.groups) {
    const own = tabsOf(s, g.id);
    if (!own.some((t) => t.id === g.activeId)) g.activeId = own[0]?.id ?? null;
  }
  s.focused = s.groups.some((g) => g.id === saved.focused) ? saved.focused! : s.groups[0].id;
  s.activeId = s.groups.find((g) => g.id === s.focused)!.activeId;
  const activeConn = new Set(s.groups.map((g) => tabs.find((t) => t.id === g.activeId)?.connectionId));
  s.collapsed = (saved.collapsed ?? []).filter((c) => tabs.some((t) => t.connectionId === c) && !activeConn.has(c));
  return s;
}

/** For a view's own window-wide shortcuts (⌘S, ⌘E…): the element is shown
 *  and, with the editor split, in the focused group (EditorArea marks the
 *  other groups' views). Outside the editor area, shown is enough. */
export function inFocusedEditor(el: HTMLElement | null | undefined): boolean {
  return !!el?.offsetParent && !el.closest('.ea-view.ea-unfocused');
}
