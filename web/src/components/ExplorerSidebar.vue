<script setup lang="ts">
import ImportConnectionsDialog from './ImportConnectionsDialog.vue';
import ImportSuggestion from './ImportSuggestion.vue';
import SupportReminder from './SupportReminder.vue';
import TelemetryConsent from './TelemetryConsent.vue';
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import type { ConnectionFolder, DbObject, DriverInfo, KeyEntry, KeySearch, Permissions, SavedConnection, SavedQuery } from '../api/types';
import { dbKey, objKey, useConnectionsStore, type KeyBrowse } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { useSettingsStore } from '../stores/settings';
import { readJson, writeJson } from '../stores/storage';
import { confirmNative } from '../native';
import { createDatabase, deleteQuery, dropDatabase, dropObjects, duplicateQuery, newFromTemplate, newQuery, renameQuery } from '../composables/actions';
import {
  deleteSavedMigration, duplicateSavedMigration, newMigration, openSavedMigration, renameSavedMigration, savedMigrationState, type SavedMigrationState,
} from '../composables/savedMigrations';
import type { SavedMigration } from '../api/savedMigrations';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';
import KeySearchRow from './KeySearchRow.vue';
import CloneTableDialog from './CloneTableDialog.vue';
import { tagColor } from '../composables/tags';

// The explorer: user folders (clients, environments… nested at will) →
// connections → databases → Queries + the kinds of objects the driver
// declares → objects → columns. The tree is virtualized and built from the
// stores; expanding a node loads what's below it. Engines with a single
// namespace skip the database level. Connections and folders move by drag
// and drop or with "Mover a…".

type NodeType = 'group' | 'connection' | 'database' | 'schema' | 'folder' | 'queries' | 'object' | 'column' | 'query' | 'status'
  // "Migraciones" and its saved migrations.
  | 'migrations' | 'migration'
  // A key database's search row, namespace folders and "Cargar más".
  | 'keysearch' | 'keyns' | 'keymore';

interface TNode {
  id: string;
  label: string;
  type: NodeType;
  children?: TNode[];
  connectionId?: string;
  database?: string;
  kindId?: string;
  object?: DbObject;
  query?: SavedQuery;
  migration?: SavedMigration;
  /** A saved migration's state (its status icon). */
  state?: SavedMigrationState;
  hint?: string;
  icon?: string;
  count?: number;
  status?: 'loading' | 'error' | 'empty';
  pk?: boolean;
  folder?: ConnectionFolder;
  color?: string | null;
  /** A key found by a key search: its type and time to live. */
  key?: KeyEntry;
  /** A namespace folder's prefix (`user:1:`). */
  prefix?: string;
  /** The schema a schema node, or a kind folder inside one, stands for. */
  schema?: string;
  /** Flat mode: an object's non-default schema, shown dim before its name. */
  qualifier?: string;
}

const conns = useConnectionsStore();
const tabs = useTabsStore();
const ui = useUiStore();
const { t } = useTranslation();

const KIND_ICONS: Record<string, string> = {
  table: 'grid', view: 'view', materialized_view: 'copy-document', procedure: 'operation',
  function: 'cpu', trigger: 'lightning', sequence: 'sort', collection: 'files', key: 'key',
  index: 'collection', topic: 'chat-line-square', stream: 'share', measurement: 'trend-charts',
  package: 'box', alias: 'link', type: 'price-tag', device: 'monitor', dictionary: 'notebook',
  constraint: 'lock', supertable: 'menu', subtable: 'grid', task: 'timer', file: 'document',
  label: 'coordinate', vertex: 'coordinate', relationship: 'connection', edge: 'connection',
  source: 'upload', sink: 'download', synonym: 'link', domain: 'price-tag', virtual_table: 'grid',
  fulltext_catalog: 'reading', fulltext_stoplist: 'document-remove',
};
const DEFAULT_SCHEMAS = new Set(['dbo', 'public', 'main']);

function status(parent: string, s: 'loading' | 'error' | 'empty', label: string): TNode {
  return { id: `x:${parent}`, label, type: 'status', status: s };
}

function databaseChildren(c: SavedConnection, d: DriverInfo, db: string, parentId: string): TNode[] {
  const k = dbKey(c.id, db);
  const objs = conns.objects[k];
  const queries = conns.queries[k]?.items ?? [];
  const qid = `qs:${c.id}:${db}`;
  const out: TNode[] = [{
    id: qid, label: t('explorer:tree.queries'), type: 'queries', connectionId: c.id, database: db, count: queries.length,
    children: queries.length
      ? queries.map((q) => ({ id: `q:${q.id}`, label: q.name, type: 'query' as const, connectionId: c.id, database: db, query: q }))
      : [status(qid, 'empty', t('explorer:tree.noQueries'))],
  }];
  // Migrations started from this database (never listed under the target).
  const migrations = conns.migrations[k]?.items ?? [];
  const mid = `ms:${c.id}:${db}`;
  out.push({
    id: mid, label: t('explorer:tree.migrations'), type: 'migrations', connectionId: c.id, database: db, count: migrations.length,
    children: migrations.length
      ? migrations.map((m) => ({
        id: `m:${m.id}`, label: m.name, type: 'migration' as const, connectionId: c.id, database: db, migration: m,
        state: savedMigrationState(m, conns.migrationRuns),
      }))
      : [status(mid, 'empty', t('explorer:tree.noMigrations'))],
  });
  if (!objs || objs.status === 'loading') return [...out, status(parentId, 'loading', t('common:loading'))];
  if (objs.status === 'error') return [...out, status(parentId, 'error', objs.error ?? t('common:error'))];
  if (d.key_search) {
    const kind = d.object_kinds[0];
    const fid = `f:${c.id}:${db}:${kind?.id ?? 'key'}`;
    const b = conns.keys[k];
    out.push({
      id: fid, label: kind ? tb(kind.label) : t('explorer:tree.keys'), type: 'folder', connectionId: c.id, database: db, kindId: kind?.id ?? 'key',
      count: b?.items.length, hint: b?.total != null && b.total !== b.items.length ? t('explorer:tree.ofTotal', { total: num(b.total) }) : undefined,
      children: keyChildren(c, db, d.key_search, b, fid, kind?.has_columns ?? false),
    });
    return out;
  }
  const { schemas, rank } = schemasOf(c, d, objs.items);
  if (schemas && groupBySchema.value) {
    // One node per schema, its kind folders inside; objects without a
    // schema stay at the database level, after them.
    const bySchema = new Map<string, DbObject[]>(schemas.map((s) => [s, []]));
    const loose: DbObject[] = [];
    for (const o of objs.items) (o.schema != null ? bySchema.get(o.schema)! : loose).push(o);
    for (const [s, items] of bySchema) {
      const sid = `s:${c.id}:${db}:${s}`;
      out.push({
        id: sid, label: s, type: 'schema', connectionId: c.id, database: db, schema: s, count: items.length,
        children: kindFolders(c, d, db, items, s, 'none', rank),
      });
    }
    out.push(...kindFolders(c, d, db, loose, undefined, 'hint', rank));
    return out;
  }
  // Flat: with several schemas, objects sorted by schema and qualified.
  // Ranks and positions are looked up, not recomputed, per comparison.
  let items = objs.items;
  if (schemas) {
    const pos = new Map(schemas.map((s, i) => [s, i]));
    const at = (o: DbObject) => (o.schema == null ? schemas.length : pos.get(o.schema)!);
    items = [...items].sort((a, b) => at(a) - at(b) || byName(a.name, b.name));
  }
  out.push(...kindFolders(c, d, db, items, undefined, schemas ? 'qualify' : 'hint', rank));
  return out;
}

/** The kind folders (Tablas, Vistas…) of a list of objects: only kinds with
 *  items, then "Otros" for kinds the driver returned but didn't declare.
 *  Inside a schema node their ids carry the schema. */
