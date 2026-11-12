import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { schedulerApi, type SchedulerItem, type SchedulerKind } from '../api/scheduler';
import type { MenuItem } from '../components/ContextMenu.vue';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { useTabsStore } from '../stores/tabs';
import { newQuery } from './actions';
import { askScript } from './schedulerDialogs';
import { formValues, initialLists, initialValues } from './schedulerForm';
import { isProdConnection } from './tags';

// What can be done with the objects of a server's scheduler (the designer,
// docs/server-jobs.md) from the sidebar's tree, a job's tab and the
// designer. Every change asks first, showing the script that will run and
// naming the object and the connection; read-only connections don't offer
// changes (the backend refuses them anyway).

/** The connection isn't read-only: changes are offered. */
export function canChange(connectionId: string): boolean {
  return !useConnectionsStore().byId(connectionId)?.config.read_only;
}

/** The driver's SQL dialect, to color scripts with. */
export function dialectOf(connectionId: string): string {
  return useConnectionsStore().driverOf(connectionId)?.dialect ?? '';
}

/** The production notice for a confirmation, or null. */
export function prodWarning(connectionId: string): string | null {
  const conn = useConnectionsStore().byId(connectionId);
  return isProdConnection(conn) ? t('scheduled:designer.prodWarning', { connection: conn?.name ?? '' }) : null;
}

/** A kind's singular in the UI's language ("operador"), or its key. */
export function singularOf(connectionId: string, kind: string): string {
  const k = useServerJobsStore().kind(connectionId, kind);
  return k ? tb(k.singular) : kind;
}

/** "Crear <singular>" in the tree and the designer is offered. */
export function canCreate(connectionId: string, kind: SchedulerKind | undefined): boolean {
  return !!kind?.can_create && canChange(connectionId);
}

/** The object's CREATE script in a new query of the connection: its form as
 *  the server has it, written as a new object. */
export async function openObjectScript(connectionId: string, kind: string, id: string, name: string) {
  try {
    const form = await schedulerApi.form(connectionId, kind, id);
    const values = formValues(form, initialValues(form, false), initialLists(form));
    const script = await schedulerApi.script(connectionId, kind, null, values);
    await newQuery(connectionId, '', script, t('scheduled:server.scriptName', { job: name }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

/** Deletes an object after showing its script and the user's yes; closes
 *  its tabs and reads the tree again. True when it was deleted. */
export async function deleteSchedulerObject(connectionId: string, kind: string, id: string, name: string): Promise<boolean> {
  const conns = useConnectionsStore();
  let script: string;
  try {
    script = await schedulerApi.deleteScript(connectionId, kind, id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
  const ok = await askScript({
    mode: 'confirm',
    title: t('scheduled:designer.deleteTitle'),
    message: t('scheduled:designer.confirmDelete', { name, kind: singularOf(connectionId, kind), connection: conns.byId(connectionId)?.name ?? '' }),
    warning: prodWarning(connectionId),
    script,
    dialect: dialectOf(connectionId),
    okLabel: t('scheduled:server.delete'),
    danger: true,
  });
  if (!ok) return false;
  try {
    await schedulerApi.delete(connectionId, kind, id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
  ElMessage.success(t('scheduled:designer.deleted', { name }));
  useTabsStore().closeWhere((x) => x.connectionId === connectionId && (
    (x.kind === 'schedulerObject' && x.objectKind === kind && x.objectId === id)
    || (kind === 'job' && x.kind === 'serverJob' && x.jobId === id)));
  useServerJobsStore().refreshAfterChange(connectionId);
  return true;
}

/** The tree's context menu on one object of a kind. */
export function schedulerItemMenu(connectionId: string, kind: SchedulerKind, item: SchedulerItem): MenuItem[] {
  const tabs = useTabsStore();
  const items: MenuItem[] = [
    { label: t('common:open'), action: () => tabs.openSchedulerObject(connectionId, kind.key, item.id, item.name) },
    { label: t('scheduled:server.script'), action: () => void openObjectScript(connectionId, kind.key, item.id, item.name) },
  ];
  if (kind.can_delete && canChange(connectionId)) {
    items.push({ label: t('scheduled:server.delete'), danger: true, divided: true, action: () => void deleteSchedulerObject(connectionId, kind.key, item.id, item.name) });
  }
  return items;
}

/** The tree's context menu on a kind's node. */
export function schedulerKindMenu(connectionId: string, kind: SchedulerKind): MenuItem[] {
  const store = useServerJobsStore();
  const items: MenuItem[] = [];
  if (canCreate(connectionId, kind)) {
    items.push({
      label: kind.key === 'job' ? t('scheduled:server.newJob') : t('scheduled:designer.newTitle', { kind: tb(kind.singular) }),
      action: () => useTabsStore().openSchedulerObject(connectionId, kind.key, null),
    });
  }
  items.push({
    label: t('common:refresh'),
    divided: items.length > 0,
    action: () => void (kind.key === 'job' ? store.load(connectionId) : store.loadItems(connectionId, kind.key)),
  });
  return items;
}
