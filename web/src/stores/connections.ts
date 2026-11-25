import { acceptHMRUpdate, defineStore } from 'pinia';
import { ElMessageBox } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { api, errorDetail, errorKind, errorMessage } from '../api/client';
import { askRunProxyCommand, askTrustSshHost } from '../composables/sshTrust';
import { askAllowLegacyTls } from '../composables/legacyTls';
import { askApproveSecretSource } from '../composables/secretSourceApproval';
import { askMasterPassword, unlockOf } from '../composables/secretSourceUnlock';
import type { ColumnInfo, ConnectDetail, ConnectionFolder, DbObject, DriverInfo, KeyEntry, ObjectQuery, Permissions, SavedConnection, SavedQuery, SchemaInfo } from '../api/types';
import { hasOtherKinds, LatestRead, readOtherKinds, type OtherKinds } from '../composables/keyKinds';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { rt } from '../composables/i18nLabels';
import { connectingFrom } from './liveConnection';
import { useOutputStore } from './output';
import { trackConnectionOpened } from '../composables/telemetry';
import { savedMigrationsApi, type SavedMigration } from '../api/savedMigrations';
import { lookupConnection } from '../composables/lineageSource';
import { contextsApi, type SavedContext } from '../api/contexts';
import { migrationApi, type MigrationEvent, type RunInfo } from '../api/migration';

// Drivers, folders, saved connections, what's live on each, and the
// explorer's caches (objects, columns and queries per database).

export interface LiveConnection {
  status: 'connecting' | 'connected' | 'error';
  serverVersion: string;
  databases: string[];
  defaultDatabase: string;
  error: string | null;
  /** When the last connect failed (ms since the epoch). */
  failedAt?: number;
  /** Why the last connect failed, when the backend could tell (firewall,
   *  timeout…): the "Ver qué pasa…" dialog explains it. */
  detail?: ConnectDetail | null;
}

export interface Loadable<T> {
  /** stale: `items` are what was shown before (the explorer cache's, last
   *  time's, or this session's older read), kept while the server is asked
   *  again; its answer replaces them. */
  status: 'loading' | 'ready' | 'error' | 'stale';
  items: T;
  error: string | null;
  /** When the server's answer arrived (ms since the epoch): an older one is
   *  read again in the background on its next use (`REREAD_MS`). */
  at?: number;
}

/** An engine library being downloaded on first use (DuckDB). */
export interface ComponentDownload {
  component: string;
  /** Drivers (ids) waiting for it. */
  drivers: string[];
  done: number;
  total: number;
}

/** A database's keys as the explorer has searched them (Redis, etcd): a
 *  search on the server, loaded a page at a time. */
export interface KeyBrowse {
  /** The search the keys answer ('' = all). */
  pattern: string;
  /** '' = every type. */
  keyType: string;
  items: KeyEntry[];
  /** Where the next page starts; null when the search is over. */
  cursor: string | null;
  /** Keys in the database (or in the searched range), when the engine tells. */
  total: number | null;
  /** Keys the server has looked at so far. */
  scanned: number;
  status: 'loading' | 'ready' | 'error';
  error: string | null;
}

/** Keys per page of a key search. */
const KEY_PAGE = 500;
/** A new search keeps asking for pages until it has this many keys… */
const KEY_ENOUGH = 50;
/** …or for this long. */
const KEY_SEARCH_MS = 4000;
/** The latest search per database: a slower, older reply is dropped. */
const keySeq: Record<string, number> = {};
/** The same for a key database's other kinds (`loadOtherKinds`); a
 *  connection forgotten drops its reads in flight too. */
const otherReads = new LatestRead();
/** The connect in flight per connection: every caller shares it. */
const connecting = new Map<string, Promise<boolean>>();
/** Bumped by each disconnect: a connect that ends after one (a server or
 *  tunnel that didn't answer) leaves the connection as it is. */
const generation = new Map<string, number>();
/** A load that failed with a connection error checks the connection again
 *  (a connect), at most once per this long per connection. */
const RECHECK_MS = 10_000;
const lastRecheck = new Map<string, number>();
/** A failure repeated within this long (same message) is the same burst: one
 *  Output entry for all of it. */
const FAILURE_BURST_MS = 60_000;
const lastFailure = new Map<string, { message: string; at: number }>();
/** The cache's name for an object's columns (as `cache_item` in Rust). */
const columnsItem = (o: { kind: string; schema: string | null; name: string }) => `${o.kind}\u0001${o.schema ?? ''}\u0001${o.name}`;

/** Whole object lists being read, by `dbKey` (a second caller waits for the first). */
const objectsLoading = new Map<string, Promise<void>>();
/** Schema levels being read, by `dbKey`. */
const levelsLoading = new Map<string, Promise<void>>();
/** Tree folders being read, by `folderKey`. */
const foldersLoading = new Map<string, Promise<void>>();

/** A folder of the tree: a schema's objects of one kind (`schema` null: of
 *  every schema; `noSchema`: those without one; `other`: the kinds that
 *  aren't these, "Otros"). */
export interface FolderSpec {
  schema: string | null;
  noSchema?: boolean;
  kind: string | null;
  other?: string[];
}
/** A folder's objects so far; `next`: more remain ("Cargar más"). */
export interface FolderLoad extends Loadable<DbObject[]> {
  spec: FolderSpec;
  next: string | null;
  /** Its next page is being read. */
  more?: boolean;
}
/** A folder's page: the tree never reads more at once. */
export const FOLDER_PAGE = 1000;
export const folderKey = (connectionId: string, database: string, f: FolderSpec) =>
  `${connectionId}\u0000${database}\u0000${f.noSchema ? '\u0002' : f.schema ?? ''}\u0000${f.other ? '\u0002' : f.kind ?? ''}`;
function folderQuery(f: FolderSpec): ObjectQuery {
  return { schema: f.noSchema ? null : f.schema, no_schema: !!f.noSchema, kind: f.other ? null : f.kind, not_kinds: f.other ?? [], limit: FOLDER_PAGE };
}

/** A search's page (completion, the explorer's server search, pickers). */
const SEARCH_PAGE = 200;
/** How long a search's or a missed lookup's answer serves the same question. */
const SEARCH_TTL_MS = 30_000;
/** Searches answered, by database, schema and pattern. */
const searches = new Map<string, { pattern: string; schema: string | null; items: DbObject[]; complete: boolean; at: number }>();
/** Searches in flight (the same one asked twice waits for the first). */
const searching = new Map<string, Promise<DbObject[] | null>>();
/** Lookups the server answered with nothing, and when. */
const lookupMisses = new Map<string, number>();
/** At most this many found objects are kept per database (the oldest go). */
const FOUND_CAP = 20_000;
const objectId = (o: DbObject) => `${o.kind}\u0000${o.schema ?? ''}\u0000${o.name}`;
/** Columns being read, by `objKey` (likewise). */
const columnsLoading = new Map<string, Promise<ColumnInfo[]>>();
/** A connect's read of last time's database list (the explorer cache), by
 *  connection: the tree waits for it before opening a cached database. */