function kindFolders(c: SavedConnection, d: DriverInfo, db: string, objs: DbObject[], schema: string | undefined, show: SchemaShow, rank: SchemaRank): TNode[] {
  const base = schema === undefined ? `f:${c.id}:${db}` : `f:${c.id}:${db}:${schema}`;
  const out: TNode[] = [];
  for (const kind of d.object_kinds) {
    const items = objs.filter((o) => o.kind === kind.id);
    if (!items.length) continue;
    out.push({
      id: `${base}:${kind.id}`, label: tb(kind.label), type: 'folder', connectionId: c.id, database: db, kindId: kind.id, schema, count: items.length,
      children: items.map((o) => objectNode(c, db, o, kind.has_columns, show, rank)),
    });
  }
  const declared = new Set(d.object_kinds.map((k) => k.id));
  const other = objs.filter((o) => !declared.has(o.kind));
  if (other.length) {
    out.push({
      id: `${base}:__other`, label: t('explorer:tree.others'), type: 'folder', connectionId: c.id, database: db, schema, count: other.length,
      children: other.map((o) => objectNode(c, db, o, false, show, rank)),
    });
  }
  return out;
}

// -- schemas -------------------------------------------------------------------------------
// With "Agrupar por esquema" on, a database whose objects span several
// schemas gets a level per schema (as in SSMS); off, one list with the
// schema before each name. A single schema (or none) needs neither.
const settings = useSettingsStore();
const groupBySchema = computed(() => settings.get<boolean>('explorer.groupBySchema', true));
/** Schemas the engines keep for themselves: listed last. */
const SYSTEM_SCHEMAS = new Set([
  'sys', 'information_schema', 'pg_catalog', 'pg_toast', 'guest',
  'sysibm', 'syscat', 'sysstat', 'sysfun', 'sysproc', 'systools', 'system',
]);
/** How an object shows its schema: as a hint (today's single-schema look),
 *  not at all (inside its schema node), or before its name (flat mode). */
type SchemaShow = 'hint' | 'none' | 'qualify';
/** Hoisted: localeCompare with options builds a collator on every call. */
const byName = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' }).compare;
/** A database's schemas → 0 default, 1 user, 2 system. Built once per
 *  tree build (driverOf walks reactive arrays: too slow per object). */
type SchemaRank = Map<string, number>;

/** The schemas the objects span, in tree order (the default one first,
 *  then by name, the system ones last) or null when there aren't several
 *  (the tree then looks as it always did), and each one's rank. Defaults
 *  match case-insensitively (PUBLIC in Snowflake/H2); engines without a
 *  fixed default (Oracle, DB2) default to the user's own schema. */
function schemasOf(c: SavedConnection, d: DriverInfo, objs: DbObject[]): { schemas: string[] | null; rank: SchemaRank } {
  const rank: SchemaRank = new Map();
  let defaults: Set<string> | null = null;
  for (const o of objs) {
    if (o.schema == null || rank.has(o.schema)) continue;
    if (!defaults) {
      defaults = new Set(DEFAULT_SCHEMAS);
      const own = d.has_schemas ? dialectSchema(d) ?? c.config.username : null;
      if (own) defaults.add(own.toLowerCase());
    }
    const l = o.schema.toLowerCase();
    rank.set(o.schema, defaults.has(l) ? 0 : SYSTEM_SCHEMAS.has(l) ? 2 : 1);
  }
  const schemas = rank.size > 1 ? [...rank.keys()].sort((a, b) => rank.get(a)! - rank.get(b)! || byName(a, b)) : null;
  return { schemas, rank };
}
/** The schema nodes a loaded database shows right now (none: null). */
function schemaLevel(connectionId: string, db: string): string[] | null {
  const c = conns.byId(connectionId);
  const d = conns.driverOf(connectionId);
  const objs = conns.objects[dbKey(connectionId, db)];
  if (!groupBySchema.value || !c || !d || d.key_search || !objs?.items) return null;
  return schemasOf(c, d, objs.items).schemas;
}

// -- key databases (Redis, etcd) -----------------------------------------------------------
// Their keys aren't listed: the folder holds a search row, the keys found so
// far nested by the engine's separator, and "Cargar más" while the search
// has more to give.
const num = (n: number) => n.toLocaleString(locale());

function keyChildren(c: SavedConnection, db: string, ks: KeySearch, b: KeyBrowse | undefined, fid: string, hasColumns: boolean): TNode[] {
  const rows: TNode[] = [{ id: `ks:${fid}`, label: b?.pattern ?? '', type: 'keysearch', connectionId: c.id, database: db }];
  if (!b) return [...rows, status(fid, 'loading', t('explorer:tree.searching'))];
  rows.push(...groupKeys(c, db, b.items, ks.separator, '', hasColumns));
  const searching = !!(b.pattern || b.keyType);
  if (b.status === 'loading') {
    rows.push(status(`${fid}:tail`, 'loading', b.scanned && searching ? t('explorer:tree.searchingScanned', { scanned: num(b.scanned) }) : t('explorer:tree.searching')));
  } else if (b.status === 'error') {
    rows.push(status(`${fid}:tail`, 'error', b.error ?? t('common:error')));
  } else if (b.cursor) {
    rows.push({ id: `km:${fid}`, label: t('explorer:tree.loadMore'), type: 'keymore', connectionId: c.id, database: db, hint: progress(b) });
  } else if (!b.items.length) {
    rows.push(status(`${fid}:tail`, 'empty', searching ? t('explorer:tree.noKeyMatch') : t('explorer:tree.noKeys')));
  }
  return rows;
}

/** "500 de 1.284.330", or how far a selective search got. */
function progress(b: KeyBrowse): string {
  if (b.pattern || b.keyType) {
    const vars = { found: num(b.items.length), scanned: num(b.scanned), total: b.total != null ? num(b.total) : '' };
    return b.total != null ? t('explorer:tree.progressSearchOf', vars) : t('explorer:tree.progressSearch', vars);
  }
  return b.total != null
    ? t('explorer:tree.progressOf', { loaded: num(b.items.length), total: num(b.total) })
    : t('explorer:tree.progressLoaded', { loaded: num(b.items.length) });
}

/** Keys nested by the separator: `user:1:cart` under user › user:1. A
 *  prefix with a single key shows the key itself, not a folder. */
function groupKeys(c: SavedConnection, db: string, items: KeyEntry[], sep: string, prefix: string, hasColumns: boolean, depth = 0): TNode[] {
  const groups = new Map<string, KeyEntry[]>();
  const leaves: KeyEntry[] = [];
  for (const it of items) {
    const i = sep && depth < 16 ? it.name.indexOf(sep, prefix.length) : -1;
    if (i < 0) { leaves.push(it); continue; }
    const seg = it.name.slice(prefix.length, i);
    const list = groups.get(seg);
    if (list) list.push(it);
    else groups.set(seg, [it]);
  }
  const folders: TNode[] = [];
  for (const seg of [...groups.keys()].sort(byName)) {
    const list = groups.get(seg)!;
    if (list.length === 1) { leaves.push(list[0]); continue; }
    const p = `${prefix}${seg}${sep}`;
    folders.push({
      id: `kn:${c.id}:${db}:${p}`, label: seg || sep, type: 'keyns', connectionId: c.id, database: db, prefix: p, count: list.length,
      children: groupKeys(c, db, list, sep, p, hasColumns, depth + 1),
    });
  }
  leaves.sort((a, b) => byName(a.name, b.name));
  return [...folders, ...leaves.map((e) => keyNode(c, db, e, hasColumns))];
}

function keyNode(c: SavedConnection, db: string, e: KeyEntry, hasColumns: boolean): TNode {
  const node = objectNode(c, db, { kind: 'key', schema: null, name: e.name, parent: null }, hasColumns);
  node.key = e;
  node.hint = e.ttl_ms != null ? ttl(e.ttl_ms) : undefined;
  return node;
}

/** Time to live, short: 45 s, 12 min, 3 h, 5 d. */
function ttl(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `TTL ${s} s`;
  if (s < 3600) return `TTL ${Math.round(s / 60)} min`;
  if (s < 86400) return `TTL ${Math.round(s / 3600)} h`;
  return `TTL ${Math.round(s / 86400)} d`;
}

const TYPE_TAGS: Record<string, string> = { string: 'STR', hash: 'HASH', list: 'LIST', set: 'SET', zset: 'ZSET', stream: 'STRM', json: 'JSON' };
const typeTag = (t: string) => TYPE_TAGS[t] ?? t.slice(0, 4).toUpperCase();

/** The search that shows only a namespace folder's keys. */
function searchIn(n: TNode) {
  const ks = conns.driverOf(n.connectionId!)?.key_search;
  if (!ks || !n.prefix) return;
  let pattern = n.prefix;
  if (ks.syntax === 'glob') {
    pattern = `${n.prefix.replace(/[*?[\]\\]/g, '\\$&')}*`;
  } else {
    // etcd searches below the connection's own prefix.
    const own = conns.byId(n.connectionId!)?.config.options?.prefix ?? '';
    if (own && pattern.startsWith(own)) pattern = pattern.slice(own.length);
  }
  const b = conns.keys[dbKey(n.connectionId!, n.database ?? '')];
  conns.searchKeys(n.connectionId!, n.database ?? '', pattern, b?.keyType ?? '');
}

