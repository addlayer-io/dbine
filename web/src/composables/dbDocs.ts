import { h, ref } from 'vue';
import { ElButton, ElMessage, ElNotification } from 'element-plus';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { errorMessage } from '../api/client';
import { dbDocsApi, type DocOptions, type DocsProgress, type Generated } from '../api/dbDocs';
import { useConnectionsStore } from '../stores/connections';
import { runTask } from '../stores/tasks';

// "Documentar la base…": the database the dialog is open for (the explorer
// sets it, DbDocsDialog.vue reads it), and the run as a task with progress
// and cancel. It ends with a notice that opens the file or its folder.

export const dbDocsTarget = ref<{ connectionId: string; database: string } | null>(null);

export function openDbDocs(connectionId: string, database: string) {
  dbDocsTarget.value = { connectionId, database };
}

async function openFile(path: string, reveal: boolean) {
  try {
    await dbDocsApi.open(path, reveal);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

function notifyReady(name: string, path: string) {
  const n = ElNotification({
    type: 'success',
    title: t('dbDocs:ready', { name }),
    duration: 0,
    message: h('div', [
      h('div', { style: 'word-break: break-all; margin-bottom: 8px; font-size: 12px' }, path),
      h('div', [
        h(ElButton, { size: 'small', type: 'primary', onClick: () => { void openFile(path, false); n.close(); } }, () => t('dbDocs:open')),
        h(ElButton, { size: 'small', onClick: () => { void openFile(path, true); n.close(); } }, () => t('dbDocs:reveal')),
      ]),
    ]),
  });
}

export function runDbDocs(connectionId: string, database: string, path: string, options: DocOptions) {
  const runId = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  const name = database || useConnectionsStore().byId(connectionId)?.name || '';
  const { promise } = runTask<Generated>({
    kind: 'dbdocs',
    title: t('dbDocs:task', { name }),
    connectionId, database,
    cancel: () => dbDocsApi.cancel(runId),
    run: async (task) => {
      await task.listen<DocsProgress>('dbdocs-progress', ({ payload }) => {
        if (payload.run_id !== runId) return;
        task.progress({ done: payload.done, total: payload.total, unit: 'objects', phase: t(`dbDocs:phase.${payload.phase}`) });
      });
      const r = await dbDocsApi.generate(connectionId, database, runId, path, options);
      for (const note of r.notes) task.log(tb(note), 'warn');
      return r;
    },
    summary: (r) => t('dbDocs:summary', { tables: r.tables, objects: r.objects }),
  });
  promise.then((r) => notifyReady(name, r.path)).catch(() => { /* the task shows it */ });
}
