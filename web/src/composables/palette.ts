import { reactive } from 'vue';
import { ElMessage } from 'element-plus';
import { api } from '../api/client';
import type { DbObject, SavedConnection, SavedQuery } from '../api/types';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { readJson, writeJson } from '../stores/storage';
import { useTabsStore, type Tab } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { COMMANDS, bindingLabel, bindingOf, commandAvailable, runCommand } from './commands';
import { fuzzyMatch } from './fuzzy';
import { KIND_ICONS } from './kindIcons';
import { openQuickPick, type QuickPickItem } from './quickPick';
import { tabTitle } from './tabTitle';

// VS Code's command palette and "Go to", on the quick pick:
//
// - ⌘⇧P / Ctrl+Shift+P: the action registry's commands (composables/
//   commands.ts) that something can run now, fuzzy-matched, the recently used first. It opens with
//   ">" typed; erasing it turns the box into "Ir a…".
// - ⌘P / Ctrl+P "Ir a…": open tabs, connections, databases, their objects
//   (tables, views, routines…) and saved queries, from what the explorer
//   has loaded and its cache (cache.db, no server is asked). Typing ">"
//   switches to the commands.
//
// A command that needs a connection or a database takes the active tab's,
// or asks for it with a second quick pick (`resolveConnection`,
// `resolveDatabase`).

const RECENT_KEY = 'dbine.palette.recent';
const MAX_RECENT = 12;
/** "Ir a…" lists at most this many matches (the list isn't virtual). */
const MAX_GOTO = 150;

function recentIds(): string[] {
  const v = readJson<unknown>(RECENT_KEY, []);
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
}
function remember(id: string) {
  writeJson(RECENT_KEY, [id, ...recentIds().filter((x) => x !== id)].slice(0, MAX_RECENT));
}

/** Fuzzy filter: the matches by score, with the label's matched characters
 *  to highlight. `rank` orders ties, or comes before the score with
 *  `rankFirst` (the recently used commands). */
function fuzzyFilter(items: QuickPickItem[], text: string, rank?: (i: QuickPickItem) => number, rankFirst = false) {
  const scored: { item: QuickPickItem; score: number; rank: number }[] = [];
  for (const item of items) {
    const m = fuzzyMatch(text, item.label);
    if (m) { scored.push({ item: { ...item, matches: m.indices }, score: m.score, rank: rank?.(item) ?? 0 }); continue; }
    // The description too (a table's connection, a command's category), below the label's.
    const d = item.description ? fuzzyMatch(text, `${item.label} ${item.description}`) : null;
    if (d) scored.push({ item: { ...item, matches: d.indices.filter((i) => i < item.label.length) }, score: d.score - 20, rank: rank?.(item) ?? 0 });
  }
  scored.sort((a, b) => (rankFirst ? a.rank - b.rank || b.score - a.score : b.score - a.score || a.rank - b.rank));
  return scored.map((s) => s.item);
}

// -- the command palette ---------------------------------------------------------------------

/** Open the command palette; `text`: already typed after the ">". */
export async function openCommandPalette(text = '') {
  const recent = recentIds();
  const order = new Map(recent.map((id, i) => [id, i]));
  const items: QuickPickItem[] = COMMANDS
    .filter((c) => c.id !== 'palette.open' && commandAvailable(c.id))
    .map((c) => ({
      id: c.id, label: `${t(`shortcuts:group.${c.group}`)}: ${t(c.label)}`,
      keybinding: bindingOf(c.id) ? bindingLabel(c.id) : undefined,
    }))
    .sort((a, b) => a.label.localeCompare(b.label));
  const recentGroup = t('palette:commands.recent');
  const otherGroup = t('palette:commands.other');
  const picked = await openQuickPick({
    placeholder: t('palette:commands.placeholder'),
    empty: t('palette:commands.empty'),
    value: `>${text}`,
    items: () => items,
    filter: (list, typed) => {
      const q = typed.replace(/^>\s*/, '');
      const rank = (i: QuickPickItem) => order.get(i.id) ?? MAX_RECENT;
      const out = q ? fuzzyFilter(list, q, rank, true) : [...list].sort((a, b) => rank(a) - rank(b));
      // "usado recientemente" over the recent ones, "otros comandos" over the rest.
      const firstOther = out.findIndex((i) => !order.has(i.id));
      return out.map((i, n) => (n === 0 && order.has(i.id) ? { ...i, group: recentGroup }
        : n === firstOther && firstOther > 0 ? { ...i, group: otherGroup } : i));
    },
    onText: (typed) => {
      if (typed.startsWith('>')) return false;
      void openGoTo(typed);
      return true;
    },
  });
  if (!picked) return;
  remember(picked.id);
  // After the quick pick gave the focus back (to the editor, for its commands).
  setTimeout(() => { runCommand(picked.id); }, 0);
}