function objectNode(c: SavedConnection, db: string, o: DbObject, hasColumns: boolean, show: SchemaShow = 'hint', rank?: SchemaRank): TNode {
  const id = `o:${c.id}:${db}:${o.kind}:${o.schema ?? ''}:${o.name}`;
  const own = show !== 'none' && o.schema && rank?.get(o.schema) !== 0 ? o.schema : undefined;
  const node: TNode = {
    id, label: o.name, type: 'object', connectionId: c.id, database: db, object: o, kindId: o.kind,
    hint: o.parent ?? (show === 'hint' ? own : undefined),
    qualifier: show === 'qualify' ? own : undefined,
    icon: KIND_ICONS[o.kind] ?? 'document',
  };
  if (hasColumns) {
    const cols = conns.columns[objKey(c.id, db, o.schema, o.name)];
    node.children = !cols || cols.status === 'loading'
      ? [status(id, 'loading', t('common:loading'))]
      : cols.status === 'error'
        ? [status(id, 'error', cols.error ?? t('common:error'))]
        : cols.items.length
          ? cols.items.map((col) => ({
            id: `col:${id}:${col.name}`, label: col.name, type: 'column' as const, hint: col.data_type, pk: col.primary_key,
          }))
          : [status(id, 'empty', t('explorer:tree.noColumns'))];
  }
  return node;
}

function connectionNode(c: SavedConnection): TNode {
    const id = `c:${c.id}`;
    const d = conns.driver(c.config.driver);
    const node: TNode = { id, label: c.name, type: 'connection', connectionId: c.id, hint: d?.name ?? c.config.driver };
    const live = conns.live[c.id];
    if (!d) node.children = [status(id, 'error', t('explorer:tree.driverUnavailable', { driver: c.config.driver }))];
    else if (!live || (live.status === 'connecting' && !live.fromCache)) {
      const label = live ? (conns.downloadNote(d.id) ?? t('explorer:tree.connecting')) : t('explorer:tree.expandToConnect');
      node.children = [status(id, 'loading', label)];
    }
    else if (live.status === 'error') node.children = [status(id, 'error', live.error ?? t('common:error'))];
    else if (!d.databases_label) node.children = databaseChildren(c, d, live.databases[0] ?? '', id);
    else {
      node.children = live.databases.map((db) => {
        const did = `d:${c.id}:${db}`;
        const loaded = conns.objects[dbKey(c.id, db)];
        return {
          id: did, label: db, type: 'database' as const, connectionId: c.id, database: db,
          hint: loaded?.status === 'stale' ? t('explorer:tree.refreshing') : db === live.defaultDatabase ? t('explorer:tree.defaultDatabase') : undefined,
          children: loaded ? databaseChildren(c, d, db, did) : [status(did, 'loading', t('common:loading'))],
        };
      });
      if (!node.children.length) node.children = [status(id, 'empty', t('explorer:tree.noDatabases'))];
    }
    return node;
}

/** Folders under `parent` (then their connections), recursively. */
function folderLevel(parent: string | null): TNode[] {
  return conns.folders
    .filter((f) => (f.parent_id ?? null) === parent)
    .map((f) => {
      const id = `g:${f.id}`;
      const children = [...folderLevel(f.id), ...conns.list.filter((c) => c.folder_id === f.id).map(connectionNode)];
      return {
        id, label: f.name, type: 'group' as const, folder: f, color: f.color,
        count: countConnections(f.id),
        children: children.length ? children : [status(id, 'empty', t('explorer:tree.emptyFolder'))],
      };
    });
}
function countConnections(folderId: string): number {
  return conns.list.filter((c) => c.folder_id === folderId).length +
    conns.folders.filter((f) => f.parent_id === folderId).reduce((n, f) => n + countConnections(f.id), 0);
}

const data = computed<TNode[]>(() => {
  const known = new Set(conns.folders.map((f) => f.id));
  return [
    ...folderLevel(null),
    // Top level, plus any whose folder no longer exists.
    ...conns.list.filter((c) => !c.folder_id || !known.has(c.folder_id)).map(connectionNode),
  ];
});

// -- expansion ---------------------------------------------------------------------------
// el-tree-v2 falls back to `default-expanded-keys` whenever `data` changes
// (every load, every save), so the expanded set lives here.
// Open folders are remembered across launches (connections aren't: opening
// one connects).
const expanded = ref<string[]>(readJson<string[]>('dbine.openFolders', []));
watch(expanded, (keys) => writeJson('dbine.openFolders', keys.filter((k) => k.startsWith('g:'))));

function onCollapse(n: TNode) {
  // Drop the node and everything below it, or re-expanding a child would
  // reopen its ancestors on the next data change.
  const gone = new Set<string>();
  const walk = (x: TNode) => { gone.add(x.id); x.children?.forEach(walk); };
  walk(n);
  expanded.value = expanded.value.filter((k) => !gone.has(k));
}

// -- loading on expand ----------------------------------------------------------------
async function onExpand(n: TNode) {
  if (!expanded.value.includes(n.id)) expanded.value = [...expanded.value, n.id];
  if (n.type === 'connection' && n.connectionId) {
    const id = n.connectionId;
    const d = conns.driverOf(id);
    const connecting = conns.ensureConnected(id);
    // Engines with a single namespace: its objects from the explorer cache
    // right away (they refresh once connected).
    await conns.cachedDatabasesReady(id);
    const cachedDb = conns.live[id]?.fromCache ? conns.live[id]?.databases[0] : undefined;
    if (d && !d.databases_label && cachedDb !== undefined) loadDatabase(id, cachedDb);
    if (!(await connecting)) return;
    if (d && !d.databases_label) loadDatabase(id, conns.live[id]?.databases[0] ?? '');
  } else if (n.type === 'database' && n.connectionId) {
    loadDatabase(n.connectionId, n.database ?? '');
  } else if (n.type === 'object' && n.object && n.connectionId) {
    conns.loadColumns(n.connectionId, n.database ?? '', n.object);
  }
}
function loadDatabase(connectionId: string, database: string, force = false) {
  conns.loadObjects(connectionId, database, force);
  conns.loadQueries(connectionId, database, force);
  conns.loadMigrations(connectionId, database, force);
  conns.loadPermissions(connectionId, database);
}

/** The item, disabled with the missing privilege as its tooltip when the
 *  login lacks it (the server would refuse the action anyway). */
function guarded(cid: string, db: string, action: keyof Permissions, item: MenuItem): MenuItem {
  const missing = conns.denied(cid, db, action);
  return missing ? { ...item, disabled: true, hint: t('common:noPermission', { missing }) } : item;
}

// -- clicks --------------------------------------------------------------------------------
function open(n: TNode, preview: boolean) {
  if (n.type === 'query' && n.query) tabs.openQuery(n.query, preview);
  if (n.type === 'migration' && n.migration) openSavedMigration(n.migration);
  if (n.type === 'object' && n.object && n.connectionId) {
    const kind = conns.driverOf(n.connectionId)?.object_kinds.find((k) => k.id === n.object!.kind);
    const view = kind && !kind.browsable ? (kind.has_definition ? 'definition' : 'structure') : 'data';
    tabs.openObject(n.connectionId, n.database ?? '', { kind: n.object.kind, schema: n.object.schema, name: n.object.name }, view, preview);
  }
}

// -- context menu -------------------------------------------------------------------------
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
/** "Clonar…": the table being cloned. */
const cloning = ref<{ connectionId: string; database: string; object: { kind: string; schema: string | null; name: string } } | null>(null);
/** Kinds whose objects hold no rows of their own, and engines without tables to clone (see `check_cloneable` in Rust). */
const NOT_CLONEABLE = new Set(['view', 'materialized_view', 'stream', 'topic', 'virtual_table', 'alias', 'dictionary', 'source', 'file']);
/** `driver:kind` pairs one engine can't clone faithfully (`NOT_CLONEABLE_ON` in `clone_table/timeseries.rs`). */
const NOT_CLONEABLE_ON = new Set(['tdengine:supertable', 'tdengine:subtable', 'influxdb:measurement', 'influxdb3:measurement']);
function cloneable(d: DriverInfo | undefined, kind: { has_columns: boolean; browsable: boolean } | undefined, kindId: string): boolean {
  if (!d || !kind?.has_columns || !kind.browsable || NOT_CLONEABLE.has(kindId) || NOT_CLONEABLE_ON.has(`${d.id}:${kindId}`)) return false;
  // CouchDB: its documents are the whole database, not a table in it.
  return !['graph', 'key_value', 'streaming'].includes(d.family) && d.id !== 'couchdb';
}

