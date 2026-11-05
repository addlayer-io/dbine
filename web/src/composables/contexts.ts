import { nextTick } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { errorMessage } from '../api/client';
import type { SavedContext } from '../api/contexts';
import { t } from '../i18n';
import { confirmNative } from '../native';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// The "Contextos" node's actions (explorer menus).

/** The last save per context (a tab's autosave), so deleting waits for it:
 *  a save that lands after the delete would bring the context back. */
export const contextSaves = new Map<string, Promise<unknown>>();

/** The setting with the text for every chat (Settings › AI assistant). */
export const GLOBAL_INSTRUCTIONS = 'ai.instructions';

async function askName(title: string, value: string): Promise<string | null> {
  try {
    const r = await ElMessageBox.prompt(t('ai:contexts.namePrompt'), title, {
      inputValue: value, confirmButtonText: t('common:save'), cancelButtonText: t('common:cancel'),
      inputValidator: (v) => !!v?.trim() || t('core:actions.nameRequired'),
    });
    return r.value.trim();
  } catch { return null; }
}

export async function newContext(connectionId: string, database: string) {
  const name = await askName(t('ai:contexts.new'), '');
  if (!name) return;
  try {
    const saved = await useConnectionsStore().saveContext({ id: '', connection_id: connectionId, database, name, body: '', active: true, updated_at: '' });
    useTabsStore().openContext(saved);
  } catch (e) { ElMessage.error(errorMessage(e)); }
}

export async function renameContext(x: SavedContext) {
  const name = await askName(t('ai:contexts.renameTitle'), x.name);
  if (!name) return;
  try { await useConnectionsStore().saveContext({ ...x, name }); } catch (e) { ElMessage.error(errorMessage(e)); }
}

export async function toggleContext(x: SavedContext) {
  try { await useConnectionsStore().saveContext({ ...x, active: !x.active }); } catch (e) { ElMessage.error(errorMessage(e)); }
}

export async function deleteContext(x: SavedContext) {
  const ok = await confirmNative(t('ai:contexts.deleteConfirm', { name: x.name }), {
    title: t('ai:contexts.deleteTitle'), okLabel: t('common:delete'),
  });
  if (ok) await removeContext(x);
}

/** Closes its tabs and deletes it. */
export async function removeContext(x: SavedContext) {
  useTabsStore().closeWhere((tab) => tab.kind === 'context' && tab.contextId === x.id);
  try {
    // Closing the tab saves what it hadn't saved yet.
    await nextTick();
    await contextSaves.get(x.id);
    contextSaves.delete(x.id);
    await useConnectionsStore().deleteContext(x);
  } catch (e) { ElMessage.error(errorMessage(e)); }
}