// -- "Ir a…" ---------------------------------------------------------------------------------

interface GoItem extends QuickPickItem {
  /** Tabs first, then connections, databases, queries, objects (when scores tie). */
  rank: number;
  open(): void;
}

const where = (connectionId: string, database: string) => {
  const name = useConnectionsStore().byId(connectionId)?.name ?? '';
  return database ? `${name} › ${database}` : name;
};

function tabItem(tab: Tab): GoItem | null {
  if (tab.kind === 'connection') return null;
  const tabs = useTabsStore();
  const icon = tab.kind === 'object' ? `ei-${KIND_ICONS[tab.object.kind] ?? 'document'}` : tab.kind === 'query' || tab.kind === 'file' ? 'ei-document' : tab.kind === 'notebook' ? 'ei-notebook' : 'ei-files';
  return {
    id: `tab:${tab.id}`, rank: 0, icon, label: tabTitle(tab),
    description: [t('palette:goto.tab'), where(tab.connectionId, tab.database)].filter(Boolean).join(' · '),
    current: tab.id === tabs.activeId,
    open: () => tabs.activate(tab.id),
  };
}

function connectionItem(c: SavedConnection): GoItem {
  const conns = useConnectionsStore();
  const d = conns.driver(c.config.driver);
  const folder = c.folder_id ? conns.folderPath(c.folder_id) : '';
  return {
    id: `c:${c.id}`, rank: 1, icon: 'ei-connection', label: c.name,
    description: [t('palette:goto.connection'), d?.name ?? c.config.driver, folder].filter(Boolean).join(' · '),
    open: () => {
      // Engines without databases show their objects right under the connection.
      const single = d && !d.databases_label ? conns.live[c.id]?.databases[0] ?? '' : '';
      useUiStore().revealInExplorer({ connectionId: c.id, database: single });
    },
  };
}

function databaseItem(connectionId: string, database: string): GoItem {
  return {
    id: `d:${connectionId}:${database}`, rank: 2, icon: 'ei-coin', label: database,
    description: `${t('palette:goto.database')} · ${where(connectionId, '')}`,
    open: () => useUiStore().revealInExplorer({ connectionId, database }),
  };
}

function queryItem(q: SavedQuery): GoItem {
  return {
    id: `q:${q.id}`, rank: 3, icon: 'ei-document', label: q.name,
    description: `${t('palette:goto.query')} · ${where(q.connection_id, q.database)}`,
    open: () => {
      const conns = useConnectionsStore();
      // Its tab's title comes from the database's query list.
      void conns.loadQueries(q.connection_id, q.database);
      useTabsStore().openQuery(q, false);
    },
  };
}

function objectItem(connectionId: string, database: string, o: DbObject): GoItem {
  const conns = useConnectionsStore();
  const kind = conns.driverOf(connectionId)?.object_kinds.find((k) => k.id === o.kind);
  return {
    id: `o:${connectionId}:${database}:${o.kind}:${o.schema ?? ''}:${o.name}`, rank: 4,
    icon: `ei-${KIND_ICONS[o.kind] ?? 'document'}`,
    label: o.schema ? `${o.schema}.${o.name}` : o.name,
    description: [kind ? tb(kind.label) : o.kind, where(connectionId, database)].join(' · '),
    open: () => {
      // As a click in the explorer: the data, or the definition / structure of what holds no rows.
      const view = kind && !kind.browsable ? (kind.has_definition ? 'definition' : 'structure') : 'data';
      useTabsStore().openObject(connectionId, database, { kind: o.kind, schema: o.schema, name: o.name }, view, false);
    },
  };
}