function copy(text: string) {
  navigator.clipboard.writeText(text).then(() => ElMessage.success({ message: t('common:copied'), duration: 1200 }));
}

async function removeConnection(c: SavedConnection) {
  const ok = await confirmNative(
    t('explorer:confirm.deleteConnection', { name: c.name }),
    { title: t('explorer:confirm.deleteConnectionTitle'), okLabel: t('common:delete') },
  );
  if (!ok) return;
  await conns.remove(c.id);
  tabs.closeWhere((t) => t.connectionId === c.id);
}

/** "host:port" (or the file) of a connection, for "Copiar servidor". */
function serverText(c: SavedConnection): string {
  const d = conns.driver(c.config.driver);
  const port = c.config.port || d?.default_port || 0;
  return port ? `${c.config.host}:${port}` : c.config.host;
}

/** The schema new objects go to by default. */
function defaultSchema(connectionId: string): string | null {
  const d = conns.driverOf(connectionId);
  return d?.has_schemas ? dialectSchema(d) : null;
}
/** The fixed default schema of a dialect, if it has one. */
function dialectSchema(d: DriverInfo): string | null {
  return d.dialect === 'mssql' ? 'dbo' : d.dialect === 'postgres' ? 'public' : null;
}

/** Database-level actions (the database node, or the connection of engines
 *  with a single namespace). */
function dbItems(cid: string, db: string, items: MenuItem[]) {
  const d = conns.driverOf(cid);
  if (!d) return;
  if (d.designer) items.push({ label: `${tb(d.designer.label)}…`, action: () => tabs.openDesigner(cid, db, null) });
  for (const tpl of d.create_templates) items.push({ label: tb(tpl.label), action: () => newFromTemplate(cid, db, tpl, defaultSchema(cid)) });
  items.push({ label: t('explorer:menu.databaseDiagram'), divided: true, action: () => tabs.openDiagram(cid, db) });
  if (d.supports_profiler) items.push(guarded(cid, db, 'profiler', { label: t('explorer:menu.profiler'), action: () => tabs.openProfiler(cid, db) }));
  items.push({ label: t('explorer:menu.generateScript'), action: () => ui.openDbDialog('script', cid, db) });
  items.push({ label: t('explorer:menu.migrate'), action: () => newMigration(cid, db) });
  items.push({ label: t('explorer:menu.compareSchemas'), action: () => tabs.openCompare(cid, db) });
  items.push({ label: t('dataCompare:menu'), action: () => tabs.openDataCompare(cid, db, null) });
  if (d.security) items.push({ label: t('security:menu'), action: () => tabs.openSecurity(cid, db) });
  items.push({ label: t('backups:menu'), action: () => tabs.openBackups(cid, db) });
  items.push({ label: t('explorer:menu.exportDatabase'), action: () => ui.openDbDialog('export', cid, db) });
  items.push({ label: t('explorer:menu.importData'), divided: true, action: () => ui.openDbDialog('import', cid, db) });
  items.push({ label: t('explorer:menu.runScriptFile'), action: () => ui.openDbDialog('run', cid, db) });
}

async function removeFolder(f: ConnectionFolder) {
  const n = countConnections(f.id);
  const ok = await confirmNative(
    t('explorer:confirm.deleteFolder', {
      name: f.name,
      detail: n ? t('explorer:confirm.deleteFolderMoves', { n }) : t('explorer:confirm.deleteFolderEmpty'),
    }),
    { title: t('explorer:confirm.deleteFolderTitle'), okLabel: t('common:delete') },
  );
  if (ok) await conns.deleteFolder(f.id);
}

function disconnectAll(folderId: string) {
  const inside = (fid: string | null): boolean => {
    for (let f = conns.folders.find((x) => x.id === fid), g = 0; f && g < 50; f = conns.folders.find((x) => x.id === f!.parent_id), g++) {
      if (f.id === folderId) return true;
    }
    return false;
  };
  for (const c of conns.list) if (inside(c.folder_id) && conns.live[c.id]) conns.disconnect(c.id);
}

/** How long a right click waits for the permission check before showing
 *  the menu (it's usually cached by then: databases check when they load). */
const PERMISSIONS_WAIT_MS = 400;

