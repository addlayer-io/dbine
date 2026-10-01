import { invoke } from '@tauri-apps/api/core';
import { errorMessage } from '../api/client';
import { compareApi, type SyncScript } from '../api/compare';
import type { TableSchema } from '../api/schema-types';
import type { IndexUsage, ObjectRef } from '../api/types';
import type { MenuItem } from '../components/ContextMenu.vue';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { indexUsageEntry, loadIndexUsage } from './indexUsage';

// "Eliminar índice…" (explorer tree and the "Índices" tab). The script comes
// from the engine's own schema-sync generator: the table as it is against the
// same table without that index (`schema_sync_script`), so each engine writes
// the right statement for each kind (a UNIQUE constraint, a full-text or
// memory-optimized index…). It runs like a schema sync (`schema_sync_run`).

export interface DropIndexTarget {
  connectionId: string;
  database: string;
  table: ObjectRef;
  index: string;
}

/** The index's usage, as last read for the table. */
export function indexUsageOf(target: DropIndexTarget): IndexUsage | null {
  return indexUsageEntry(target.connectionId, target.database, target.table)?.report?.indexes.find((i) => i.name === target.index) ?? null;
}

/** The menu item, or null where it isn't offered: engines without schema
 *  sync and read-only connections. The primary key shows it disabled. */
export function dropIndexItem(target: DropIndexTarget, open: (target: DropIndexTarget) => void, divided = true): MenuItem | null {
  const conns = useConnectionsStore();
  if (!conns.driverOf(target.connectionId)?.supports_schema_sync || conns.byId(target.connectionId)?.config.read_only) return null;
  const item: MenuItem = { label: t('explorer:indexes.drop.menu'), danger: true, divided, action: () => open(target) };
  return indexUsageOf(target)?.primary_key ? { ...item, disabled: true, hint: t('explorer:indexes.drop.pkHint') } : item;
}

/** The script that drops the index, with the generator's warnings. */
export async function dropIndexScript(target: DropIndexTarget): Promise<SyncScript> {
  const conns = useConnectionsStore();
  if (!(await conns.ensureConnected(target.connectionId))) throw new Error(t('common:error'));
  const tables = await invoke<TableSchema[]>('database_schema', { args: { connection_id: target.connectionId, database: target.database } });
  const table = tables.find((x) => x.name === target.table.name && (x.schema ?? null) === (target.table.schema ?? null));
  if (!table) throw new Error(t('explorer:indexes.drop.tableNotFound', { table: target.table.name }));
  if (!table.indexes.some((i) => i.name === target.index)) throw new Error(t('explorer:indexes.drop.indexNotFound', { name: target.index }));
  const without: TableSchema = { ...table, indexes: table.indexes.filter((i) => i.name !== target.index) };
  return compareApi.script(target.connectionId, [{ op: 'alter', old: table, new: without }], [], []);
}

/** Run the script; the error of the statement that failed, or null. */
export async function runDropIndex(target: DropIndexTarget, statements: string[]): Promise<string | null> {
  try {
    const r = await compareApi.run(target.connectionId, target.database, statements, crypto.randomUUID());
    if (r.failed) return tb(r.failed[1]);
  } catch (e) {
    return errorMessage(e);
  }
  // The table's indexes changed: the tree (columns, "Índices") and an open "Índices" tab.
  const conns = useConnectionsStore();
  const ref = target.table;
  conns.loadColumns(target.connectionId, target.database, { kind: ref.kind, schema: ref.schema, name: ref.name, parent: null }, true);
  loadIndexUsage(target.connectionId, target.database, ref, true);
  return null;
}
