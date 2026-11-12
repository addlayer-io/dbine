import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { jobsApi, type ServerJob } from '../api/jobs';
import type { MenuItem } from '../components/ContextMenu.vue';
import { t } from '../i18n';
import { confirmNative } from '../native';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { useTabsStore } from '../stores/tabs';
import { newQuery } from './actions';

// What can be done with a server job (docs/server-jobs.md), from the
// sidebar's menu and from the job's tab. Everything that changes the server
// asks first, naming the job and the connection; read-only connections
// don't offer it (the backend refuses it anyway).

type Action = 'run' | 'stop' | 'enable' | 'disable';

function canChange(connectionId: string): boolean {
  return !useConnectionsStore().byId(connectionId)?.config.read_only;
}

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

/** What a job's row and tab offer, with what its engine and the
 *  connection allow. */
export function serverJobActions(connectionId: string, job: ServerJob) {
  const writable = canChange(connectionId);
  return {
    run: writable && !!job.can_run,
    stop: writable && !!job.can_stop && !!job.running,
    toggle: writable && !!job.can_enable,
    script: !!job.has_script,
  };
}

/** The sidebar's context menu on a job. */
export function serverJobMenu(connectionId: string, job: ServerJob): MenuItem[] {
  const tabs = useTabsStore();
  const can = serverJobActions(connectionId, job);
  const items: MenuItem[] = [{ label: t('scheduled:open'), action: () => tabs.openServerJob(connectionId, job.id) }];
  if (can.run) items.push({ label: t('scheduled:server.run'), action: () => void runServerJob(connectionId, job) });
  if (can.stop) items.push({ label: t('scheduled:server.stop'), action: () => void stopServerJob(connectionId, job) });
  if (can.toggle) {
    items.push({
      label: job.enabled ? t('scheduled:server.disable') : t('scheduled:server.enable'),
      action: () => void toggleServerJob(connectionId, job),
    });
  }
  if (can.script) items.push({ label: t('scheduled:server.script'), divided: true, action: () => void openServerJobScript(connectionId, job) });
  return items;
}
