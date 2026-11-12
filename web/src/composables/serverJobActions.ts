import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { jobsApi, type ServerJob } from '../api/jobs';
import { schedulerApi } from '../api/scheduler';
import type { MenuItem } from '../components/ContextMenu.vue';
import { t } from '../i18n';
import { confirmNative } from '../native';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { useTabsStore } from '../stores/tabs';
import { newQuery } from './actions';
import { canChange, deleteSchedulerObject } from './schedulerActions';
import { askStep } from './schedulerDialogs';

// What can be done with a server job (docs/server-jobs.md), from the
// sidebar's menu and from the job's tab. Everything that changes the server
// asks first, naming the job and the connection; read-only connections
// don't offer it (the backend refuses it anyway). Engines whose scheduler
// DBine manages whole (`capabilities.scheduler`) also offer Editar (the
// designer), Iniciar en el paso…, Borrar historial and Borrar.

type Action = 'run' | 'stop' | 'enable' | 'disable';

async function confirmed(connectionId: string, job: ServerJob, action: Action): Promise<boolean> {
  const connection = useConnectionsStore().byId(connectionId)?.name ?? '';
  return confirmNative(t(`scheduled:server.confirm.${action}`, { job: job.name, connection }), {
    title: t(`scheduled:server.${action}`),
    okLabel: t(`scheduled:server.${action}`),
    kind: action === 'run' || action === 'enable' ? 'info' : 'warning',
  });
}

/** Runs one change after the user's yes; reads the list again after it.
 *  True when it was done. */
async function change(connectionId: string, job: ServerJob, action: Action): Promise<boolean> {
  if (!(await confirmed(connectionId, job, action))) return false;
  try {
    if (action === 'run') await jobsApi.run(connectionId, job.id);
    else if (action === 'stop') await jobsApi.stop(connectionId, job.id);
    else await jobsApi.setEnabled(connectionId, job.id, action === 'enable');
    ElMessage.success(t(`scheduled:server.done.${action}`, { job: job.name }));
    void useServerJobsStore().load(connectionId);
    return true;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
}

export const runServerJob = (connectionId: string, job: ServerJob) => change(connectionId, job, 'run');
export const stopServerJob = (connectionId: string, job: ServerJob) => change(connectionId, job, 'stop');
export const toggleServerJob = (connectionId: string, job: ServerJob) =>
  change(connectionId, job, job.enabled ? 'disable' : 'enable');

/** The job's script in a new query of the connection. */
export async function openServerJobScript(connectionId: string, job: ServerJob) {
  try {
    const script = await jobsApi.script(connectionId, job.id);
    await newQuery(connectionId, '', script, t('scheduled:server.scriptName', { job: job.name }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

/** "Iniciar en el paso…": the job's steps to pick from, then the run from
 *  the one picked (the dialog names the job and the connection). True when
 *  the server took it. */
export async function runServerJobFrom(connectionId: string, job: ServerJob): Promise<boolean> {
  let steps;
  try {
    steps = (await jobsApi.detail(connectionId, job.id)).steps;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
  if (!steps.length) {
    ElMessage.info(t('scheduled:server.runFromDialog.noSteps', { job: job.name }));
    return false;
  }
  const connection = useConnectionsStore().byId(connectionId)?.name ?? '';
  const step = await askStep({
    title: t('scheduled:server.runFromDialog.title'),
    message: t('scheduled:server.runFromDialog.message', { job: job.name, connection }),
    steps: steps.map((s) => ({ name: s.name, kind: s.kind })),
    okLabel: t('scheduled:server.runFromDialog.start'),
  });
  if (!step) return false;
  try {
    await schedulerApi.runFrom(connectionId, job.id, step);
    ElMessage.success(t('scheduled:server.done.runFrom', { job: job.name, step }));
    void useServerJobsStore().load(connectionId);
    return true;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
}

/** "Borrar historial", after the user's yes. True when it was cleared. */
export async function clearServerJobHistory(connectionId: string, job: ServerJob): Promise<boolean> {
  const connection = useConnectionsStore().byId(connectionId)?.name ?? '';
  const ok = await confirmNative(t('scheduled:server.confirm.clearHistory', { job: job.name, connection }), {
    title: t('scheduled:server.clearHistory'),
    okLabel: t('scheduled:server.clearHistory'),
    kind: 'warning',
  });
  if (!ok) return false;
  try {
    await schedulerApi.clearHistory(connectionId, job.id);
    ElMessage.success(t('scheduled:server.done.clearHistory', { job: job.name }));
    void useServerJobsStore().load(connectionId);
    return true;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
}

/** "Borrar": the delete script, the user's yes, then the delete. */
export const deleteServerJob = (connectionId: string, job: ServerJob) =>
  deleteSchedulerObject(connectionId, 'job', job.id, job.name);

/** "Editar": the job in the designer. */
export function editServerJob(connectionId: string, job: ServerJob) {
  useTabsStore().openSchedulerObject(connectionId, 'job', job.id, job.name);
}

/** What a job's row and tab offer, with what its engine and the
 *  connection allow. */
export function serverJobActions(connectionId: string, job: ServerJob) {
  const writable = canChange(connectionId);
  const store = useServerJobsStore();
  const scheduler = store.hasScheduler(connectionId);
  const kind = store.kind(connectionId, 'job');
  return {
    run: writable && !!job.can_run,
    stop: writable && !!job.can_stop && !!job.running,
    toggle: writable && !!job.can_enable,
    script: !!job.has_script,
    edit: scheduler && (!kind || !!kind.can_edit),
    runFrom: scheduler && writable && !!job.can_run,
    clearHistory: scheduler && writable && !!job.has_history,
    delete: scheduler && writable && !!kind?.can_delete,
  };
}

/** The sidebar's context menu on a job. */
export function serverJobMenu(connectionId: string, job: ServerJob): MenuItem[] {
  const tabs = useTabsStore();
  const can = serverJobActions(connectionId, job);
  const items: MenuItem[] = [{ label: t('scheduled:open'), action: () => tabs.openServerJob(connectionId, job.id) }];
  if (can.run) items.push({ label: t('scheduled:server.run'), action: () => void runServerJob(connectionId, job) });
  if (can.runFrom) items.push({ label: t('scheduled:server.runFrom'), action: () => void runServerJobFrom(connectionId, job) });
  if (can.stop) items.push({ label: t('scheduled:server.stop'), action: () => void stopServerJob(connectionId, job) });
  if (can.toggle) {
    items.push({
      label: job.enabled ? t('scheduled:server.disable') : t('scheduled:server.enable'),
      action: () => void toggleServerJob(connectionId, job),
    });
  }
  if (can.edit) items.push({ label: t('scheduled:server.edit'), divided: true, action: () => editServerJob(connectionId, job) });
  if (can.script) items.push({ label: t('scheduled:server.script'), divided: !can.edit, action: () => void openServerJobScript(connectionId, job) });
  if (can.clearHistory) items.push({ label: t('scheduled:server.clearHistory'), divided: true, action: () => void clearServerJobHistory(connectionId, job) });
  if (can.delete) {
    items.push({ label: t('scheduled:server.delete'), danger: true, divided: !can.clearHistory, action: () => void deleteServerJob(connectionId, job) });
  }
  return items;
}
