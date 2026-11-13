import { t as i18nT } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useScheduledStore } from '../stores/scheduled';
import { useServerJobsStore } from '../stores/serverJobs';
import { baseName, type NotebookTab, type SchedulerObjectTab, type Tab } from '../stores/tabs';

// An editor tab's title (the tab strip, "Ir a…"). `tr` is the translate
// function to use: the tab strip passes its reactive one, so titles follow
// a language switch.

type Translate = (key: string, options?: Record<string, unknown>) => string;

function scheduledTitle(id: string | null, tr: Translate): string {
  const scheduled = useScheduledStore();
  return `${tr('scheduled:tab')} · ${scheduled.byId(id)?.name || tr('scheduled:newTask')}`;
}

/** A scheduler object's tab: "Operador · DBA", or "Crear operador" while
 *  it's new ("Nuevo job" for a job, as the menus say). The kind's words come
 *  from the driver (Spanish: `tb`). */
export function schedulerObjectTitle(t: SchedulerObjectTab, tr: Translate = i18nT): string {
  const kind = useServerJobsStore().kind(t.connectionId, t.objectKind);
  const singular = kind ? tb(kind.singular) : t.objectKind;
  if (!t.objectId) return t.objectKind === 'job' ? tr('scheduled:server.newJob') : tr('scheduled:designer.newTitle', { kind: singular });
  return `${singular.charAt(0).toLocaleUpperCase() + singular.slice(1)} · ${t.title || t.objectId}`;
}

/** A notebook's name: its file's, or "Notebook N" while untitled. */
export function notebookTitle(t: NotebookTab, tr: Translate = i18nT): string {
  const file = t.path ?? t.projectPath;
  if (file) return file.split(/[\\/]/).pop() ?? file;
  return tr('notebook:untitled', { n: t.untitled ?? 1 });
}

export function tabTitle(t: Tab, tr: Translate = i18nT): string {
  const conns = useConnectionsStore();
  if (t.kind === 'notebook') return notebookTitle(t, tr);
  if (t.kind === 'object') return t.object.name;
  if (t.kind === 'file') return baseName(t.path);
  if (t.kind === 'fileDiff') return `${baseName(t.path)} ${tr('workbench:tabs.fileDiffSuffix')}`;
  if (t.kind === 'designer' && t.table) return tr('designer:alter.tab', { name: t.table.name });
  if (t.kind === 'designer') return conns.driverOf(t.connectionId)?.designer?.label ? tb(conns.driverOf(t.connectionId)!.designer!.label) : tr('workbench:tabs.designer');
  if (t.kind === 'monitor') return `${tr('workbench:tabs.monitor')} · ${conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'profiler') return `Profiler · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'connection') {
    if (t.editId) return `${tr('workbench:tabs.connection')} · ${conns.byId(t.editId)?.name ?? ''}`;
    return t.duplicateOf ? tr('workbench:tabs.duplicateConnection') : tr('workbench:tabs.newConnection');
  }
  if (t.kind === 'compare') return `${tr('workbench:tabs.compare')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'indexes') return `${tr('explorer:indexes.tab')} · ${t.object.name}`;
  if (t.kind === 'health') return `${tr('health:tab')} · ${t.database}`;
  if (t.kind === 'optimizer') return `${tr('optimizer:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'subset') return `${tr('subset:tab')} · ${t.object.name}`;
  if (t.kind === 'scheduledTask') return scheduledTitle(t.taskId, tr);
  if (t.kind === 'serverJob') return `${tr('scheduled:server.tab')} · ${t.jobName || t.jobId}`;
  if (t.kind === 'schedulerObject') return schedulerObjectTitle(t, tr);
  if (t.kind === 'schedulerLog') return `${tr('scheduled:log.tab')} · ${conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'search') return `${tr('search:tab')} · ${t.database}`;
  if (t.kind === 'dependencies') return `${tr('dependencies:tab')} · ${t.column ? `${t.object.name}.${t.column}` : t.object.name}`;
  if (t.kind === 'backups') return `${tr('backups:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'security') return `${tr('security:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'dataCompare') return `${tr('dataCompare:tab')} · ${t.object?.name || t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'context') return `${tr('ai:contexts.tab')} · ${conns.contextById(t.contextId)?.name ?? ''}`;
  if (t.kind === 'migration') {
    const saved = t.migrationId ? conns.migrationById(t.migrationId) : undefined;
    return `${tr('workbench:tabs.migrate')} · ${saved?.name || t.database || conns.byId(t.connectionId)?.name || ''}`;
  }
  if (t.kind === 'diagram') return `${tr('workbench:tabs.diagram')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'queryBuilder') return `${tr('queryBuilder:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind !== 'query') return '';
  const q = conns.queries[dbKey(t.connectionId, t.database)]?.items.find((x) => x.id === t.queryId);
  return q?.name ?? tr('workbench:tabs.query');
}