const priming = new Map<string, Promise<void>>();
/** Past this age, what the server answered is read again in the background
 *  on its next use (an expand, the window's focus): changes made outside
 *  DBine show up without "Actualizar". */
export const REREAD_MS = 5 * 60_000;
const isOld = (l: { at?: number }) => Date.now() - (l.at ?? 0) > REREAD_MS;
/** Permission checks in flight, by `dbKey`. */
const permissionsLoading = new Map<string, Promise<void>>();

/** A failed load: its error (the tree shows it; nothing stays spinning). */
function failedLoad<T>(error: string): Loadable<T[]> {
  return { status: 'error', items: [], error };
}

/** When each connection's database list was last read again (`refreshDatabasesSoon`). */
const lastDatabasesRead = new Map<string, number>();
const DATABASES_REREAD_MS = 3000;
export const dbKey = (connectionId: string, database: string) => `${connectionId}\u0000${database}`;
export const objKey = (connectionId: string, database: string, schema: string | null, name: string) =>
  `${connectionId}\u0000${database}\u0000${schema ?? ''}\u0000${name}`;

export const useConnectionsStore = defineStore('connections', {
  state: () => ({
    drivers: [] as DriverInfo[],
    list: [] as SavedConnection[],
    /** The lineage's internal connections: found by id (connect, metadata) but never in `list`, so never
     *  in the sidebar or any picker. */
    hidden: [] as SavedConnection[],
    folders: [] as ConnectionFolder[],
    loaded: false,
    live: {} as Record<string, LiveConnection>,
    /** A whole database's objects, by `dbKey`: only when something that
     *  needs every one asked for it (`loadAllObjects`); a key database's
     *  entry is its key search's state. */
    objects: {} as Record<string, Loadable<DbObject[]>>,
    /** Every schema of a database (empty ones too), by `dbKey`; null or
     *  absent: not listed by the driver. */
    schemas: {} as Record<string, SchemaInfo[] | null>,
    /** The tree's database level (its schemas), by `dbKey`. */
    levels: {} as Record<string, Loadable<SchemaInfo[] | null>>,
    /** The tree's folders, page by page, by `folderKey`. */
    objectFolders: {} as Record<string, FolderLoad>,
    /** Objects found by searches and lookups, by `dbKey` (see `knownObjects`). */
    found: {} as Record<string, DbObject[]>,
    columns: {} as Record<string, Loadable<ColumnInfo[]>>,
    queries: {} as Record<string, Loadable<SavedQuery[]>>,
    /** Saved migrations per database (the "Migraciones" node). */
    migrations: {} as Record<string, Loadable<SavedMigration[]>>,
    /** The AI assistant's contexts, by connection (its own and its databases'). */
    contexts: {} as Record<string, Loadable<SavedContext[]>>,
    /** Their current runs on this machine, by run id (the node's status icons). */
    migrationRuns: {} as Record<string, RunInfo>,
    listeningMigrations: false,
    keys: {} as Record<string, KeyBrowse>,
    /** A key database's objects of its driver's other kinds (Redis' search
     *  indexes), by `dbKey`: its keys are searched (`keys`), these listed. */
    otherKinds: {} as Record<string, OtherKinds>,
    /** What the login may do, by `dbKey` ('' database: the server's actions). */
    permissions: {} as Record<string, Permissions>,
    /** Downloads in progress, by component. */
    downloads: {} as Record<string, ComponentDownload>,
    listening: false,
  }),
  getters: {
    byId: (s) => (id: string) => lookupConnection(s.list, s.hidden, id),
    driver: (s) => (id: string) => s.drivers.find((d) => d.id === id),
    /** The driver of a saved connection. */
    driverOf: (s) => (connectionId: string): DriverInfo | undefined => {
      const id = lookupConnection(s.list, s.hidden, connectionId)?.config.driver;
      return s.drivers.find((d) => d.id === id);
    },
    isConnected: (s) => (id: string) => s.live[id]?.status === 'connected',
    /** A database's objects known so far: its whole list when something
     *  read it, the tree's loaded folders, and what searches and lookups
     *  found. Completion, hover and jumps read this; it isn't the whole
     *  database (look up a name with `findObject`). */
    knownObjects: (s) => (connectionId: string, database: string): DbObject[] => {
      const k = dbKey(connectionId, database);
      const seen = new Set<string>();
      const out: DbObject[] = [];
      const add = (o: DbObject) => {
        const id = objectId(o);
        if (!seen.has(id)) {
          seen.add(id);
          out.push(o);
        }
      };
      s.objects[k]?.items.forEach(add);
      const prefix = `${k}\u0000`;
      for (const [fk, f] of Object.entries(s.objectFolders)) if (fk.startsWith(prefix)) f.items.forEach(add);
      s.found[k]?.forEach(add);
      return out;
    },
    /** The privilege the login lacks for an action, or null when it may (or
     *  nobody knows: the server has the last word). */
    denied: (s) => (connectionId: string, database: string, action: keyof Permissions): string | null => {
      const a = s.permissions[dbKey(connectionId, database)]?.[action];
      return a?.state === 'denied' ? a.missing : null;
    },
    /** "Descargando DuckDB… 45 % (6 de 13 MB)" while a driver's library downloads. */
    downloadNote: (s) => (driverId: string): string | null => {
      const d = Object.values(s.downloads).find((x) => x.drivers.includes(driverId));
      if (!d) return null;
      const mb = (n: number) => Math.round(n / 1e6);
      const pct = d.total ? Math.floor((d.done / d.total) * 100) : 0;
      return rt('core:connections.downloading', { component: tb(d.component), pct, done: mb(d.done), total: mb(d.total) });
    },
    /** The connection's color, else the nearest folder's (environments). */
    colorOf: (s) => (connectionId: string): string | null => {
      const c = s.list.find((x) => x.id === connectionId);
      if (!c) return null;
      if (c.color) return c.color;
      let f = s.folders.find((x) => x.id === c.folder_id);
      for (let guard = 0; f && guard < 50; guard++) {
        if (f.color) return f.color;
        f = s.folders.find((x) => x.id === f!.parent_id);
      }
      return null;
    },
    /** "Cliente A / Producción" */
    folderPath: (s) => (folderId: string | null): string => {
      const parts: string[] = [];
      let f = s.folders.find((x) => x.id === folderId);
      for (let guard = 0; f && guard < 50; guard++) {
        parts.unshift(f.name);
        f = s.folders.find((x) => x.id === f!.parent_id);
      }
      return parts.join(' / ');
    },
  },
  actions: {
    async load() {
      [this.drivers, this.list, this.folders, this.hidden] = await Promise.all([
        api.listDrivers(), api.listConnections(), api.listFolders(), api.lineageInternalList().catch(() => [] as SavedConnection[]),
      ]);
      this.loaded = true;
    },

    /** The lineage's internal connections, read again (one was made, forgotten or added to the explorer). */
    async loadHidden() {
      this.hidden = await api.lineageInternalList();
    },

    /** "Agregar al explorador": the internal connection joins the explorer's list as a regular one. */
    async promoteHidden(id: string) {
      const saved = await api.lineageInternalPromote(id);
      this.hidden = this.hidden.filter((c) => c.id !== id);
      this.list = this.list.filter((c) => c.id !== id);
      this.list.push(saved);
      return saved;
    },

    /** "Olvidar": the internal connection and its remembered password go. */
    async forgetHidden(id: string) {
      await api.lineageInternalForget(id);
      this.hidden = this.hidden.filter((c) => c.id !== id);
      this.forget(id);
    },

    /** `duplicateOf`: the connection the form was copied from;
     *  `forgetTlsPin`: the user forgot its pinned certificate. */
    async save(conn: SavedConnection, duplicateOf: string | null = null, forgetTlsPin = false) {
      const saved = await api.saveConnection(conn, duplicateOf, forgetTlsPin);
      const i = this.list.findIndex((c) => c.id === saved.id);
      // Same place; new, or in another folder, last (as the backend orders it).
      if (i >= 0 && (this.list[i].folder_id ?? null) === (saved.folder_id ?? null)) this.list[i] = saved;
      else {
        if (i >= 0) this.list.splice(i, 1);
        this.list.push(saved);
      }
      // The backend dropped its sessions: start over on next expand.
      this.forget(saved.id);
      return saved;
    },

    async saveFolder(f: ConnectionFolder) {
      const saved = await api.saveFolder(f);
      const i = this.folders.findIndex((x) => x.id === saved.id);
      // Same place; new, or under another folder, last (as the backend orders it).
      if (i >= 0 && (this.folders[i].parent_id ?? null) === (saved.parent_id ?? null)) this.folders[i] = saved;
      else {
        if (i >= 0) this.folders.splice(i, 1);
        this.folders.push(saved);
      }
      return saved;
    },

    /** Its content moves up to its parent (same as the backend). */
    async deleteFolder(id: string) {
      await api.deleteFolder(id);
      const parent = this.folders.find((f) => f.id === id)?.parent_id ?? null;
      for (const c of this.list) if (c.folder_id === id) c.folder_id = parent;
      for (const f of this.folders) if (f.parent_id === id) f.parent_id = parent;
      this.folders = this.folders.filter((f) => f.id !== id);
    },

    async moveConnection(connectionId: string, folderId: string | null) {
      await api.moveConnection(connectionId, folderId);
      const i = this.list.findIndex((c) => c.id === connectionId);
      if (i >= 0 && (this.list[i].folder_id ?? null) !== folderId) {
        const [c] = this.list.splice(i, 1);
        c.folder_id = folderId;
        this.list.push(c); // last in its new folder
      }
    },

    /** Drag and drop: one level's connections or folders, in this order
     *  (`parentId` null = top level); then the store reads them back. */
    async reorderExplorer(kind: 'connection' | 'folder', parentId: string | null, ids: string[]) {
      await api.reorderExplorer(parentId, kind, ids);
      if (kind === 'connection') this.list = await api.listConnections();
      else this.folders = await api.listFolders();
    },

    /** Move a folder under another (null = top level). */
    async moveFolder(id: string, parentId: string | null) {
      const f = this.folders.find((x) => x.id === id);
      if (!f || f.parent_id === parentId || id === parentId) return;
      await this.saveFolder({ ...f, parent_id: parentId });
    },

    async remove(id: string) {
      await api.deleteConnection(id);
      this.list = this.list.filter((c) => c.id !== id);
      this.forget(id);
    },

    forget(id: string) {
      delete this.live[id];
      const prefix = `${id}\u0000`;
      for (const map of [this.objects, this.schemas, this.levels, this.objectFolders, this.found, this.columns, this.queries, this.migrations, this.keys, this.otherKinds, this.permissions]) {
        for (const k of Object.keys(map)) if (k.startsWith(prefix)) delete map[k];
      }
      for (const m of [searches, lookupMisses]) for (const k of [...m.keys()]) if (k.startsWith(prefix)) m.delete(k);
      // A read that lands after this doesn't bring the old list back.
      otherReads.forget(prefix);
    },

    /** Follow engine library downloads (the first connection to DuckDB). */
    async listenDownloads() {
      if (this.listening) return;
      this.listening = true;
      try {
        await listen<ComponentDownload>('component-download', (e) => {
          const d = e.payload;
          if (d.done >= d.total) delete this.downloads[d.component];
          else this.downloads[d.component] = d;
        });
      } catch {
        this.listening = false;
      }
    },

    /** Connect (asking for the password when the connection doesn't keep
     *  it). Resolves false when it couldn't. A connect already in flight is
     *  shared, not started again. */
    connect(id: string, password: string | null = null): Promise<boolean> {
      const running = connecting.get(id);
      if (running) return running;
      const p = this.openConnection(id, password).finally(() => connecting.delete(id));
      connecting.set(id, p);
      return p;
    },

    async openConnection(id: string, password: string | null): Promise<boolean> {
      const conn = this.byId(id);
      if (!conn) return false;
      // While the server is asked, last time's databases (the explorer
      // cache) show at once, and so do their cached objects when opened; a
      // failed connect replaces them with its error.
      const gen = generation.get(id) ?? 0;
      const prev = this.live[id];
      this.live[id] = connectingFrom(prev);
      const primed = api.getCached<string[]>(id, '', 'databases').catch(() => null).then((dbs) => {
        const l = this.live[id];
        if (dbs?.length && l?.status === 'connecting' && !l.databases.length && (generation.get(id) ?? 0) === gen) l.databases = dbs;
      });
      priming.set(id, primed);
      void primed.finally(() => { if (priming.get(id) === primed) priming.delete(id); });
      await this.listenDownloads();
      try {
        // A failed download leaves no stale progress behind.
        const r = await api.connect(id, password).finally(() => {
          for (const [k, d] of Object.entries(this.downloads)) if (d.drivers.includes(conn.config.driver)) delete this.downloads[k];
        });
        if ((generation.get(id) ?? 0) !== gen) return false;
        this.live[id] = {
          status: 'connected', serverVersion: r.server_version, databases: r.databases,
          defaultDatabase: r.default_database, error: null,
        };
        lastFailure.delete(id);
        // A reconnect may be with another login: check again.
        for (const k of Object.keys(this.permissions)) if (k.startsWith(`${id}\u0000`)) delete this.permissions[k];
        useOutputStore().add('info', t('core:connections.connected', { version: r.server_version }), { where: conn.name });
        trackConnectionOpened(conn.config.driver);
        return true;
      } catch (e) {
        if ((generation.get(id) ?? 0) !== gen) return false;
        const kind = errorKind(e);
        // A secret field's source came from outside this computer (an
        // import, a restored backup): the user sees exactly what would run
        // and, if they confirm it, it connects again.
        if (await askApproveSecretSource(e)) return this.openConnection(id, password);
        // A KeePass / Zoho Vault master password set to "ask": the backend
        // keeps what's typed in memory for this run, and it connects again.
        const unlock = unlockOf(e);
        if (unlock) {
          const typed = await askMasterPassword(unlock);
          if (typed !== null) {
            // The tunnel's SSH profile's source is the profile's (its id).
            await api.unlockSecretSource(unlock.connection_id ?? id, unlock.field, typed);
            return this.openConnection(id, password);
          }
        }
        // The tunnel's `~/.ssh/config` entry runs a command: only after a yes.
        if (await askRunProxyCommand(e)) return this.openConnection(id, password);
        // The tunnel's SSH server isn't known yet: trust it (the user checks
        // the fingerprint) and connect again.
        const entry = await askTrustSshHost(e);
        if (entry) {
          // The command's `fingerprint` takes the entry bound to its server.
          await api.trustSshHost(id, entry);
          await this.load();
          return this.openConnection(id, password);
        }
        // The server only speaks TLS 1.0/1.1: asked once; on yes the
        // connection keeps it and connects again through the legacy-TLS
        // driver (downloaded then, with the usual progress). Never without
        // that yes: a failed TLS 1.2 handshake doesn't fall back by itself.
        if (await askAllowLegacyTls(e)) {
          await api.allowLegacyTls(id);
          await this.load();
          return this.openConnection(id, password);
        }
        if (kind === 'password_required' || (password !== null && kind === 'auth_failed')) {
          // The backend explains when it's not the plain "no saved password" case.
          const raw = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : String(e);
          const why = kind === 'auth_failed' || raw !== 'se necesita la contraseña' ? errorMessage(e) : null;
          const typed = await askPassword(conn, why);
          if (typed !== null) return this.openConnection(id, typed);
        }
        await this.failed(id, e);
        return false;
      }
    },

    /** A connect failed: the connection shows the error (the explorer, its
     *  banner and nothing below it); Output hears once per burst. */
    async failed(id: string, e: unknown) {
      const message = errorMessage(e);
      const now = this.live[id];
      this.live[id] = {
        status: 'error', serverVersion: '', databases: [], defaultDatabase: now?.defaultDatabase ?? '', error: message,
        failedAt: Date.now(), detail: errorDetail(e),
      };
      const at = Date.now();
      const last = lastFailure.get(id);
      lastFailure.set(id, { message, at });
      if (last && last.message === message && at - last.at < FAILURE_BURST_MS) return;
      const conn = this.byId(id);
      useOutputStore().add('error', t('core:connections.connectFailed', { error: message }), { where: conn?.name ?? id });
    },

    /** Ask the server what the login may do (once per database; a failed
     *  check leaves every action on). */
    async loadPermissions(connectionId: string, database: string): Promise<void> {
      const k = dbKey(connectionId, database);
      if (this.permissions[k] || permissionsLoading.has(k) || !this.isConnected(connectionId)) return permissionsLoading.get(k);
      const p = api.getPermissions(connectionId, database)
        .then((r) => { this.permissions[k] = r; })
        .catch(() => {})
        .finally(() => permissionsLoading.delete(k));
      permissionsLoading.set(k, p);
      return p;
    },

    /** Close the connection's sessions and tunnel, whatever its state: a
     *  connect still in flight gives up and changes nothing when it ends. */
    async disconnect(id: string) {
      generation.set(id, (generation.get(id) ?? 0) + 1);
      connecting.delete(id);
      await api.disconnect(id).catch(() => {});
      this.forget(id);
    },

    /** "Conectar" on a connection that failed: from scratch, with new
     *  sessions and a new tunnel rather than the ones that broke. */
    async reconnect(id: string): Promise<boolean> {
      if (this.live[id]?.status === 'error') await this.disconnect(id);
      return this.connect(id);
    },

    async refreshDatabases(id: string) {
      const live = this.live[id];
      if (!live) return;
      live.databases = await api.listDatabases(id);
    },

    /** The database list read again when a picker opens, so one created
     *  outside DBine's menus (a `CREATE DATABASE` in a query, another tool)
     *  shows up: at most every few seconds, quietly, connected only. */
    refreshDatabasesSoon(id: string) {
      if (!id || this.live[id]?.status !== 'connected') return;
      const now = Date.now();
      if (now - (lastDatabasesRead.get(id) ?? 0) < DATABASES_REREAD_MS) return;
      lastDatabasesRead.set(id, now);
      void this.refreshDatabases(id).catch(() => {});
    },

    /** A load failed: with a connection error (kind "connect"), the
     *  connection is checked again — a connect, shared with any other caller —
     *  so a dropped one flips to the error state (banner, nothing below it)
     *  instead of leaving stale nodes. At most once per RECHECK_MS. */
    async recheckAfter(id: string, e: unknown) {
      if (errorKind(e) !== 'connect' || this.live[id]?.status !== 'connected') return;
      const at = Date.now();
      if (at - (lastRecheck.get(id) ?? 0) < RECHECK_MS) return;
      lastRecheck.set(id, at);
      await this.connect(id);
    },

    /** Make sure the connection is up; connects when needed (sharing a
     *  connect in flight). `auto`: a background load or an explorer expand —
     *  on a connection that failed it doesn't connect again (the banner's
     *  "Reintentar", Connect and explicit actions do). */
    async ensureConnected(id: string, opts: { auto?: boolean } = {}): Promise<boolean> {
      if (this.live[id]?.status === 'connected') return true;
      const running = connecting.get(id);
      if (running) return running;
      if (opts.auto && this.live[id]?.status === 'error') return false;
      return this.connect(id);
    },

    /** The tree's database level: its schemas (empty ones too; null: the
     *  driver doesn't list them), the cache's at once (also while
     *  connecting), then the server's. The kind folders below load on their
     *  own (`loadFolder`). A key database starts its key search instead.
     *  `quiet`: a background re-read, whose failure keeps what's shown. */
    async loadLevel(connectionId: string, database: string, force = false, quiet = false): Promise<void> {
      const k = dbKey(connectionId, database);
      // Databases of keys aren't listed: their keys are searched, page by page.
      if (this.driverOf(connectionId)?.key_search) {
        if (!force && this.objects[k]?.status === 'ready') return;
        const last = this.keys[k];
        // Not connected: the connection shows its error; nothing to search.
        if (!(await this.ensureConnected(connectionId, { auto: !force }))) {
          this.objects[k] = failedLoad(this.live[connectionId]?.error ?? t('common:error'));
          return;
        }
        this.objects[k] = { status: 'ready', items: [], error: null };
        const search = this.searchKeys(connectionId, database, last?.pattern ?? '', last?.keyType ?? '');
        if (!hasOtherKinds(this.driverOf(connectionId)?.object_kinds)) return search;
        await Promise.all([search, this.loadOtherKinds(connectionId, database)]);
        return;
      }
      const cur = this.levels[k];
      if (!force && cur?.status === 'ready') {
        if (isOld(cur) && !levelsLoading.has(k)) void this.loadLevel(connectionId, database, true, true);
        return;
      }
      if (!force && levelsLoading.has(k)) return levelsLoading.get(k);
      const auto = !force || quiet;
      const run = (async () => {
        const prev = this.levels[k];
        const shown = prev?.status === 'ready' || prev?.status === 'stale';
        this.levels[k] = { status: shown ? 'stale' : 'loading', items: prev?.items ?? null, error: null, at: prev?.at };
        if (!shown && !(auto && this.live[connectionId]?.status === 'error')) {
          const cached = await api.getCached<SchemaInfo[]>(connectionId, database, 'schemas').catch(() => null);
          if (cached && this.levels[k]?.status === 'loading') {
            this.schemas[k] = cached;
            this.levels[k] = { status: 'stale', items: cached, error: null };
          }
        }
        if (!(await this.ensureConnected(connectionId, { auto }))) {
          this.levels[k] = { status: 'error', items: null, error: this.live[connectionId]?.error ?? t('common:error') };
          return;
        }
        try {
          const schemas = await api.listSchemas(connectionId, database);
          this.schemas[k] = schemas;
          this.levels[k] = { status: 'ready', items: schemas, error: null, at: Date.now() };
        } catch (e) {
          if (quiet && shown) this.levels[k] = { status: 'ready', items: prev?.items ?? null, error: null, at: Date.now() };
          else this.levels[k] = { status: 'error', items: null, error: errorMessage(e) };
          void this.recheckAfter(connectionId, e);
        }
      })();
      levelsLoading.set(k, run);
      try {
        await run;
      } finally {
        if (levelsLoading.get(k) === run) levelsLoading.delete(k);
      }
    },

    /** A folder of the tree: a schema's objects of one kind (or "Otros", or
     *  those without a schema), its first page; the cache's at once (also
     *  while connecting), then the server's. "Cargar más" is `moreFolder`. */
    async loadFolder(connectionId: string, database: string, f: FolderSpec, force = false, quiet = false): Promise<void> {
      const key = folderKey(connectionId, database, f);
      const cur = this.objectFolders[key];
      if (!force && cur?.status === 'ready') {
        if (isOld(cur) && !foldersLoading.has(key)) void this.loadFolder(connectionId, database, f, true, true);
        return;
      }
      if (!force && foldersLoading.has(key)) return foldersLoading.get(key);
      const auto = !force || quiet;
      const query = folderQuery(f);
      const run = (async () => {
        const prev = this.objectFolders[key];
        const had = prev?.items ?? [];
        this.objectFolders[key] = { spec: f, status: had.length ? 'stale' : 'loading', items: had, next: prev?.next ?? null, error: null, at: prev?.at };
        if (!had.length && !(auto && this.live[connectionId]?.status === 'error')) {
          const cached = await api.getCachedPage(connectionId, database, query).catch(() => null);
          if (cached?.length && this.objectFolders[key]?.status === 'loading') {
            this.objectFolders[key] = { spec: f, status: 'stale', items: cached, next: null, error: null };
          }
        }
        if (!(await this.ensureConnected(connectionId, { auto }))) {
          this.objectFolders[key] = { spec: f, ...failedLoad<DbObject>(this.live[connectionId]?.error ?? t('common:error')), next: null };
          return;
        }
        try {
          // A refresh or a re-read asks the server, not a list kept a moment ago.
          const page = await api.listObjectPage(connectionId, database, query, { tree: true, fresh: force });
          this.objectFolders[key] = { spec: f, status: 'ready', items: page.objects, next: page.next, error: null, at: Date.now() };
        } catch (e) {
          if (quiet && had.length) this.objectFolders[key] = { spec: f, status: 'ready', items: had, next: prev?.next ?? null, error: null, at: Date.now() };
          else this.objectFolders[key] = { spec: f, ...failedLoad<DbObject>(errorMessage(e)), next: null };
          void this.recheckAfter(connectionId, e);
        }
      })();
      foldersLoading.set(key, run);
      try {
        await run;
      } finally {
        if (foldersLoading.get(key) === run) foldersLoading.delete(key);
      }
    },

    /** "Cargar más" in a folder: its next page, added below. */
    async moreFolder(connectionId: string, database: string, f: FolderSpec) {
      const key = folderKey(connectionId, database, f);
      const cur = this.objectFolders[key];
      if (!cur?.next || cur.more || cur.status !== 'ready') return;
      cur.more = true;
      try {
        const page = await api.listObjectPage(connectionId, database, { ...folderQuery(f), after: cur.next }, { tree: true });
        if (this.objectFolders[key] !== cur) return;
        cur.items.push(...page.objects);
        cur.next = page.next;
      } catch (e) {
        useOutputStore().add('error', errorMessage(e), { where: this.byId(connectionId)?.name ?? connectionId });
      } finally {
        cur.more = false;
      }
    },

    /** Objects whose name contains `pattern` (completion, the explorer's
     *  server search, pickers): one page from the server, kept a short
     *  while; what's found joins `knownObjects`. A longer pattern whose
     *  shorter one came back whole is answered from it, without the server.
     *  `kinds`: only those (filtered here). `tree`: on the tree's session. */
    async searchObjects(
      connectionId: string, database: string, pattern: string,
      opts: { schema?: string | null; kinds?: string[]; limit?: number; tree?: boolean } = {},
    ): Promise<DbObject[]> {
      return (await this.searchObjectsOrNull(connectionId, database, pattern, opts)) ?? [];
    },

    /** The explorer cache's objects whose name contains `pattern`, in every
     *  folder and list it kept for the database: at once, also without a
     *  connection (the filter shows them before the server answers). */
    async searchCached(connectionId: string, database: string, pattern: string, limit = SEARCH_PAGE): Promise<DbObject[]> {
      if (this.driverOf(connectionId)?.key_search || !pattern) return [];
      return (await api.getCachedPage(connectionId, database, { pattern, limit }).catch(() => null)) ?? [];
    },

    /** `searchObjects`, with null when the server couldn't answer (not
     *  connected, an error) instead of an empty list. */
    async searchObjectsOrNull(
      connectionId: string, database: string, pattern: string,
      opts: { schema?: string | null; kinds?: string[]; limit?: number; tree?: boolean } = {},
    ): Promise<DbObject[] | null> {
      if (this.driverOf(connectionId)?.key_search) return [];
      const k = dbKey(connectionId, database);
      const p = pattern.toLowerCase();
      const schema = opts.schema ?? null;
      const pick = (objs: DbObject[]) => objs.filter((o) => o.name.toLowerCase().includes(p) && (!opts.kinds || opts.kinds.includes(o.kind)));
      const now = Date.now();
      for (const [sk, s] of searches) {
        if (sk.startsWith(`${k}\u0000`) && now - s.at < SEARCH_TTL_MS && s.complete && s.schema === schema && p.includes(s.pattern)) return pick(s.items);
      }
      const sk = `${k}\u0000${schema ?? ''}\u0000${p}`;
      const running = searching.get(sk);
      if (running) {
        const got = await running;
        return got && pick(got);
      }
      const run = (async (): Promise<DbObject[] | null> => {
        if (!(await this.ensureConnected(connectionId, { auto: true }))) return null;
        const page = await api.listObjectPage(connectionId, database, { pattern: pattern || null, schema, limit: opts.limit ?? SEARCH_PAGE }, { tree: opts.tree })
          .catch(() => null);
        if (!page) return null;
        searches.set(sk, { pattern: p, schema, items: page.objects, complete: !page.next, at: Date.now() });
        this.remember(k, page.objects);
        return page.objects;
      })();
      searching.set(sk, run);
      try {
        const got = await run;
        return got && pick(got);
      } finally {
        searching.delete(sk);
      }
    },

    /** The object named `name` (in `schema` when given; among `kinds` when
     *  given): what's known first, else a lookup on the server. The exact
     *  spelling wins over one that differs only in case. */
    async findObject(connectionId: string, database: string, schema: string | null | undefined, name: string, kinds?: string[]): Promise<DbObject | null> {
      const pick = (objs: DbObject[]) => {
        const fits = objs.filter((o) => (!kinds || kinds.includes(o.kind)) && o.name.toLowerCase() === name.toLowerCase());
        const inSchema = schema == null ? fits : fits.filter((o) => o.schema === schema || o.schema?.toLowerCase() === schema.toLowerCase());
        return inSchema.find((o) => o.name === name && (schema == null || o.schema === schema)) ?? inSchema.find((o) => o.name === name) ?? inSchema[0] ?? null;
      };
      const known = pick(this.knownObjects(connectionId, database));
      if (known || this.driverOf(connectionId)?.key_search) return known;
      const k = dbKey(connectionId, database);
      const lk = `${k}\u0000${schema ?? ''}\u0000${name.toLowerCase()}`;
      const missed = lookupMisses.get(lk);
      if (missed && Date.now() - missed < SEARCH_TTL_MS) return null;
      if (!(await this.ensureConnected(connectionId, { auto: true }))) return null;
      const page = await api.listObjectPage(connectionId, database, { name, limit: 50 }).catch(() => null);
      if (!page) return null;
      this.remember(k, page.objects);
      const found = pick(page.objects);
      if (!found) lookupMisses.set(lk, Date.now());
      return found;
    },

    /** Whether the server said, not long ago, that there's no object named
     *  `path` (`table` or `schema.table`): a lookup found nothing. What isn't
     *  known yet isn't missing: only this marks a name unknown. */
    missing(connectionId: string, database: string, path: string): boolean {
      const parts = path.split('.');
      const name = parts[parts.length - 1] ?? '';
      const schema = parts.length > 1 ? parts[parts.length - 2] : null;
      const at = lookupMisses.get(`${dbKey(connectionId, database)}\u0000${schema ?? ''}\u0000${name.toLowerCase()}`);
      return at !== undefined && Date.now() - at < REREAD_MS;
    },

    /** Add what a search or a lookup found to the database's known objects. */
    remember(k: string, objs: DbObject[]) {
      if (!objs.length) return;
      const list = this.found[k] ?? (this.found[k] = []);
      const have = new Set(list.map(objectId));
      for (const o of objs) if (!have.has(objectId(o))) list.push(o);
      if (list.length > FOUND_CAP) list.splice(0, list.length - FOUND_CAP);
    },

    /** Read a database's structure again (Refresh, a structure change made
     *  in DBine or in the editor): its schema level, its loaded folders and,
     *  when something holds it, its whole list; searches start over.
     *  `quiet`: in the background, keeping what's shown if it fails. */
    async refreshDatabase(connectionId: string, database: string, quiet = false) {
      const k = dbKey(connectionId, database);
      if (this.driverOf(connectionId)?.key_search) return this.loadLevel(connectionId, database, true, quiet);
      for (const sk of [...searches.keys()]) if (sk.startsWith(`${k}\u0000`)) searches.delete(sk);
      for (const lk of [...lookupMisses.keys()]) if (lk.startsWith(`${k}\u0000`)) lookupMisses.delete(lk);
      delete this.found[k];
      const jobs: Promise<unknown>[] = [];
      if (this.levels[k]) jobs.push(this.loadLevel(connectionId, database, true, quiet));
      const prefix = `${k}\u0000`;
      for (const [fk, f] of Object.entries(this.objectFolders)) {
        if (fk.startsWith(prefix)) jobs.push(this.loadFolder(connectionId, database, f.spec, true, quiet));
      }
      const all = this.objects[k];
      if (all && all.status !== 'error') jobs.push(this.loadAllObjects(connectionId, database, true, quiet));
      await Promise.all(jobs);
    },

    /** A whole database's objects, for what truly needs every one (a
     *  migration's tables, a backup's or a script's object list, the grants
     *  screen, q and DAX completion). The tree, completion and lookups never
     *  ask for it: they read by folder, search or look up. `quiet`: a
     *  background re-read, whose failure keeps what's shown. */
    async loadAllObjects(connectionId: string, database: string, force = false, quiet = false): Promise<void> {
      const k = dbKey(connectionId, database);
      // Key databases aren't listed: their keys are searched (the tree's).
      if (this.driverOf(connectionId)?.key_search) return this.loadLevel(connectionId, database, force, quiet);
      const cur = this.objects[k];
      if (!force && cur?.status === 'ready') {
        // An old answer: read again in the background, what's shown stays.
        if (isOld(cur) && !objectsLoading.has(k)) void this.loadAllObjects(connectionId, database, true, true);
        return;
      }
      if (!force && objectsLoading.has(k)) return objectsLoading.get(k);
      // A background re-read doesn't connect again a connection that failed.
      const auto = !force || quiet;
      const run = (async () => {
        const prev = this.objects[k];
        const had = prev?.items ?? [];
        this.objects[k] = { status: had.length ? 'stale' : 'loading', items: had, error: null, at: prev?.at };
        // The first time, last time's list from the explorer cache at once,
        // also while the connection is still opening; a connection that
        // failed shows its error instead.
        if (!had.length && !(auto && this.live[connectionId]?.status === 'error')) {
          const [cached, cachedSchemas] = await Promise.all([
            api.getCached<DbObject[]>(connectionId, database, 'objects').catch(() => null),
            api.getCached<SchemaInfo[]>(connectionId, database, 'schemas').catch(() => null),
          ]);
          if (this.objects[k]?.status === 'loading' && (cached?.length || cachedSchemas?.length)) {
            if (!(k in this.schemas)) this.schemas[k] = cachedSchemas ?? null;
            this.objects[k] = { status: 'stale', items: cached ?? [], error: null };
          }
        }
        // A server that can't be reached replaces what's shown with its error.
        if (!(await this.ensureConnected(connectionId, { auto }))) {
          this.objects[k] = failedLoad(this.live[connectionId]?.error ?? t('common:error'));
          return;
        }
        try {
          const read = await api.listDatabaseObjects(connectionId, database);
          this.schemas[k] = read.schemas;
          this.objects[k] = { status: 'ready', items: read.objects, error: null, at: Date.now() };
        } catch (e) {
          // A background re-read that fails keeps what's shown (and tries
          // again after REREAD_MS); any other read shows the error.
          if (quiet && had.length) this.objects[k] = { status: 'ready', items: had, error: null, at: Date.now() };
          else this.objects[k] = failedLoad(errorMessage(e));
          void this.recheckAfter(connectionId, e);
        }
      })();
      objectsLoading.set(k, run);
      try {
        await run;
      } finally {
        if (objectsLoading.get(k) === run) objectsLoading.delete(k);
      }
    },

    /** A key database's objects of its driver's other kinds (Redis' search
     *  indexes), from its schema read, next to the key search
     *  (composables/keyKinds.ts). A failed read shows in their folder, with
     *  why; a refresh asks again. */
    async loadOtherKinds(connectionId: string, database: string) {
      const d = this.driverOf(connectionId);
      if (!d) return;
      const failed = await readOtherKinds({
        state: this.otherKinds, latest: otherReads, key: dbKey(connectionId, database), kinds: d.object_kinds,
        connectionId, database, call: (cmd, args) => invoke(cmd, args), errorText: errorMessage,
      });
      if (failed) void this.recheckAfter(connectionId, failed);
    },

    /** Start a key search: the first page of the keys matching `pattern`. */
    async searchKeys(connectionId: string, database: string, pattern: string, keyType = '') {
      const k = dbKey(connectionId, database);
      this.keys[k] = { pattern, keyType, items: [], cursor: null, total: null, scanned: 0, status: 'loading', error: null };
      // The store's (reactive) copy: what later reads of `this.keys[k]` return.
      const b = this.keys[k];
      await this.keyPage(connectionId, database, null);
      // A selective search over a big database finds little per page: keep
      // going a few seconds, so the first thing shown isn't an empty page.
      const until = Date.now() + KEY_SEARCH_MS;
      while (this.keys[k] === b && b.status === 'ready' && b.cursor && b.items.length < KEY_ENOUGH && Date.now() < until) {
        b.status = 'loading';
        await this.keyPage(connectionId, database, b.cursor);
      }
    },

    /** The next page of the current key search. */
    async moreKeys(connectionId: string, database: string) {
      const b = this.keys[dbKey(connectionId, database)];
      if (!b || b.status === 'loading' || !b.cursor) return;
      b.status = 'loading';
      await this.keyPage(connectionId, database, b.cursor);
    },

    async keyPage(connectionId: string, database: string, cursor: string | null) {
      const k = dbKey(connectionId, database);
      const seq = (keySeq[k] = (keySeq[k] ?? 0) + 1);
      const b = this.keys[k];
      try {
        const page = await api.scanKeys(connectionId, database, {
          pattern: b.pattern, key_type: b.keyType || null, cursor, count: KEY_PAGE,
        });
        if (keySeq[k] !== seq || this.keys[k] !== b) return;
        const known = new Set(b.items.map((x) => x.name));
        b.items.push(...page.keys.filter((x) => !known.has(x.name)));
        b.cursor = page.cursor;
        if (page.total !== null) b.total = page.total;
        b.scanned += page.scanned;
        b.status = 'ready';
      } catch (e) {
        if (keySeq[k] !== seq || this.keys[k] !== b) return;
        b.status = 'error';
        b.error = errorMessage(e);
        void this.recheckAfter(connectionId, e);
      }
    },

    /** An object's columns. `opts.tree`: asked by the explorer tree (read on
     *  the tree's own session, not behind the editor's); `opts.quiet`: a
     *  background re-read, whose failure keeps what's shown. */
    async loadColumns(
      connectionId: string, database: string, obj: DbObject, force = false, opts: { tree?: boolean; quiet?: boolean } = {},
    ): Promise<ColumnInfo[]> {
      const k = objKey(connectionId, database, obj.schema, obj.name);
      const cur = this.columns[k];
      if (!force && cur?.status === 'ready') {
        // An old answer: read again in the background, what's shown stays.
        if (isOld(cur) && !columnsLoading.has(k)) void this.loadColumns(connectionId, database, obj, true, { ...opts, quiet: true });
        return cur.items;
      }
      const running = columnsLoading.get(k);
      if (!force && running) return running;
      // A background re-read doesn't connect again a connection that failed.
      const auto = !force || !!opts.quiet;
      const run = (async (): Promise<ColumnInfo[]> => {
        const prev = this.columns[k];
        const had = prev?.items ?? [];
        this.columns[k] = { status: had.length ? 'stale' : 'loading', items: had, error: null, at: prev?.at };
        // The cache at once (also while connecting), then the server.
        if (!had.length && !(auto && this.live[connectionId]?.status === 'error')) {
          const cached = await api.getCached<ColumnInfo[]>(connectionId, database, 'columns', columnsItem(obj)).catch(() => null);
          if (cached?.length && this.columns[k]?.status === 'loading') this.columns[k] = { status: 'stale', items: cached, error: null };
        }
        if (!(await this.ensureConnected(connectionId, { auto }))) {
          this.columns[k] = failedLoad(this.live[connectionId]?.error ?? t('common:error'));
          return [];
        }
        try {
          const items = await api.getColumns(connectionId, database, { kind: obj.kind, schema: obj.schema, name: obj.name }, opts.tree);
          this.columns[k] = { status: 'ready', items, error: null, at: Date.now() };
          return items;
        } catch (e) {
          void this.recheckAfter(connectionId, e);
          if (opts.quiet && had.length) {
            this.columns[k] = { status: 'ready', items: had, error: null, at: Date.now() };
            return had;
          }
          this.columns[k] = failedLoad(errorMessage(e));
          return [];
        }
      })();
      columnsLoading.set(k, run);
      try {
        return await run;
      } finally {
        if (columnsLoading.get(k) === run) columnsLoading.delete(k);
      }
    },

    /** A connect's read of last time's databases (see `openConnection`):
     *  resolved once they're in `live` (or there were none). */
    cachedDatabases(id: string): Promise<void> {
      return priming.get(id) ?? Promise.resolve();
    },

    /** The editor ran `sql` on this database and it changes structure
     *  (CREATE, ALTER, DROP, RENAME…): its object list is read again in the
     *  background, and so are the loaded columns of the objects it names;
     *  the other loaded columns are read again on their next use. */
    structureChanged(connectionId: string, database: string, sql: string) {
      if (this.driverOf(connectionId)?.key_search) return;
      const k = dbKey(connectionId, database);
      const known = this.knownObjects(connectionId, database);
      void this.refreshDatabase(connectionId, database, true);
      const words = new Set(sql.toLowerCase().match(/[\w$#]+/g) ?? []);
      const prefix = `${k}\u0000`;
      for (const [key, l] of Object.entries(this.columns)) {
        if (!key.startsWith(prefix) || l.status !== 'ready') continue;
        const [schema, name] = key.slice(prefix.length).split('\u0000');
        if (words.has(name.toLowerCase())) {
          const obj = known.find((o) => o.name === name && (o.schema ?? '') === schema);
          if (obj) {
            void this.loadColumns(connectionId, database, obj, true, { quiet: true });
            continue;
          }
        }
        l.at = 0;
      }
    },

    async loadQueries(connectionId: string, database: string, force = false) {
      const k = dbKey(connectionId, database);
      if (!force && this.queries[k]?.status === 'ready') return;
      try {
        this.queries[k] = { status: 'ready', items: await api.listQueries(connectionId, database), error: null };
      } catch (e) {
        this.queries[k] = { status: 'error', items: [], error: errorMessage(e) };
      }
    },

    /** Save and keep the explorer's list in step. */
    async saveQuery(q: SavedQuery, checkpoint = false): Promise<SavedQuery> {
      const saved = await api.saveQuery(q, checkpoint);
      const k = dbKey(saved.connection_id, saved.database);
      // It may have moved to another database: drop it from the old list.
      for (const [key, l] of Object.entries(this.queries)) {
        if (key !== k) l.items = l.items.filter((x) => x.id !== saved.id);
      }
      const list = this.queries[k] ?? (this.queries[k] = { status: 'ready', items: [], error: null });
      const i = list.items.findIndex((x) => x.id === saved.id);
      if (i >= 0) list.items[i] = saved;
      else list.items.push(saved);
      list.items.sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base', numeric: true }));
      return saved;
    },

    async deleteQuery(q: SavedQuery) {
      await api.deleteQuery(q.id);
      const l = this.queries[dbKey(q.connection_id, q.database)];
      if (l) l.items = l.items.filter((x) => x.id !== q.id);
    },

    // -- saved migrations ------------------------------------------------------------------

    async loadMigrations(connectionId: string, database: string, force = false) {
      const k = dbKey(connectionId, database);
      if (!force && this.migrations[k]?.status === 'ready') return;
      this.listenMigrationRuns();
      try {
        const items = await savedMigrationsApi.list(connectionId, database);
        this.migrations[k] = { status: 'ready', items, error: null };
        await this.loadMigrationRuns(items);
      } catch (e) {
        this.migrations[k] = { status: 'error', items: [], error: errorMessage(e) };
      }
    },

    /** The current run of each entry, as this machine knows it. */
    async loadMigrationRuns(items: SavedMigration[]) {
      const ids = items.map((m) => m.run_ids[m.run_ids.length - 1]).filter((x): x is string => !!x);
      if (!ids.length) return;
      try {
        for (const r of await migrationApi.runs(undefined, ids)) this.migrationRuns[r.id] = r;
      } catch { /* the icons are optional */ }
    },

    /** A run started or ended: refresh the lists that link it. */
    listenMigrationRuns() {
      if (this.listeningMigrations) return;
      this.listeningMigrations = true;
      listen<MigrationEvent>('migration-progress', (e) => {
        const p = e.payload;
        const edge = p.event === 'plan' || p.event === 'run_finished' || (p.event === 'step' && p.phase === 'done');
        if (!edge) return;
        for (const l of Object.values(this.migrations)) {
          const linked = l.items.filter((m) => m.run_ids.includes(p.id));
          if (linked.length) this.loadMigrationRuns(linked);
        }
      }).catch(() => { this.listeningMigrations = false; });
    },

    /** Keep an entry in its list (newest first). */
    putMigration(saved: SavedMigration) {
      const k = dbKey(saved.connection_id, saved.database);
      const list = this.migrations[k] ?? (this.migrations[k] = { status: 'ready', items: [], error: null });
      const i = list.items.findIndex((x) => x.id === saved.id);
      if (i >= 0) list.items[i] = saved;
      else list.items.unshift(saved);
      return saved;
    },

    async loadContexts(connectionId: string, force = false) {
      if (!force && this.contexts[connectionId]?.status === 'ready') return;
      try {
        const items = await contextsApi.list(connectionId);
        this.contexts[connectionId] = { status: 'ready', items, error: null };
      } catch (e) {
        this.contexts[connectionId] = { status: 'error', items: [], error: errorMessage(e) };
      }
    },
    async saveContext(x: SavedContext) {
      const saved = await contextsApi.save(x);
      await this.loadContexts(saved.connection_id, true);
      return saved;
    },
    async deleteContext(x: SavedContext) {
      await contextsApi.delete(x.id);
      await this.loadContexts(x.connection_id, true);
    },
    contextById(id: string): SavedContext | undefined {
      for (const l of Object.values(this.contexts)) {
        const x = l.items.find((c) => c.id === id);
        if (x) return x;
      }
      return undefined;
    },
    migrationById(id: string): SavedMigration | undefined {
      for (const l of Object.values(this.migrations)) {
        const m = l.items.find((x) => x.id === id);
        if (m) return m;
      }
      return undefined;
    },

    async saveMigration(m: SavedMigration): Promise<SavedMigration> {
      return this.putMigration(await savedMigrationsApi.save(m));
    },

    async renameMigration(id: string, name: string): Promise<SavedMigration> {
      return this.putMigration(await savedMigrationsApi.rename(id, name));
    },

    async linkMigrationRun(id: string, runId: string): Promise<SavedMigration> {
      const saved = this.putMigration(await savedMigrationsApi.linkRun(id, runId));
      await this.loadMigrationRuns([saved]);
      return saved;
    },

    async duplicateMigration(id: string, name: string): Promise<SavedMigration> {
      return this.putMigration(await savedMigrationsApi.duplicate(id, name));
    },

    async deleteMigration(m: SavedMigration) {
      await savedMigrationsApi.delete(m.id);
      const l = this.migrations[dbKey(m.connection_id, m.database)];
      if (l) l.items = l.items.filter((x) => x.id !== m.id);
    },

    /** "Query N" not used yet in that database. */
    nextQueryName(connectionId: string, database: string): string {
      const names = new Set((this.queries[dbKey(connectionId, database)]?.items ?? []).map((q) => q.name));
      let n = 1;
      while (names.has(t('core:connections.queryName', { n }))) n++;
      return t('core:connections.queryName', { n });
    },
  },
});

async function askPassword(conn: SavedConnection, error: string | null): Promise<string | null> {
  try {
    const { value } = await ElMessageBox.prompt(
      `${error ? `${error}\n\n` : ''}${t('core:connections.password', { user: conn.config.username ?? '', connection: conn.name })}`,
      t('common:connect'),
      { inputType: 'password', confirmButtonText: t('common:connect'), cancelButtonText: t('common:cancel') },
    );
    return value ?? '';
  } catch {
    return null;
  }
}

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useConnectionsStore, import.meta.hot));