async function onContext(e: MouseEvent, n: TNode) {
  e.preventDefault();
  if (n.connectionId && (n.type === 'connection' || n.type === 'database')) {
    const d = conns.driverOf(n.connectionId);
    const dbs = n.type === 'database' ? [n.database ?? ''] : ['', ...(d && !d.databases_label ? [conns.live[n.connectionId]?.databases[0] ?? ''] : [])];
    const checks = Promise.all(dbs.map((db) => conns.loadPermissions(n.connectionId!, db)));
    await Promise.race([checks, new Promise((r) => setTimeout(r, PERMISSIONS_WAIT_MS))]);
  }
  // Several objects picked, and this is one of them: act on all of them.
  if (n.type === 'object' && picked.value.size > 1 && picked.value.has(n.id)) {
    const nodes = pickedNodes(n);
    const refs = nodes.map((x) => ({ kind: x.object!.kind, schema: x.object!.schema, name: x.object!.name }));
    menu.value = {
      x: e.clientX, y: e.clientY,
      items: [
        { label: t('explorer:menu.selectedObjects', { n: refs.length }), header: true },
        { label: t('explorer:menu.copyNames'), action: () => copy(refs.map((r) => (r.schema ? `${r.schema}.${r.name}` : r.name)).join('\n')) },
        { label: t('explorer:menu.deleteObjects', { n: refs.length }), danger: true, divided: true, action: () => { dropObjects(n.connectionId!, n.database ?? '', refs); picked.value = new Set(); } },
        { label: t('explorer:menu.clearSelection'), action: () => { picked.value = new Set(); } },
      ],
    };
    return;
  }
  // Several databases picked, and this is one of them.
  if (n.type === 'database' && picked.value.size > 1 && picked.value.has(n.id)) {
    const names = pickedDatabases().map((x) => x.database ?? x.label);
    menu.value = {
      x: e.clientX, y: e.clientY,
      items: [
        { label: t('explorer:menu.selectedDatabases', { n: names.length }), header: true },
        { label: t('explorer:menu.copyNames'), action: () => copy(names.join('\n')) },
        { label: t('explorer:menu.copyNamesComma'), action: () => copy(names.join(', ')) },
        { label: t('explorer:menu.clearSelection'), divided: true, action: () => { picked.value = new Set(); } },
      ],
    };
    return;
  }
  const items: MenuItem[] = [];
  const cid = n.connectionId;
  const db = n.database ?? '';
  switch (n.type) {
    case 'connection': {
      const c = conns.byId(cid!)!;
      const connected = conns.isConnected(cid!);
      const d = conns.driverOf(cid!);
      if (connected) {
        if (d && !d.databases_label) items.push({ label: t('explorer:menu.newQuery'), action: () => newQuery(cid!, conns.live[cid!]?.databases[0] ?? '') });
        items.push({ label: t('common:refresh'), action: () => conns.connect(cid!) });
        items.push({ label: t('common:disconnect'), action: () => conns.disconnect(cid!) });
      } else {
        items.push({ label: t('common:connect'), action: () => conns.connect(cid!) });
      }
      if (d?.capabilities.monitor) items.push({ label: t('explorer:menu.monitor'), action: () => tabs.openMonitor(cid!) });
      // Backups of the whole server (Redis, a snapshot of every index…).
      if (connected && d?.backup?.server_wide && d.databases_label) items.push({ label: t('backups:menu'), action: () => tabs.openBackups(cid!, '') });
      if (connected && d?.capabilities.create_database && !c.config.read_only) items.push(guarded(cid!, '', 'create_database', { label: t('explorer:menu.newDatabase'), action: () => createDatabase(cid!) }));
      if (d && !d.databases_label && connected) dbItems(cid!, conns.live[cid!]?.databases[0] ?? '', items);
      items.push({ label: t('explorer:menu.copyServer'), divided: true, action: () => copy(serverText(c)) });
      items.push({ label: t('explorer:menu.copyConnectionName'), action: () => copy(c.name) });
      items.push({ label: t('explorer:menu.editConnection'), action: () => ui.editConnection(cid!), divided: true });
      items.push({ label: t('explorer:menu.duplicateConnection'), action: () => ui.duplicateConnection(cid!) });
      items.push({ label: t('explorer:menu.moveTo'), action: () => ui.moveItem({ kind: 'connection', id: cid! }) });
      items.push({ label: t('explorer:menu.deleteConnection'), danger: true, divided: true, action: () => removeConnection(c) });
      break;
    }
    case 'group': {
      const f = n.folder!;
      items.push({ label: t('explorer:menu.newConnectionHere'), action: () => ui.newConnection(f.id) });
      items.push({ label: t('explorer:menu.newSubfolder'), action: () => ui.newFolder(f.id) });
      items.push({ label: t('explorer:menu.editFolder'), action: () => ui.editFolder(f.id), divided: true });
      items.push({ label: t('explorer:menu.moveTo'), action: () => ui.moveItem({ kind: 'folder', id: f.id }) });
      items.push({ label: t('explorer:menu.disconnectAll'), action: () => disconnectAll(f.id) });
      items.push({ label: t('explorer:menu.deleteFolder'), danger: true, divided: true, action: () => removeFolder(f) });
      break;
    }
    case 'database':
      items.push({ label: t('explorer:menu.newQuery'), action: () => newQuery(cid!, db) });
      dbItems(cid!, db, items);
      items.push({ label: t('common:refresh'), divided: true, action: () => loadDatabase(cid!, db, true) });
      items.push({ label: t('explorer:menu.copyName'), action: () => copy(db) });
      if (conns.driverOf(cid!)?.capabilities.drop_database && !conns.byId(cid!)?.config.read_only) {
        items.push(guarded(cid!, db, 'drop_database', { label: t('explorer:menu.dropDatabase'), danger: true, divided: true, action: () => dropDatabase(cid!, db) }));
      }
      break;
    case 'queries':
      items.push({ label: t('explorer:menu.newQuery'), action: () => newQuery(cid!, db) });
      items.push({ label: t('common:refresh'), action: () => conns.loadQueries(cid!, db, true) });
      break;
    case 'query':
      items.push({ label: t('common:open'), action: () => open(n, false) });
      items.push({ label: t('explorer:menu.renameEllipsis'), action: () => renameQuery(n.query!) });
      items.push({ label: t('common:duplicate'), action: () => duplicateQuery(n.query!) });
      items.push({ label: t('common:delete'), danger: true, divided: true, action: () => deleteQuery(n.query!) });
      break;
    case 'migrations':
      items.push({ label: t('explorer:menu.newMigration'), action: () => newMigration(cid!, db) });
      items.push({ label: t('common:refresh'), action: () => conns.loadMigrations(cid!, db, true) });
      break;
    case 'migration':
      items.push({ label: t('common:open'), action: () => openSavedMigration(n.migration!) });
      items.push({ label: t('explorer:menu.renameEllipsis'), action: () => renameSavedMigration(n.migration!) });
      items.push({ label: t('common:duplicate'), action: () => duplicateSavedMigration(n.migration!) });
      items.push({ label: t('migration:saved.delete'), danger: true, divided: true, action: () => deleteSavedMigration(n.migration!) });
      break;
    case 'folder': {
      // Inside a schema node, new objects go to that schema.
      const d = conns.driverOf(cid!);
      if (d?.designer && d.designer.kind === n.kindId) {
        items.push({ label: `${tb(d.designer.label)}…`, action: () => tabs.openDesigner(cid!, db, n.schema ?? null) });
      }
      for (const tpl of d?.create_templates.filter((x) => x.kind === n.kindId) ?? []) {
        items.push({ label: tb(tpl.label), action: () => newFromTemplate(cid!, db, tpl, n.schema ?? defaultSchema(cid!)) });
      }
      items.push({ label: t('common:refresh'), divided: items.length > 0, action: () => conns.loadObjects(cid!, db, true) });
      break;
    }
    case 'schema': {
      const s = n.schema!;
      const d = conns.driverOf(cid!);
      items.push({ label: t('explorer:menu.newQuery'), action: () => newQuery(cid!, db) });
      if (d?.designer) items.push({ label: `${tb(d.designer.label)}…`, action: () => tabs.openDesigner(cid!, db, s) });
      for (const tpl of d?.create_templates ?? []) items.push({ label: tb(tpl.label), action: () => newFromTemplate(cid!, db, tpl, s) });
      items.push({ label: t('common:refresh'), divided: true, action: () => loadDatabase(cid!, db, true) });
      items.push({ label: t('explorer:menu.copyName'), action: () => copy(s) });
      break;
    }
    case 'object': {
      const o = n.object!;
      const ref = { kind: o.kind, schema: o.schema, name: o.name };
      const kind = conns.driverOf(cid!)?.object_kinds.find((k) => k.id === o.kind);
      if (kind?.browsable ?? true) items.push({ label: t('explorer:menu.viewData'), action: () => tabs.openObject(cid!, db, ref, 'data', false) });
      if (kind?.has_columns ?? true) items.push({ label: t('explorer:menu.structure'), action: () => tabs.openObject(cid!, db, ref, 'structure', false) });
      if (kind?.has_definition ?? true) items.push({ label: t('explorer:menu.definition'), action: () => tabs.openObject(cid!, db, ref, 'definition', false) });
      if (kind?.browsable ?? true) items.push({ label: t('dataCompare:menu'), action: () => tabs.openDataCompare(cid!, db, ref) });
      if (cloneable(conns.driverOf(cid!), kind, o.kind)) items.push({ label: t('cloneTable:menu'), action: () => { cloning.value = { connectionId: cid!, database: db, object: ref }; } });
      if (kind?.browsable ?? true) {
        items.push({
          label: t('explorer:menu.newSelectQuery'), divided: true,
          action: async () => {
            const { api } = await import('../api/client');
            newQuery(cid!, db, await api.browseQuery(cid!, db, ref, 100), o.name);
          },
        });
      }
      items.push({ label: t('explorer:menu.copyName'), divided: true, action: () => copy(o.schema ? `${o.schema}.${o.name}` : o.name) });
      if (kind?.has_columns) items.push({ label: t('explorer:menu.refreshColumns'), action: () => conns.loadColumns(cid!, db, o, true) });
      items.push({ label: t('explorer:menu.deleteEllipsis'), danger: true, divided: true, action: () => dropObjects(cid!, db, [ref]) });
      break;
    }
    case 'column':
      items.push({ label: t('explorer:menu.copyName'), action: () => copy(n.label) });
      break;
    case 'keyns': {
      const ks = conns.driverOf(cid!)?.key_search;
      const shown = ks?.syntax === 'glob' ? `${n.prefix}*` : n.prefix;
      items.push({ label: t('explorer:menu.searchOnServer', { pattern: shown }), action: () => searchIn(n) });
      items.push({ label: t('explorer:menu.copyPrefix'), action: () => copy(n.prefix ?? '') });
      break;
    }
    default:
      return;
  }
  menu.value = { x: e.clientX, y: e.clientY, items };
}

// -- drag and drop ------------------------------------------------------------------------
// Connections and folders drag onto a folder, or onto the header / empty
// area for the top level.
type Dragged = { kind: 'connection' | 'folder'; id: string };
const dropTarget = ref<string | null>(null);
let dragged: Dragged | null = null;

function onDragStart(e: DragEvent, n: TNode) {
  dragged = n.type === 'group' ? { kind: 'folder', id: n.folder!.id } : { kind: 'connection', id: n.connectionId! };
  e.dataTransfer?.setData('text/plain', n.label);
  if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
}
function onDragOver(e: DragEvent, target: string) {
  if (!dragged) return;
  e.preventDefault();
  dropTarget.value = target;
}
async function onDrop(e: DragEvent, folderId: string | null) {
  // A folder dropped on a connection of its own subtree, or on itself.
  if (dragged?.kind === 'folder' && dragged.id === folderId) { onDragEnd(); return; }
  e.preventDefault();
  dropTarget.value = null;
  const d = dragged;
  dragged = null;
  if (!d) return;
  try {
    if (d.kind === 'connection') await conns.moveConnection(d.id, folderId);
    else await conns.moveFolder(d.id, folderId);
    if (folderId && !expanded.value.includes(`g:${folderId}`)) expanded.value = [...expanded.value, `g:${folderId}`];
  } catch (err) {
    ElMessage.error(String((err as { message?: string })?.message ?? err));
  }
}
/** Where a drop on this node puts things: into a folder, or next to a
 *  connection (its folder). `undefined` = not a drop target. */