/** Run `jobs` a few at a time. */
async function pool(jobs: (() => Promise<void>)[], size = 6) {
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(size, jobs.length) }, async () => {
    while (next < jobs.length) await jobs[next++]().catch(() => {});
  }));
}

/** Open "Ir a…"; `text`: already typed. */
export async function openGoTo(text = '') {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const state = reactive({ items: [] as GoItem[], busy: true });
  const seen = new Set<string>();
  const add = (list: (GoItem | null)[]) => {
    const fresh = list.filter((x): x is GoItem => !!x && !seen.has(x.id));
    for (const x of fresh) seen.add(x.id);
    if (fresh.length) state.items = [...state.items, ...fresh];
  };

  // What's in memory now: open tabs, connections, the databases and objects
  // the explorer loaded, the queries it listed.
  add(tabs.tabs.map(tabItem));
  add(conns.list.map(connectionItem));
  const databases = new Map<string, Set<string>>();
  const known = (cid: string, db: string) => (databases.get(cid) ?? databases.set(cid, new Set()).get(cid)!).add(db);
  for (const c of conns.list) {
    const d = conns.driverOf(c.id);
    for (const db of conns.live[c.id]?.databases ?? []) {
      known(c.id, db);
      if (d?.databases_label) add([databaseItem(c.id, db)]);
    }
  }
  for (const [k, loaded] of Object.entries(conns.objects)) {
    const [cid, db] = k.split('\u0000');
    if (!conns.byId(cid)) continue;
    known(cid, db);
    add(loaded.items.map((o) => objectItem(cid, db, o)));
  }
  for (const loaded of Object.values(conns.queries)) add(loaded.items.map(queryItem));

  // The explorer cache: databases and objects seen last time (even of
  // connections not open now), and the saved queries of each database.
  void (async () => {
    await pool(conns.list.map((c) => async () => {
      const cached = await api.getCached<unknown>(c.id, '', 'databases').catch(() => null);
      const d = conns.driverOf(c.id);
      for (const db of Array.isArray(cached) ? cached.filter((x): x is string => typeof x === 'string') : []) {
        known(c.id, db);
        if (d?.databases_label) add([databaseItem(c.id, db)]);
      }
    }));
    const pairs = [...databases].flatMap(([cid, dbs]) => [...dbs].map((db) => [cid, db] as const));
    await pool(pairs.map(([cid, db]) => async () => {
      const [objects, queries] = await Promise.all([
        conns.objects[dbKey(cid, db)] ? null : api.getCached<DbObject[]>(cid, db, 'objects').catch(() => null),
        conns.queries[dbKey(cid, db)] ? null : api.listQueries(cid, db).catch(() => null),
      ]);
      if (Array.isArray(objects)) add(objects.map((o) => objectItem(cid, db, o)));
      if (Array.isArray(queries)) add(queries.map(queryItem));
    }));
    state.busy = false;
  })();

  const picked = await openQuickPick({
    placeholder: t('palette:goto.placeholder'),
    empty: t('palette:goto.empty'),
    value: text,
    items: () => state.items,
    busy: () => state.busy,
    filter: (list, typed) => {
      const q = typed.trim();
      // Nothing typed: the open tabs and the connections.
      if (!q) {
        const shown = list.filter((i) => (i as GoItem).rank <= 1);
        const firstConn = shown.findIndex((i) => (i as GoItem).rank === 1);
        return shown.map((i, n) => (n === 0 && (i as GoItem).rank === 0 ? { ...i, group: t('palette:goto.tabs') }
          : n === firstConn ? { ...i, group: t('palette:goto.connections') } : i));
      }
      return fuzzyFilter(list, q, (i) => (i as GoItem).rank).slice(0, MAX_GOTO);
    },
    onText: (typed) => {
      if (!typed.startsWith('>')) return false;
      void openCommandPalette(typed.slice(1));
      return true;
    },
  });
  if (!picked) return;
  const item = state.items.find((x) => x.id === picked.id);
  setTimeout(() => item?.open(), 0);
}

