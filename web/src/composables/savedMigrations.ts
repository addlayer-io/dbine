import { ElMessage, ElMessageBox } from 'element-plus';
import { errorMessage } from '../api/client';
import type { RunInfo } from '../api/migration';
import type { SavedMigration } from '../api/savedMigrations';
import { t } from '../i18n';
import { confirmNative } from '../native';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// The "Migraciones" node's actions (explorer menus, the Migrate tab).

/** Saves in flight per migration (a tab's autosave), so deleting waits for them. */
export const migrationSaves = new Map<string, Promise<unknown>>();

/** "Nueva migración…": a new tab; its entry is saved on the first change. */
export function newMigration(connectionId: string, database: string) {
  useTabsStore().openMigration(connectionId, database);
}

export function openSavedMigration(m: SavedMigration) {
  useTabsStore().openMigration(m.connection_id, m.database, m.id);
}

export async function renameSavedMigration(m: SavedMigration) {
  const conns = useConnectionsStore();
  let value: string;
  try {
    ({ value } = await ElMessageBox.prompt(t('migration:saved.renamePrompt'), t('migration:saved.renameTitle'), {
      inputValue: m.name, confirmButtonText: t('common:rename'), cancelButtonText: t('common:cancel'),
      inputValidator: (v) => !!v?.trim() || t('core:actions.nameRequired'),
    }));
  } catch { return; }
  try { await conns.renameMigration(m.id, value.trim()); } catch (e) { ElMessage.error(errorMessage(e)); }
}

/** The same configuration as a new draft, opened in its own tab. */
export async function duplicateSavedMigration(m: SavedMigration) {
  const conns = useConnectionsStore();
  const names = new Set((conns.migrations[dbKey(m.connection_id, m.database)]?.items ?? []).map((x) => x.name));
  let name = t('core:actions.copyName', { name: m.name });
  for (let n = 2; names.has(name); n++) name = t('core:actions.copyNameN', { name: m.name, n });
  try {
    await migrationSaves.get(m.id);
    openSavedMigration(await conns.duplicateMigration(m.id, name));
  } catch (e) { ElMessage.error(errorMessage(e)); }
}

/** Only the entry goes: the source, the target and the runs' records stay. */
export async function deleteSavedMigration(m: SavedMigration) {
  const ok = await confirmNative(t('migration:saved.deleteConfirm', { name: m.name }), {
    title: t('migration:saved.deleteTitle'), okLabel: t('migration:saved.delete'),
  });
  if (!ok) return;
  const conns = useConnectionsStore();
  useTabsStore().closeWhere((x) => x.kind === 'migration' && x.migrationId === m.id);
  try {
    await migrationSaves.get(m.id);
    await conns.deleteMigration(m);
  } catch (e) { ElMessage.error(errorMessage(e)); }
}

export type SavedMigrationState = 'draft' | 'elsewhere' | 'running' | 'done' | 'failed' | 'interrupted' | 'cancelled';

/** Where an entry stands: its current run's status, or a draft. */
export function savedMigrationState(m: SavedMigration, runs: Record<string, RunInfo>): SavedMigrationState {
  const id = m.run_ids[m.run_ids.length - 1];
  if (!id) return 'draft';
  return runs[id]?.status ?? 'elsewhere';
}