function dropFolderOf(n: TNode): string | null | undefined {
  if (n.type === 'group') return n.folder!.id;
  if (n.type === 'connection') return conns.byId(n.connectionId!)?.folder_id ?? null;
  return undefined;
}
function onDragEnd() {
  dragged = null;
  dropTarget.value = null;
}

// -- sizing / filter ------------------------------------------------------------------------
const wrap = ref<HTMLDivElement | null>(null);
const height = ref(400);
let ro: ResizeObserver | null = null;
onMounted(() => {
  ro = new ResizeObserver(() => { height.value = wrap.value?.clientHeight ?? 400; });
  if (wrap.value) ro.observe(wrap.value);
});
onBeforeUnmount(() => ro?.disconnect());

// -- full name of a cut-off node, after resting the pointer on it ----------------------
// One floating tooltip for the whole (virtualized) tree instead of one per node.
const tip = ref<{ text: string; x: number; y: number } | null>(null);
let tipTimer: ReturnType<typeof setTimeout> | undefined;
function hideTip() {
  clearTimeout(tipTimer);
  tip.value = null;
}
function onTreeOver(e: MouseEvent) {
  const label = (e.target as HTMLElement).closest<HTMLElement>('.ex-label');
  if (!label) return hideTip();
  clearTimeout(tipTimer);
  tipTimer = setTimeout(() => {
    // Only when the name doesn't fit.
    if (!label.isConnected || label.scrollWidth <= label.clientWidth) return;
    const r = label.getBoundingClientRect();
    tip.value = { text: label.textContent ?? '', x: r.left, y: r.bottom + 4 };
  }, 700);
}
onBeforeUnmount(() => clearTimeout(tipTimer));

const tree = ref<any>(null);
const filter = ref('');
// el-tree-v2's filter opens every node it keeps (all of them for an empty
// query) and never says so: after filtering, the tree's expansion is set
// here instead, so no connection or database opens by itself.
function applyFilter() {
  const q = filter.value.trim();
  tree.value?.filter(q);
  // Cleared: back to what was open (`expanded` only changes by hand).
  // Filtering: open the way to each match, not the matches themselves.
  tree.value?.setExpandedKeys(q ? pathsTo(q) : expanded.value);
}
function pathsTo(q: string): string[] {
  const keys: string[] = [];
  const walk = (n: TNode): boolean => {
    let below = false;
    for (const c of n.children ?? []) if (walk(c)) below = true;
    if (below) keys.push(n.id);
    return below || filterNode(q, n);
  };
  data.value.forEach(walk);
  return keys;
}
/** Text matches names and connection tags; `tag:prod` only tags. */
function filterNode(q: string, n: TNode) {
  const text = q.trim().toLowerCase();
  if (!text) return true;
  const tags = n.type === 'connection' ? tagsOf(n.connectionId).map((t) => t.toLowerCase()) : [];
  if (text.startsWith('tag:')) {
    const want = text.slice(4).trim();
    return n.type === 'connection' && tags.some((t) => (want ? t === want : true));
  }
  const label = n.qualifier ? `${n.qualifier}.${n.label}` : n.label;
  return label.toLowerCase().includes(text) || tags.some((t) => t.includes(text));
}
const tagsOf = (connectionId?: string) => (connectionId ? conns.byId(connectionId)?.tags ?? [] : []);
/** Clicking a tag shows only its connections; clicking it again, all. */
function filterByTag(tag: string) {
  filter.value = activeTag.value?.toLowerCase() === tag.toLowerCase() ? '' : `tag:${tag}`;
  applyFilter();
}
/** The tag the explorer is filtered by (`tag:prod`), if any. */
const activeTag = computed(() => {
  const m = /^tag:\s*(.*)$/i.exec(filter.value.trim());
  return m ? m[1] : null;
});
function clearFilter() {
  filter.value = '';
  applyFilter();
}

// -- reveal (from a tab: double click / "Ir a la base") -------------------------------------
function findNode(id: string, nodes: TNode[] = data.value): TNode | null {
  for (const n of nodes) {
    if (n.id === id) return n;
    const inner = n.children ? findNode(id, n.children) : null;
    if (inner) return inner;
  }
  return null;
}

watch(() => ui.reveal?.seq, async () => {
  const r = ui.reveal;
  const c = r ? conns.byId(r.connectionId) : null;
  if (!r || !c) return;
  if (filter.value) { filter.value = ''; applyFilter(); }
  if (!(await conns.ensureConnected(c.id))) return;
  const d = conns.driverOf(c.id);
  await Promise.all([conns.loadObjects(c.id, r.database), conns.loadQueries(c.id, r.database)]);
  // Everything above the target must be open.
  const keys: string[] = [];
  for (let f = conns.folders.find((x) => x.id === c.folder_id), g = 0; f && g < 50; f = conns.folders.find((x) => x.id === f!.parent_id), g++) {
    keys.unshift(`g:${f.id}`);
  }
  keys.push(`c:${c.id}`);
  const dbNode = d?.databases_label ? `d:${c.id}:${r.database}` : `c:${c.id}`;
  if (d?.databases_label) keys.push(dbNode);
  let target = dbNode;
  if (r.queryId) {
    keys.push(`qs:${c.id}:${r.database}`);
    target = `q:${r.queryId}`;
  } else if (r.object) {
    // The schema node (when the tree has that level) and the kind folder.
    const s = r.object.schema;
    const kind = d?.key_search || d?.object_kinds.some((k) => k.id === r.object!.kind) ? r.object.kind : '__other';
    if (s != null && schemaLevel(c.id, r.database)?.includes(s)) {
      keys.push(`s:${c.id}:${r.database}:${s}`, `f:${c.id}:${r.database}:${s}:${kind}`);
    } else {
      keys.push(`f:${c.id}:${r.database}:${kind}`);
    }
    const sep = d?.key_search?.separator;
    if (sep) {
      // The namespace folders down to the key (those that exist are opened).
      const parts = r.object.name.split(sep);
      for (let i = 1; i < parts.length; i++) keys.push(`kn:${c.id}:${r.database}:${parts.slice(0, i).join(sep)}${sep}`);
    }
    target = `o:${c.id}:${r.database}:${r.object.kind}:${r.object.schema ?? ''}:${r.object.name}`;
  }
  expanded.value = [...new Set([...expanded.value, ...keys])];
  await nextTick();
  await nextTick();
  tree.value?.setCurrentKey(target);
  tree.value?.scrollToNode(target, 'center');
  if (!r.menu) return;
  await nextTick();
  const node = findNode(target);
  const el = document.querySelector(`.ex-tree [data-key="${CSS.escape(target)}"]`);
  if (!node || !el) return;
  const rect = el.getBoundingClientRect();
  onContext(new MouseEvent('contextmenu', { clientX: rect.left + 40, clientY: rect.bottom }), node);
});

// -- multiple selection of objects or databases (Cmd/Ctrl+click, Shift+click) ----------
const picked = ref<Set<string>>(new Set());
let anchor: TNode | null = null;
/** Nodes that can be picked together (one type at a time). */
const pickable = (n: TNode | null): n is TNode => !!n && (n.type === 'object' || n.type === 'database');
/** The picked database nodes (of any connection). */
function pickedDatabases(): TNode[] {
  const out: TNode[] = [];
  const walk = (list: TNode[]) => list.forEach((x) => {
    if (x.type === 'database' && picked.value.has(x.id)) out.push(x);
    if (x.children) walk(x.children);
  });
  walk(data.value);
  return out;
}
function findParent(list: TNode[], id: string): TNode[] | null {
  for (const n of list) {
    if (n.children?.some((c) => c.id === id)) return n.children;
    const deeper = n.children ? findParent(n.children, id) : null;
    if (deeper) return deeper;
  }
  return null;
}
/** The picked object nodes (same connection and database as `n`). */
function pickedNodes(n: TNode): TNode[] {
  const out: TNode[] = [];
  const walk = (list: TNode[]) => list.forEach((x) => {
    if (x.type === 'object' && picked.value.has(x.id) && x.connectionId === n.connectionId && x.database === n.database) out.push(x);
    if (x.children) walk(x.children);
  });
  walk(data.value);
  return out;
}