// -- the context of a command: a connection, a database --------------------------------------

export interface DbTarget { connectionId: string; database: string }

/** The active tab's saved connection and database, if it has one. */
export function activeTarget(): DbTarget | null {
  const conns = useConnectionsStore();
  const tab = useTabsStore().active;
  if (!tab || tab.kind === 'connection' || !tab.connectionId || !conns.byId(tab.connectionId)) return null;
  return { connectionId: tab.connectionId, database: tab.database ?? '' };
}

/** Ask which connection (among those `accept` takes); one alone is taken. */
export async function pickConnection(accept: (c: SavedConnection) => boolean): Promise<string | null> {
  const conns = useConnectionsStore();
  const list = conns.list.filter(accept);
  if (!list.length) { ElMessage.info(t('palette:pick.noConnection')); return null; }
  if (list.length === 1) return list[0].id;
  const active = activeTarget()?.connectionId;
  const picked = await openQuickPick({
    placeholder: t('palette:pick.connection'),
    items: () => list.map((c) => {
      const d = conns.driver(c.config.driver);
      const folder = c.folder_id ? conns.folderPath(c.folder_id) : '';
      return { id: c.id, label: c.name, icon: 'ei-connection', current: c.id === active, description: [d?.name ?? c.config.driver, folder].filter(Boolean).join(' · ') };
    }),
    filter: (items, typed) => (typed.trim() ? fuzzyFilter(items, typed) : items),
  });
  return picked?.id ?? null;
}

/** Ask which database of the connection (connecting to list them). Engines
 *  without databases have just the one. */
export async function pickDatabase(connectionId: string): Promise<string | null> {
  const conns = useConnectionsStore();
  const d = conns.driverOf(connectionId);
  if (!d?.databases_label) {
    if (!(await conns.ensureConnected(connectionId))) return null;
    return conns.live[connectionId]?.databases[0] ?? '';
  }
  void conns.ensureConnected(connectionId);
  const picked = await openQuickPick({
    placeholder: t('palette:pick.database', { connection: conns.byId(connectionId)?.name ?? '' }),
    items: () => {
      const live = conns.live[connectionId];
      return (live?.status === 'connected' ? live.databases : []).map((db) => ({ id: db, label: db, icon: 'ei-coin', current: db === live?.defaultDatabase }));
    },
    busy: () => conns.live[connectionId]?.status !== 'connected' && conns.live[connectionId]?.status !== 'error',
    empty: conns.live[connectionId]?.error ?? t('palette:pick.noDatabases'),
    filter: (items, typed) => (typed.trim() ? fuzzyFilter(items, typed) : items),
  });
  return picked ? picked.id : null;
}

/** The connection a command runs on: the active tab's when `accept` takes
 *  it, else the one the user picks. */
export async function resolveConnection(accept: (c: SavedConnection) => boolean): Promise<string | null> {
  const conns = useConnectionsStore();
  const a = activeTarget();
  const c = a ? conns.byId(a.connectionId) : undefined;
  if (c && accept(c)) return c.id;
  return pickConnection(accept);
}

/** The database a command runs on: the active tab's (asking only for the
 *  database when the tab has none, as the monitor's), or one picked. */
export async function resolveDatabase(accept: (c: SavedConnection) => boolean): Promise<DbTarget | null> {
  const conns = useConnectionsStore();
  const a = activeTarget();
  const c = a ? conns.byId(a.connectionId) : undefined;
  const connectionId = c && accept(c) ? c.id : await pickConnection(accept);
  if (!connectionId) return null;
  if (a && connectionId === a.connectionId && (a.database || !conns.driverOf(connectionId)?.databases_label)) return a;
  const database = await pickDatabase(connectionId);
  return database === null ? null : { connectionId, database };
}
