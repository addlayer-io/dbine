import { acceptHMRUpdate, defineStore } from 'pinia';
import { ElMessageBox } from 'element-plus';
import { listen } from '@tauri-apps/api/event';
import { api, errorKind, errorMessage } from '../api/client';
import { askTrustSshHost } from '../composables/sshTrust';
import type { ColumnInfo, ConnectionFolder, DbObject, DriverInfo, KeyEntry, Permissions, SavedConnection, SavedQuery, SchemaInfo } from '../api/types';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { rt } from '../composables/i18nLabels';
import { useOutputStore } from './output';
import { trackConnectionOpened } from '../composables/telemetry';
import { savedMigrationsApi, type SavedMigration } from '../api/savedMigrations';
import { migrationApi, type MigrationEvent, type RunInfo } from '../api/migration';

// Drivers, folders, saved connections, what's live on each, and the
// explorer's caches (objects, columns and queries per database).

export interface LiveConnection {
  status: 'connecting' | 'connected' | 'error';
  serverVersion: string;
  databases: string[];
  defaultDatabase: string;
  error: string | null;
  /** While connecting: `databases` are the explorer cache's (last time's). */
  fromCache?: boolean;
}

export interface Loadable<T> {
  /** stale: `items` are the explorer cache's (last time's), shown while the
   *  server is asked again; its answer replaces them. */
  status: 'loading' | 'ready' | 'error' | 'stale';
  items: T;
  error: string | null;
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
/** Explorer-cache lookups of a connection's databases, while connecting. */
const cachedDatabases = new Map<string, Promise<void>>();
/** The cache's name for an object's columns (as `cache_item` in Rust). */
const columnsItem = (o: { kind: string; schema: string | null; name: string }) => `${o.kind}\u0001${o.schema ?? ''}\u0001${o.name}`;

/** Object lists being read, by `dbKey` (a second expand waits for the first). */
const objectsLoading = new Map<string, Promise<void>>();
/** Permission checks in flight, by `dbKey`. */
const permissionsLoading = new Map<string, Promise<void>>();

export const dbKey = (connectionId: string, database: string) => `${connectionId}\u0000${database}`;
export const objKey = (connectionId: string, database: string, schema: string | null, name: string) =>
  `${connectionId}\u0000${database}\u0000${schema ?? ''}\u0000${name}`;

export const useConnectionsStore = defineStore('connections', {
  state: () => ({
    drivers: [] as DriverInfo[],
    list: [] as SavedConnection[],
    folders: [] as ConnectionFolder[],
    loaded: false,
    live: {} as Record<string, LiveConnection>,
    objects: {} as Record<string, Loadable<DbObject[]>>,
    /** Every schema of a database (empty ones too), by `dbKey`; null or
     *  absent: not listed by the driver, derived from `objects`. */
    schemas: {} as Record<string, SchemaInfo[] | null>,
    columns: {} as Record<string, Loadable<ColumnInfo[]>>,
    queries: {} as Record<string, Loadable<SavedQuery[]>>,
    /** Saved migrations per database (the "Migraciones" node). */
    migrations: {} as Record<string, Loadable<SavedMigration[]>>,
    /** Their current runs on this machine, by run id (the node's status icons). */
    migrationRuns: {} as Record<string, RunInfo>,
    listeningMigrations: false,
    keys: {} as Record<string, KeyBrowse>,
    /** What the login may do, by `dbKey` ('' database: the server's actions). */
    permissions: {} as Record<string, Permissions>,
    /** Downloads in progress, by component. */
    downloads: {} as Record<string, ComponentDownload>,
    listening: false,
  }),
  getters: {
    byId: (s) => (id: string) => s.list.find((c) => c.id === id),
    driver: (s) => (id: string) => s.drivers.find((d) => d.id === id),
    /** The driver of a saved connection. */
    driverOf: (s) => (connectionId: string): DriverInfo | undefined => {
      const id = s.list.find((c) => c.id === connectionId)?.config.driver;
      return s.drivers.find((d) => d.id === id);
    },
    isConnected: (s) => (id: string) => s.live[id]?.status === 'connected',
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
      [this.drivers, this.list, this.folders] = await Promise.all([api.listDrivers(), api.listConnections(), api.listFolders()]);
      this.loaded = true;
    },

    async save(conn: SavedConnection) {
      const saved = await api.saveConnection(conn);
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
      for (const map of [this.objects, this.schemas, this.columns, this.queries, this.migrations, this.keys, this.permissions]) {
        for (const k of Object.keys(map)) if (k.startsWith(prefix)) delete map[k];
      }
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
     *  it). Resolves false when it couldn't. */
    async connect(id: string, password: string | null = null): Promise<boolean> {
      const conn = this.byId(id);
      if (!conn) return false;
      await this.listenDownloads();
      this.live[id] = { status: 'connecting', serverVersion: '', databases: [], defaultDatabase: '', error: null };
      // Last time's databases, shown while the server answers.
      const lookup = api.getCached<string[]>(id, '', 'databases').then((dbs) => {
        const live = this.live[id];
        if (dbs?.length && live?.status === 'connecting') {
          live.databases = dbs;
          live.fromCache = true;
          live.defaultDatabase = conn.config.database;
        }
      }).catch(() => {});
      cachedDatabases.set(id, lookup);
      try {
        // A failed download leaves no stale progress behind.
        const r = await api.connect(id, password).finally(() => {
          for (const [k, d] of Object.entries(this.downloads)) if (d.drivers.includes(conn.config.driver)) delete this.downloads[k];
        });
        this.live[id] = {
          status: 'connected', serverVersion: r.server_version, databases: r.databases,
          defaultDatabase: r.default_database, error: null,
        };
        // A reconnect may be with another login: check again.
        for (const k of Object.keys(this.permissions)) if (k.startsWith(`${id}\u0000`)) delete this.permissions[k];
        useOutputStore().add('info', t('core:connections.connected', { version: r.server_version }), { where: conn.name });
        trackConnectionOpened(conn.config.driver);
        return true;
      } catch (e) {
        const kind = errorKind(e);
        // The tunnel's SSH server isn't known yet: trust it (the user checks
        // the fingerprint) and connect again.
        const entry = await askTrustSshHost(e);
        if (entry) {
          // The command's `fingerprint` takes the entry bound to its server.
          await api.trustSshHost(id, entry);
          await this.load();
          return this.connect(id, password);
        }
        if (kind === 'password_required' || (password !== null && kind === 'auth_failed')) {
          // The backend explains when it's not the plain "no saved password" case.
          const raw = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : String(e);
          const why = kind === 'auth_failed' || raw !== 'se necesita la contraseña' ? errorMessage(e) : null;
          const typed = await askPassword(conn, why);
          if (typed !== null) return this.connect(id, typed);
        }
        this.live[id] = { status: 'error', serverVersion: '', databases: [], defaultDatabase: '', error: errorMessage(e) };
        useOutputStore().add('error', t('core:connections.connectFailed', { error: errorMessage(e) }), { where: conn.name });
        return false;
      }
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

    async disconnect(id: string) {
      await api.disconnect(id).catch(() => {});
      this.forget(id);
    },

    async refreshDatabases(id: string) {
      const live = this.live[id];
      if (!live) return;
      live.databases = await api.listDatabases(id);
    },

    /** Wait for the explorer cache's databases of a connection that's
     *  connecting (they're in `live[id].databases` then, if there were any). */
    async cachedDatabasesReady(id: string): Promise<void> {
      await cachedDatabases.get(id);
    },

    /** Make sure the connection is up; connects when needed. */
    async ensureConnected(id: string): Promise<boolean> {
      const s = this.live[id]?.status;
      if (s === 'connected') return true;
      if (s === 'connecting') {
        await new Promise<void>((resolve) => {
          const t = setInterval(() => {
            if (this.live[id]?.status !== 'connecting') { clearInterval(t); resolve(); }
          }, 100);
        });
        return this.live[id]?.status === 'connected';
      }
      return this.connect(id);
    },

    async loadObjects(connectionId: string, database: string, force = false) {
      const k = dbKey(connectionId, database);
      // Databases of keys aren't listed: their keys are searched, page by page.
      if (this.driverOf(connectionId)?.key_search) {
        if (!force && this.objects[k]?.status === 'ready') return;
        this.objects[k] = { status: 'ready', items: [], error: null };
        const last = this.keys[k];
        return this.searchKeys(connectionId, database, last?.pattern ?? '', last?.keyType ?? '');
      }
      if (!force && this.objects[k]?.status === 'ready') return;
      if (!force && objectsLoading.has(k)) return objectsLoading.get(k);
      const run = (async () => {
        // What's there stays on screen while the server is asked; the first
        // time, last time's list from the explorer cache.
        const had = this.objects[k]?.items ?? [];
        this.objects[k] = { status: had.length ? 'stale' : 'loading', items: had, error: null };
        if (!had.length) {
          const [cached, cachedSchemas] = await Promise.all([
            api.getCached<DbObject[]>(connectionId, database, 'objects').catch(() => null),
            api.getCached<SchemaInfo[]>(connectionId, database, 'schemas').catch(() => null),
          ]);
          if (this.objects[k]?.status === 'loading' && (cached?.length || cachedSchemas?.length)) {
            if (!(k in this.schemas)) this.schemas[k] = cachedSchemas ?? null;
            this.objects[k] = { status: 'stale', items: cached ?? [], error: null };
          }
        }
        if (!(await this.ensureConnected(connectionId))) {
          const now = this.objects[k];
          if (now && !now.items.length) this.objects[k] = { status: 'error', items: [], error: this.live[connectionId]?.error ?? null };
          return;
        }
        try {
          const read = await api.listDatabaseObjects(connectionId, database);
          this.schemas[k] = read.schemas;
          this.objects[k] = { status: 'ready', items: read.objects, error: null };
        } catch (e) {
          this.objects[k] = { status: 'error', items: [], error: errorMessage(e) };
        }
      })();
      objectsLoading.set(k, run);
      try {
        await run;
      } finally {
        objectsLoading.delete(k);
      }
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
      }
    },

    async loadColumns(connectionId: string, database: string, obj: DbObject, force = false) {
      const k = objKey(connectionId, database, obj.schema, obj.name);
      if (!force && this.columns[k]?.status === 'ready') return this.columns[k].items;
      const had = this.columns[k]?.items ?? [];
      this.columns[k] = { status: had.length ? 'stale' : 'loading', items: had, error: null };
      if (!had.length) {
        const cached = await api.getCached<ColumnInfo[]>(connectionId, database, 'columns', columnsItem(obj)).catch(() => null);
        if (cached?.length && this.columns[k]?.status === 'loading') this.columns[k] = { status: 'stale', items: cached, error: null };
      }
      if (!(await this.ensureConnected(connectionId))) return this.columns[k]?.items ?? [];
      try {
        const items = await api.getColumns(connectionId, database, { kind: obj.kind, schema: obj.schema, name: obj.name });
        this.columns[k] = { status: 'ready', items, error: null };
        return items;
      } catch (e) {
        this.columns[k] = { status: 'error', items: [], error: errorMessage(e) };
        return [];
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