let lastClick = { id: '', at: 0 };
function onClick(n: TNode, node: { expanded: boolean; isLeaf?: boolean }, e?: MouseEvent) {
  if (pickable(n) && e && (e.metaKey || e.ctrlKey)) {
    // A selection holds one type: picking another type starts over.
    const s = anchor && anchor.type !== n.type ? new Set<string>() : new Set(picked.value);
    // The node open so far joins the selection too.
    if (anchor && anchor.type === n.type && !s.size) s.add(anchor.id);
    if (s.has(n.id)) s.delete(n.id);
    else s.add(n.id);
    picked.value = s;
    anchor = n;
    return;
  }
  if (pickable(n) && e?.shiftKey && anchor && anchor.type === n.type) {
    const siblings = findParent(data.value, n.id) ?? [];
    const a = siblings.findIndex((x) => x.id === anchor!.id);
    const b = siblings.findIndex((x) => x.id === n.id);
    if (a >= 0 && b >= 0) {
      const s = new Set(picked.value);
      for (const x of siblings.slice(Math.min(a, b), Math.max(a, b) + 1)) if (x.type === n.type) s.add(x.id);
      picked.value = s;
      return;
    }
  }
  if (n.type === 'keymore') {
    conns.moreKeys(n.connectionId!, n.database ?? '');
    return;
  }
  if (n.type === 'keysearch') return;
  if (picked.value.size) picked.value = new Set();
  anchor = n;
  // Objects and queries open on click (expanding them needs the arrow);
  // everything else toggles.
  if (n.type !== 'object' && n.type !== 'query' && n.type !== 'migration') {
    if (!node.isLeaf) tree.value?.[node.expanded ? 'collapseNode' : 'expandNode'](node);
    return;
  }
  // el-tree-v2 has no dblclick event: tell a double click by timing.
  const now = Date.now();
  const dbl = lastClick.id === n.id && now - lastClick.at < 350;
  lastClick = { id: n.id, at: now };
  open(n, !dbl);
}

/** "Importar conexiones" is open. */
const importing = ref(false);
/** The tool the first-run suggestion found, to open the dialog on it. */
const importSource = ref<'dbeaver' | 'dbgate' | 'datagrip' | 'azure_data_studio' | 'ssms' | null>(null);
</script>

<template>
  <div class="ex">
    <div class="ex-header">
      <span class="nm-section-title" style="margin: 0">{{ $t('explorer:title') }}</span>
      <div class="nm-spacer" />
      <button class="ide-icon-btn" :title="$t('explorer:header.newConnection')" @click="ui.newConnection()">
        <el-icon><ei-plus /></el-icon>
      </button>
      <button class="ide-icon-btn" :title="$t('explorer:header.newFolder')" @click="ui.newFolder(null)">
        <el-icon><ei-folder-add /></el-icon>
      </button>
      <button class="ide-icon-btn" :title="$t('explorer:header.importConnections')" @click="importing = true">
        <el-icon><ei-download /></el-icon>
      </button>
      <button class="ide-icon-btn" :title="$t('explorer:header.reload')" @click="conns.load()">
        <el-icon><ei-refresh /></el-icon>
      </button>
    </div>
    <div class="ex-filter">
      <el-input
        v-model="filter"
        :placeholder="$t('explorer:filter.placeholder')"
        clearable
        size="small"
        @input="applyFilter"
        @clear="applyFilter"
        @keydown.esc="clearFilter"
      >
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
      <div v-if="activeTag !== null" class="ex-tagbar">
        <span>{{ $t('explorer:filter.onlyTag') }}</span>
        <span v-if="activeTag" class="ex-tag" :style="{ '--t': tagColor(activeTag) }">{{ activeTag }}</span>
        <span v-else>{{ $t('explorer:filter.anyTag') }}</span>
        <button class="ex-tagbar-all" @click="clearFilter">{{ $t('explorer:filter.showAll') }}</button>
      </div>
    </div>
    <div
      ref="wrap"
      class="ex-tree"
      :class="{ 'drop-root': dropTarget === 'root' }"
      @mouseover="onTreeOver"
      @mouseleave="hideTip"
      @mousedown="hideTip"
      @wheel.passive="hideTip"
      @dragover="onDragOver($event, 'root')"
      @drop="onDrop($event, null)"
    >
      <div v-if="conns.loaded && !conns.list.length && !conns.folders.length" class="ex-empty">
        <p class="nm-muted">{{ $t('explorer:empty.noConnections') }}</p>
        <el-button type="primary" @click="ui.newConnection()">{{ $t('explorer:header.newConnection') }}</el-button>
      </div>
      <el-tree-v2
        v-else
        ref="tree"
        :data="data"
        :props="{ value: 'id', label: 'label', children: 'children' }"
        :height="height"
        :item-size="22"
        :indent="12"
        :filter-method="filterNode"
        :expand-on-click-node="false"
        :default-expanded-keys="expanded"
        @node-expand="onExpand"
        @node-collapse="onCollapse"
        @node-click="onClick"
        @node-contextmenu="(e: Event, n: TNode) => onContext(e as MouseEvent, n)"
      >
        <template #default="{ data: n }">
          <span
            class="ex-node"
            :class="[n.type, n.status, { 'drop-here': dropTarget === n.id, picked: picked.has(n.id) }]"
            :draggable="n.type === 'group' || n.type === 'connection'"
            :data-dnd="n.type === 'group' || n.type === 'connection' ? '' : undefined"
            @dragstart.stop="onDragStart($event, n)"
            @dragend="onDragEnd"
            @dragover.stop="dropFolderOf(n) !== undefined ? onDragOver($event, n.id) : undefined"
            @drop.stop="dropFolderOf(n) !== undefined ? onDrop($event, dropFolderOf(n)!) : undefined"
          >
            <template v-if="n.type === 'connection'">
              <span class="ex-conn-ic">
                <EngineIcon :id="conns.byId(n.connectionId)?.config.driver ?? ''" :name="n.hint ?? ''" :size="15" />
                <span class="ex-conn-dot" :class="conns.live[n.connectionId]?.status" :style="{ '--c': conns.colorOf(n.connectionId) || 'var(--nm-accent)' }" />
              </span>
            </template>
            <el-icon v-else-if="n.type === 'group'" class="ex-ic g" :style="n.color ? { color: n.color } : undefined">
              <ei-folder-opened v-if="expanded.includes(n.id)" /><ei-folder v-else />
            </el-icon>
            <el-icon v-else-if="n.type === 'database'" class="ex-ic"><ei-coin /></el-icon>
            <el-icon v-else-if="n.type === 'schema'" class="ex-ic s"><ei-folder-opened v-if="expanded.includes(n.id)" /><ei-folder v-else /></el-icon>
            <el-icon v-else-if="n.type === 'queries'" class="ex-ic q"><ei-tickets /></el-icon>
            <!-- A kind's group (Tablas, Vistas…) shows its kind's icon; "Otros" a folder. -->
            <el-icon v-else-if="n.type === 'folder' && n.kindId && KIND_ICONS[n.kindId]" class="ex-ic o"><component :is="`ei-${KIND_ICONS[n.kindId]}`" /></el-icon>
            <el-icon v-else-if="n.type === 'folder'" class="ex-ic f"><ei-folder /></el-icon>
            <el-icon v-else-if="n.type === 'query'" class="ex-ic q"><ei-document /></el-icon>
            <el-icon v-else-if="n.type === 'migrations'" class="ex-ic q"><ei-switch /></el-icon>
            <el-icon
              v-else-if="n.type === 'migration'"
              class="ex-ic mg"
              :class="[n.state, { 'is-loading': n.state === 'running' }]"
              :title="$t(`migration:saved.state.${n.state}`)"
            >
              <ei-loading v-if="n.state === 'running'" /><ei-circle-check-filled v-else-if="n.state === 'done'" />
              <ei-circle-close-filled v-else-if="n.state === 'failed'" /><ei-warning-filled v-else-if="n.state === 'interrupted'" />
              <ei-remove-filled v-else-if="n.state === 'cancelled'" /><ei-edit-pen v-else />
            </el-icon>
            <el-icon v-else-if="n.type === 'column'" class="ex-ic" :class="{ pk: n.pk }"><ei-key v-if="n.pk" /><ei-minus v-else /></el-icon>
            <el-icon v-else-if="n.type === 'status' && n.status === 'loading'" class="ex-ic is-loading"><ei-loading /></el-icon>
            <el-icon v-else-if="n.type === 'status' && n.status === 'error'" class="ex-ic err"><ei-warning /></el-icon>
            <KeySearchRow v-else-if="n.type === 'keysearch'" :connection-id="n.connectionId" :database="n.database ?? ''" />
            <el-icon v-else-if="n.type === 'keyns'" class="ex-ic f"><ei-folder-opened v-if="expanded.includes(n.id)" /><ei-folder v-else /></el-icon>
            <el-icon v-else-if="n.type === 'keymore'" class="ex-ic q"><ei-arrow-down /></el-icon>
            <span v-else-if="n.type === 'object' && n.key?.key_type" class="ex-ktype" :class="n.key.key_type">{{ typeTag(n.key.key_type) }}</span>
            <el-icon v-else-if="n.type === 'object'" class="ex-ic o"><component :is="`ei-${n.icon}`" /></el-icon>
            <span v-if="n.type !== 'keysearch'" class="ex-label" :title="n.status === 'error' ? n.label : undefined"><span v-if="n.qualifier" class="ex-qual">{{ n.qualifier }}.</span>{{ n.label }}</span>
            <span v-if="n.type === 'connection' && tagsOf(n.connectionId).length" class="ex-tags">
              <span
                v-for="tag in tagsOf(n.connectionId).slice(0, 2)"
                :key="tag"
                class="ex-tag"
                :style="{ '--t': tagColor(tag) }"
                :title="$t('explorer:filter.tagTitle', { tag })"
                @click.stop="filterByTag(tag)"
              >{{ tag }}</span>
              <span v-if="tagsOf(n.connectionId).length > 2" class="ex-tag more" :title="tagsOf(n.connectionId).slice(2).join(', ')">
                +{{ tagsOf(n.connectionId).length - 2 }}
              </span>
            </span>
            <span v-if="n.count !== undefined" class="ex-count">{{ n.count }}</span>
            <span v-if="n.hint" class="ex-hint">{{ n.hint }}</span>
          </span>
        </template>
      </el-tree-v2>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <CloneTableDialog v-if="cloning" :connection-id="cloning.connectionId" :database="cloning.database" :object="cloning.object" @close="cloning = null" />
    <Teleport to="body">
      <div v-if="tip" class="ex-tip" :style="{ left: `${tip.x}px`, top: `${tip.y}px` }">{{ tip.text }}</div>
    </Teleport>
    <ImportConnectionsDialog v-if="importing" :initial-source="importSource" @close="importing = false; importSource = null" />
    <ImportSuggestion @import="(s) => { importSource = s; importing = true; }" />
    <SupportReminder />
    <TelemetryConsent />
  </div>
