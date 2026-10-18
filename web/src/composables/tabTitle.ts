import { t as i18nT } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useScheduledStore } from '../stores/scheduled';
import { baseName, type Tab } from '../stores/tabs';

// An editor tab's title (the tab strip, "Ir a…"). `tr` is the translate
// function to use: the tab strip passes its reactive one, so titles follow
// a language switch.

type Translate = (key: string, options?: Record<string, unknown>) => string;

function scheduledTitle(id: string | null, tr: Translate): string {
  const scheduled = useScheduledStore();
  return `${tr('scheduled:tab')} · ${scheduled.byId(id)?.name || tr('scheduled:newTask')}`;
}

export function tabTitle(t: Tab, tr: Translate = i18nT): string {
  const conns = useConnectionsStore();
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
  if (t.kind === 'search') return `${tr('search:tab')} · ${t.database}`;
  if (t.kind === 'dependencies') return `${tr('dependencies:tab')} · ${t.column ? `${t.object.name}.${t.column}` : t.object.name}`;
  if (t.kind === 'backups') return `${tr('backups:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'security') return `${tr('security:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'dataCompare') return `${tr('dataCompare:tab')} · ${t.object?.name || t.database || conns.byId(t.connectionId)?.name || ''}`;
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
