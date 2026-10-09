import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { api, errorMessage } from '../api/client';
import type { CodeObject, SyncScript } from '../api/compare';
import type { ObjectRef, RenameImpact, RenameSpec, RenameTarget } from '../api/types';
import type { MenuItem } from '../components/ContextMenu.vue';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { loadIndexUsage } from './indexUsage';

// "Renombrar…" (explorer tree): the engine's rename plus the code that names
// the object, rewritten, in one reviewed script (docs/rename.md). The
// impact and the script come from the backend (`rename_impact`,
// `rename_script`); the run is a schema sync (`schema_sync_run`), atomic
// where the engine allows it.

export interface RenameDialogTarget {
  connectionId: string;
  database: string;
  target: RenameTarget;
}

/** The driver's spec, when it renames anything. */
export function renameSpec(connectionId: string): RenameSpec | null {
  return useConnectionsStore().driverOf(connectionId)?.rename ?? null;
}

/** Whether the engine renames that target (as `RenameSpec::allows`). */
export function renameAllowed(spec: RenameSpec | null, target: RenameTarget): boolean {
  if (!spec) return false;
  switch (target.what) {
    case 'object': return spec.kinds.includes(target.object.kind);
    case 'column': return spec.columns;
    case 'index': return spec.indexes;
    case 'constraint': return spec.constraints;
    case 'schema': return spec.schemas;
  }
}

/** "Renombrar…", or null where it isn't offered: engines that don't rename
 *  that, and read-only connections. */
export function renameItem(t0: RenameDialogTarget, open: (t: RenameDialogTarget) => void, divided = false): MenuItem | null {
  const conns = useConnectionsStore();
  if (conns.byId(t0.connectionId)?.config.read_only || !renameAllowed(renameSpec(t0.connectionId), t0.target)) return null;
  return { label: t('rename:menu'), divided, action: () => open(t0) };
}

/** A schema rename that is the database's: engines whose database is the
 *  schema (ClickHouse), offered on the database node. */
export function isDatabaseRename(d: RenameDialogTarget): boolean {
  const driver = useConnectionsStore().driverOf(d.connectionId);
  return d.target.what === 'schema' && !!driver && !driver.has_schemas;
}

/** "Renombrar…" on a database node, where the engine has no schema level
 *  and renames its databases as schemas. */
export function databaseRenameItem(connectionId: string, database: string, open: (t: RenameDialogTarget) => void): MenuItem | null {
  const driver = useConnectionsStore().driverOf(connectionId);
  if (!database || !driver || driver.has_schemas) return null;
  return renameItem({ connectionId, database, target: { what: 'schema', database, schema: database } }, open, true);
}

export function oldName(target: RenameTarget): string {
  switch (target.what) {
    case 'object': return target.object.name;
    case 'column': return target.column;
    case 'index': return target.index;
    case 'constraint': return target.constraint;
    case 'schema': return target.schema;
  }
}

const qualified = (o: ObjectRef) => (o.schema ? `${o.schema}.${o.name}` : o.name);

/** What the dialog shows as the target: `dbo.Clientes`, `dbo.Clientes.Pepe`… */
export function targetLabel(target: RenameTarget): string {
  switch (target.what) {
    case 'object': return qualified(target.object);
    case 'schema': return target.schema;
    default: return `${qualified(target.table)}.${oldName(target)}`;
  }
}

export function renameImpact(d: RenameDialogTarget, newName: string, keepViewColumns: boolean): Promise<RenameImpact> {
  return invoke<RenameImpact>('rename_impact', {
    args: { connection_id: d.connectionId, database: d.database, target: d.target, new_name: newName, keep_view_columns: keepViewColumns },
  });
}

/** The script for the dependents the user kept. */
export function renameScript(d: RenameDialogTarget, newName: string, impact: RenameImpact, rewrites: { object: CodeObject; schemabound: boolean; carried: CodeObject[] }[]): Promise<SyncScript> {
  return invoke<SyncScript>('rename_script', {
    args: {
      connection_id: d.connectionId, database: d.database,
      request: { target: d.target, new_name: newName, table: impact.table, definition: impact.definition },
      rewrites,
    },
  });
}

export interface RenameRun {
  /** The statement that failed, translated; null when it went through. */
  error: string | null;
  /** An atomic run that failed was rolled back: nothing changed. */
  rolledBack: boolean;
}

/** Run the script (`cancel_query` on `sync:<runId>` stops it), then refresh
 *  the tree and retarget the tabs of what was renamed. */
export async function runRename(d: RenameDialogTarget, newName: string, statements: string[], atomic: boolean, runId: string): Promise<RenameRun> {
  try {
    const r = await invoke<{ done: number; failed: [number, string] | null; rolled_back?: boolean }>('schema_sync_run', {
      args: { connection_id: d.connectionId, database: d.database, statements, run_id: runId, atomic },
    });
    if (r.failed) return { error: tb(r.failed[1]), rolledBack: !!r.rolled_back };
  } catch (e) {
    return { error: errorMessage(e), rolledBack: false };
  }
  await afterRename(d, newName);
  return { error: null, rolledBack: false };
}

async function afterRename(d: RenameDialogTarget, newName: string) {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const { connectionId: cid, database: db, target } = d;
  if (isDatabaseRename(d)) {
    // The database itself: its tabs follow it, the tree reads the list again.
    for (const tab of tabs.tabs) if (tab.connectionId === cid && tab.database === db) tab.database = newName;
    tabs.persist();
    await conns.refreshDatabases(cid).catch(() => {});
    return;
  }
  await conns.loadObjects(cid, db, true).catch(() => {});
  switch (target.what) {
    case 'object':
      tabs.renameObject(cid, db, target.object, { ...target.object, name: newName });
      break;
    case 'schema':
      tabs.renameSchema(cid, db, target.schema, newName);
      break;
    default: {
      const ref = target.table;
      conns.loadColumns(cid, db, { kind: ref.kind, schema: ref.schema, name: ref.name, parent: null }, true);
      loadIndexUsage(cid, db, ref, true);
      if (target.what === 'column') tabs.renameColumn(cid, db, ref, target.column, newName);
    }
  }
  // Query tabs aren't edited: say how many still name the old name.
  const old = oldName(target);
  const word = new RegExp(`(^|[^\\w$#@])${old.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}($|[^\\w$#@])`, 'i');
  let count = 0;
  for (const tab of tabs.tabs) {
    if (tab.kind !== 'query' || tab.connectionId !== cid) continue;
    try {
      if (word.test((await api.getQuery(tab.queryId)).sql)) count++;
    } catch { /* gone */ }
  }
  if (count) ElMessage.info({ message: t('rename:queryTabs', { count, name: old }), duration: 6000 });
}