</template>

<style scoped>
.ex { display: flex; flex-direction: column; min-height: 0; height: 100%; }
.ex-header { display: flex; align-items: center; gap: 2px; height: 35px; padding: 0 8px 0 20px; flex-shrink: 0; }
.ex-filter { padding: 0 8px 6px; flex-shrink: 0; }
.ex-tagbar { display: flex; align-items: center; gap: 6px; margin-top: 6px; font-size: 11.5px; color: var(--nm-text-dim); }
.ex-tagbar .ex-tag { cursor: default; }
.ex-tagbar-all {
  margin-left: auto; padding: 0; border: 0; background: none; font: inherit; cursor: pointer;
  color: var(--nm-link, #3794ff);
}
.ex-tagbar-all:hover { text-decoration: underline; }
.ex-tree { flex: 1; min-height: 0; overflow: hidden; }
.ex-empty { padding: 16px 20px; }
/* The whole row drags and takes drops, not just the label. */
.ex-node { flex: 1; align-self: stretch; display: flex; align-items: center; gap: 5px; min-width: 0; font-size: 13px; padding-right: 8px; }
.ex-node[draggable='true'] { -webkit-user-drag: element; }
.ex-node.picked { background: var(--ide-selection); border-radius: 2px; box-shadow: -8px 0 0 var(--ide-selection); }
.ex-node.status { color: var(--nm-text-dim); font-style: italic; font-size: 12px; }
.ex-node.status.error { color: var(--nm-danger); font-style: normal; }
.ex-node.connection { font-weight: 600; color: var(--nm-text-strong); }
.ex-node.group { color: var(--nm-text-strong); }
.ex-node.drop-here { outline: 1px dashed var(--ide-focus); outline-offset: 1px; border-radius: 2px; }
.ex-tree.drop-root { outline: 1px dashed var(--ide-focus); outline-offset: -3px; }
.ex-label { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ex-tags { display: inline-flex; gap: 3px; flex-shrink: 0; margin-left: 6px; }
.ex-tag {
  padding: 0 5px; border-radius: 8px; font-size: 10px; line-height: 15px; cursor: pointer; white-space: nowrap;
  color: var(--t); border: 1px solid color-mix(in srgb, var(--t) 60%, transparent);
  background: color-mix(in srgb, var(--t) 14%, transparent);
}
.ex-tag:hover { background: color-mix(in srgb, var(--t) 28%, transparent); }
.ex-tag.more { --t: var(--nm-text-dim); cursor: default; }
.ex-tip {
  position: fixed; z-index: 3000; max-width: min(520px, calc(100vw - 24px)); padding: 5px 9px; border-radius: 4px;
  font-size: 12px; line-height: 1.4; overflow-wrap: anywhere; pointer-events: none;
  color: var(--nm-text-strong); background: var(--nm-bg-elev); border: 1px solid var(--nm-border-soft);
  box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35);
}
.ex-hint { color: var(--nm-text-muted); font-size: 11.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ex-count { color: var(--nm-text-muted); font-size: 11px; }
.ex-ic { flex-shrink: 0; color: var(--nm-text-dim); font-size: 14px; }
.ex-ic.f { color: #dcb67a; }
.ex-ic.g { color: #c5a46d; }
.ex-ic.s { color: #9cb4d8; }
/* Flat mode: the object's schema, dim before its name. */
.ex-qual { color: var(--nm-text-muted); }
.ex-ic.q { color: #75beff; }
.ex-ic.o { color: #4ec9b0; }
.ex-ic.pk { color: #d7ba7d; }
.ex-ic.err { color: var(--nm-danger); }
.ex-ic.mg { color: var(--nm-text-dim); }
.ex-ic.mg.running { color: var(--nm-accent); }
.ex-ic.mg.done { color: var(--nm-success); }
.ex-ic.mg.failed { color: var(--nm-danger); }
.ex-ic.mg.interrupted, .ex-ic.mg.cancelled { color: var(--nm-warning); }
.ex-ic.mg.elsewhere { opacity: 0.6; }
.ex-node.keymore { color: var(--nm-link, #3794ff); cursor: pointer; }
.ex-node.keymore:hover .ex-label { text-decoration: underline; }
.ex-node.keymore .ex-label { flex-shrink: 0; }
/* A key's type, as Redis names it. */
.ex-ktype {
  flex-shrink: 0; min-width: 30px; padding: 0 3px; border-radius: 3px; box-sizing: border-box; text-align: center;
  font: 600 9px/14px var(--nm-font-mono, ui-monospace, monospace); letter-spacing: 0.02em;
  color: #fff; background: #6b7280;
}
.ex-ktype.string { background: #3f7fbf; }
.ex-ktype.hash { background: #a0522d; }
.ex-ktype.list { background: #2e8b57; }
.ex-ktype.set { background: #8a4fbf; }
.ex-ktype.zset { background: #b8860b; }
.ex-ktype.stream { background: #c0392b; }
.ex-ktype.json { background: #2f8f8f; }
.ex-conn-ic { position: relative; display: inline-flex; flex-shrink: 0; }
/* Status dot on the logo's corner, ringed with the sidebar color. */
.ex-conn-dot {
  position: absolute; right: -3px; bottom: -2px;
  width: 8px; height: 8px; border-radius: 50%; box-sizing: border-box;
  border: 2px solid var(--c); background: var(--ide-sidebar); box-shadow: 0 0 0 1.5px var(--ide-sidebar);
}
.ex-conn-dot.connected { background: var(--c); }
.ex-conn-dot.error { border-color: var(--nm-danger); }
.ex-conn-dot.connecting { animation: ide-pulse 1.4s ease-in-out infinite; }
</style>
