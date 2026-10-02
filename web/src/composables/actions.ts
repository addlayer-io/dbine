import { ElMessage, ElMessageBox } from 'element-plus';
import { errorMessage } from '../api/client';
import type { SavedQuery } from '../api/types';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { confirmNative } from '../native';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { runTask } from '../stores/tasks';

// Actions reachable from several places (explorer menus, toolbars, tabs).
// Create / drop database and drop objects run as background tasks (no dialog
// stays open after the confirmation): they show in the Tareas panel and end
// with a notice. None of them has a cancel path in the backend, so the panel
// doesn't offer Cancelar.

/** New saved query under a database, opened in a tab. */
export async function newQuery(connectionId: string, database: string, sql = '', name?: string) {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  await conns.loadQueries(connectionId, database);
  try {
    const q = await conns.saveQuery({
      id: '', connection_id: connectionId, database, sql,
      name: name ?? conns.nextQueryName(connectionId, database),
      updated_at: '', last_run_at: null,
    });
    tabs.openQuery(q);
  } catch (e) {
    ElMessage.error(t('core:actions.createQueryFailed', { error: errorMessage(e) }));
  }
}

export async function renameQuery(q: SavedQuery) {
  const conns = useConnectionsStore();
  try {
    const { value } = await ElMessageBox.prompt(t('core:actions.renameQuery.prompt'), t('core:actions.renameQuery.title'), {
      inputValue: q.name, confirmButtonText: t('common:rename'), cancelButtonText: t('common:cancel'),
      inputValidator: (v) => !!v?.trim() || t('core:actions.nameRequired'),
    });
    await conns.saveQuery({ ...q, name: value.trim() });
  } catch { /* cancelled */ }
}

export async function duplicateQuery(q: SavedQuery) {
  const conns = useConnectionsStore();
  const names = new Set((conns.queries[dbKey(q.connection_id, q.database)]?.items ?? []).map((x) => x.name));
  let name = t('core:actions.copyName', { name: q.name });
  for (let n = 2; names.has(name); n++) name = t('core:actions.copyNameN', { name: q.name, n });
  await newQuery(q.connection_id, q.database, q.sql, name);
}

export async function deleteQuery(q: SavedQuery) {
  const ok = await confirmNative(t('core:actions.deleteQuery.confirm', { name: q.name }), {
    title: t('core:actions.deleteQuery.title'), okLabel: t('common:delete'),
  });
  if (!ok) return;
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  await conns.deleteQuery(q);
  tabs.closeWhere((t) => t.kind === 'query' && t.queryId === q.id);
}

