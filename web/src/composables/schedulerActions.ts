import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { schedulerApi, type FormValues, type ObjectForm, type SchedulerAction, type SchedulerItem, type SchedulerKind } from '../api/scheduler';
import type { MenuItem } from '../components/ContextMenu.vue';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { useTabsStore } from '../stores/tabs';
import { newQuery } from './actions';
import { askForm, askScript } from './schedulerDialogs';
import { actionText, formValues, initialLists, initialValues, itemAbilities, type Abilities } from './schedulerForm';
import { isProdConnection } from './tags';

// What can be done with the objects of a server's scheduler (the designer,
// docs/server-jobs.md) from the sidebar's tree, a job's tab and the
// designer. Every change asks first, showing the script that will run and
// naming the object and the connection; read-only connections don't offer
// changes (the backend refuses them anyway). Each object offers what it says
// it can (`itemAbilities`): its own edit / delete flags, else its kind's, and
// the kind's actions it lists. An action that asks for values first (a time
// range, job parameters, a chain's step) opens its form before the script.

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

/** What an existing object offers (`itemAbilities`) as the last read of
 *  its kind's objects has it, or null while that list hasn't been read (or
 *  its read failed): nothing is offered on a guess. One missing from the
 *  list read (created since, past the list's cut) offers what its kind says. */
export function objectAbilities(connectionId: string, kind: SchedulerKind | undefined, id: string): Abilities | null {
  const list = kind ? useServerJobsStore().scheduler[connectionId]?.items[kind.key]?.list : null;
  return kind && list ? itemAbilities(kind, list.items.find((i) => i.id === id)) : null;
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

/** An action's label in the UI's language, with "…" when it asks for
 *  values first (menus, buttons). */
export function actionLabel(action: SchedulerAction): string {
  return actionText(tb(action.label), action.params);
}

/** One of the kind's actions on an object (run now, open a window…): its
 *  script, the user's yes naming the object and the connection (and the
 *  production notice), then the action; the tree reads again and an open
 *  designer of the object too. One that asks for values first
 *  (`SchedulerAction.params`) shows its form for the object before: OK
 *  writes the script with those values, a "no" to it goes back to the form,
 *  and an error from the script or the run shows in the form with the
 *  values kept for a retry. Never on a read-only connection (the backend
 *  refuses the run anyway). True when it ran. */
export async function runSchedulerAction(connectionId: string, kind: SchedulerKind, id: string, name: string, action: SchedulerAction): Promise<boolean> {
  if (!canChange(connectionId)) return false;
  const label = tb(action.label);
  const what = { action: label, name, kind: tb(kind.singular), connection: useConnectionsStore().byId(connectionId)?.name ?? '' };
  /** The script for these values, the user's yes, the run: false when the
   *  user said no. Errors throw. */
  const confirmAndRun = async (values?: FormValues): Promise<boolean> => {
    const script = await schedulerApi.actionScript(connectionId, kind.key, id, action.key, values);
    const ok = await askScript({
      mode: 'confirm',
      title: label,
      message: t('scheduled:designer.confirmAction', what),
      warning: prodWarning(connectionId),
      script,
      dialect: dialectOf(connectionId),
      okLabel: label,
    });
    if (!ok) return false;
    await schedulerApi.action(connectionId, kind.key, id, action.key, values);
    return true;
  };
  let done: boolean;
  if (action.params) {
    let form: ObjectForm;
    try {
      form = await schedulerApi.actionForm(connectionId, kind.key, id, action.key);
    } catch (e) {
      ElMessage.error(errorMessage(e));
      return false;
    }
    done = await askForm({
      title: label,
      message: t('scheduled:designer.actionForm', what),
      form,
      okLabel: label,
      dialect: dialectOf(connectionId),
      submit: confirmAndRun,
    });
  } else {
    try {
      done = await confirmAndRun();
    } catch (e) {
      ElMessage.error(errorMessage(e));
      return false;
    }
  }
  if (!done) return false;
  ElMessage.success(t('scheduled:designer.actionDone', { action: label, name }));
  const store = useServerJobsStore();
  store.refreshAfterChange(connectionId);
  store.objectChanged(connectionId, kind.key, id);
  return true;
}

/** Menu entries for the kind's actions an object offers (none on a
 *  read-only connection). */
export function actionMenuItems(connectionId: string, kind: SchedulerKind, actions: SchedulerAction[], id: string, name: string): MenuItem[] {
  if (!canChange(connectionId)) return [];
  return actions.map((a, i) => ({
    label: actionLabel(a), divided: i === 0, action: () => void runSchedulerAction(connectionId, kind, id, name, a),
  }));
}

/** The tree's context menu on one object of a kind. */
export function schedulerItemMenu(connectionId: string, kind: SchedulerKind, item: SchedulerItem): MenuItem[] {
  const tabs = useTabsStore();
  const can = itemAbilities(kind, item);
  const items: MenuItem[] = [
    { label: t('common:open'), action: () => tabs.openSchedulerObject(connectionId, kind.key, item.id, item.name) },
    { label: t('scheduled:server.script'), action: () => void openObjectScript(connectionId, kind.key, item.id, item.name) },
    ...actionMenuItems(connectionId, kind, can.actions, item.id, item.name),
  ];
  if (can.delete && canChange(connectionId)) {
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