/** A driver's create template as a new query ({schema} / {name} filled in). */
export async function newFromTemplate(connectionId: string, database: string, tpl: { kind: string; label: string; template: string }, schema: string | null) {
  const name = `${t('core:actions.newObjectPrefix')}${tpl.kind.replace(/[^a-z_]/gi, '_')}`;
  const sql = tpl.template.split('{schema}').join(schema ?? '').split('{name}').join(name)
    // "{schema}." with no schema leaves a stray dot.
    .replace(/(^|[\s(])\.(?=[\w"`[])/g, '$1');
  await newQuery(connectionId, database, sql, tpl.label);
}

/** Create a database on the connection's server (asks the name). */
export async function createDatabase(connectionId: string) {
  const conns = useConnectionsStore();
  let name: string;
  try {
    ({ value: name } = await ElMessageBox.prompt(t('core:actions.createDatabase.prompt'), t('core:actions.createDatabase.title'), {
      confirmButtonText: t('core:actions.create'), cancelButtonText: t('common:cancel'), inputValidator: (v) => !!v?.trim() || t('core:actions.nameRequired'),
    }));
  } catch { return; }
  const db = name.trim();
  const server = conns.byId(connectionId)?.name ?? '';
  runTask({
    kind: 'create-database',
    title: t('tasks:actions.createDatabase', { name: db, server }),
    connectionId, database: db, background: true,
    run: async (task) => {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('create_database', { args: { connection_id: connectionId, name: db } });
      // The database exists now: a failed refresh doesn't make the task fail.
      await conns.refreshDatabases(connectionId).catch((e) => task.log(errorMessage(e), 'warn'));
    },
  });
}

/** Drop tables / collections / views…: one asks for confirmation, several
 *  ask to type "eliminar". Objects others depend on are retried after them. */
export async function dropObjects(connectionId: string, database: string, objects: { kind: string; schema: string | null; name: string }[]) {
  if (!objects.length) return;
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const server = conns.byId(connectionId)?.name ?? '';
  const label = (o: { schema: string | null; name: string }) => (o.schema ? `${o.schema}.${o.name}` : o.name);
  const first = objects.slice(0, 12).map(label).join(', ');
  const list = objects.length > 12 ? t('core:actions.dropObjects.more', { list: first, count: objects.length - 12 }) : first;
  const word = t('core:actions.dropObjects.word');
  const where = `${server}${database ? ` · ${database}` : ''}`;
  try {
    if (objects.length === 1) {
      await ElMessageBox.confirm(t('core:actions.dropObjects.confirmOne', { name: label(objects[0]), where }), t('common:delete'), {
        confirmButtonText: t('common:delete'), cancelButtonText: t('common:cancel'), type: 'error', confirmButtonClass: 'el-button--danger',
      });
    } else {
      await ElMessageBox.prompt(
        t('core:actions.dropObjects.confirmMany', { count: objects.length, where, list, word }),
        t('core:actions.dropObjects.titleMany', { count: objects.length }),
        {
          confirmButtonText: t('common:delete'), cancelButtonText: t('common:cancel'), type: 'error', confirmButtonClass: 'el-button--danger',
          inputValidator: (v) => v.trim().toLowerCase() === word.toLowerCase() || t('core:actions.dropObjects.typeWord', { word }),
        },
      );
    }
  } catch { return; }
  type Dropped = { dropped: typeof objects; errors: [typeof objects[number], string][] };
  runTask<Dropped>({
    kind: 'drop-objects',
    title: objects.length === 1
      ? t('tasks:actions.dropOne', { name: label(objects[0]), db: database || server })
      : t('tasks:actions.dropMany', { count: objects.length, db: database || server }),
    connectionId, database, background: true,
    run: async (task) => {
      task.progress({ done: 0, total: objects.length, unit: 'objects' });
      // Per object, as the backend settles each one (typed here: the
      // command is invoked directly, not through api/).
      type DropProgress = { id: string; done: number; total: number };
      const id = `drop-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      await task.listen<DropProgress>('drop-objects-progress', (e) => {
        if (e.payload.id === id) task.progress({ done: e.payload.done, total: e.payload.total, unit: 'objects' });
      });
      const { invoke } = await import('@tauri-apps/api/core');
      const r = await invoke<Dropped>('drop_objects', {
        args: { connection_id: connectionId, database, objects, id },
      });
      task.progress({ done: r.dropped.length + r.errors.length, total: objects.length, unit: 'objects' });
      for (const [o, e] of r.errors) task.log(`${label(o)}: ${tb(e)}`, 'error');
      const gone = new Set(r.dropped.map((o) => `${o.kind}\u0000${o.schema ?? ''}\u0000${o.name}`));
      tabs.closeWhere((t) => t.kind === 'object' && t.connectionId === connectionId && t.database === database
        && gone.has(`${t.object.kind}\u0000${t.object.schema ?? ''}\u0000${t.object.name}`));
      await conns.loadObjects(connectionId, database, true).catch((e) => task.log(errorMessage(e), 'warn'));
      return r;
    },
    summary: (r) => (r.errors.length
      ? t('tasks:actions.dropFailed', { failed: r.errors.length, total: objects.length })
      : t('tasks:actions.dropped', { count: r.dropped.length })),
    outcome: (r) => (r.errors.length ? 'error' : 'done'),
  });
}

/** Drop a database: the user has to type its name to confirm. */
export async function dropDatabase(connectionId: string, database: string) {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const server = conns.byId(connectionId)?.name ?? '';
  try {
    await ElMessageBox.prompt(
      t('core:actions.dropDatabase.confirm', { name: database, server }),
      t('core:actions.dropDatabase.title'),
      {
        confirmButtonText: t('common:delete'), cancelButtonText: t('common:cancel'), type: 'error',
        confirmButtonClass: 'el-button--danger',
        inputValidator: (v) => v === database || t('core:actions.dropDatabase.mismatch'),
      },
    );
  } catch { return; }
  runTask({
    kind: 'drop-database',
    title: t('tasks:actions.dropDatabase', { name: database, server }),
    connectionId, database, background: true,
    run: async (task) => {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('drop_database', { args: { connection_id: connectionId, name: database } });
      tabs.closeWhere((t) => t.connectionId === connectionId && t.database === database);
      await conns.refreshDatabases(connectionId).catch((e) => task.log(errorMessage(e), 'warn'));
    },
  });
}
