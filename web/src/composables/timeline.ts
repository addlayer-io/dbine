import { nextTick, ref } from 'vue';
import { api } from '../api/client';
import { projectsApi } from '../api/projects';
import { timelineApi } from '../api/timeline';
import { editorBridge } from '../stores/ai';
import type { Tab } from '../stores/tabs';
import { queryDocs } from './tabDocument';

// The history sidebar's "Esta pestaña" view (docs/history.md): the active
// tab's timeline, as in VS Code. A saved query has local versions and runs;
// a project file has runs and git commits.

/** Bumped after a save or a restore: the timeline reloads. */
export const timelineSeq = ref(0);

export type TimelineSource =
  | { kind: 'query'; tabId: string; queryId: string; connectionId: string; database: string }
  | { kind: 'file'; tabId: string; projectId: string; path: string; connectionId: string; database: string };

/** What the tab's timeline shows; `null` for tabs without one. */
export function timelineSource(tab: Tab | null): TimelineSource | null {
  if (!tab) return null;
  if (tab.kind === 'query') return { kind: 'query', tabId: tab.id, queryId: tab.queryId, connectionId: tab.connectionId, database: tab.database };
  if (tab.kind === 'file') {
    return { kind: 'file', tabId: tab.id, projectId: tab.projectId, path: tab.path, connectionId: tab.connectionId, database: tab.database };
  }
  return null;
}

export const sourceKey = (s: TimelineSource | null) =>
  !s ? '' : s.kind === 'query' ? `q:${s.queryId}` : `f:${s.projectId}\u0000${s.path}`;

/** The tab's text now: the editor's (with what isn't saved yet), else the
 *  saved query's or the file's on disk. */
export async function currentText(s: TimelineSource): Promise<string> {
  const bridge = editorBridge(s.tabId);
  if (bridge) return bridge.text();
  if (s.kind === 'query') return (await api.getQuery(s.queryId)).sql;
  return (await projectsApi.readFile(s.projectId, s.path)).text;
}

/** Put an older text in the tab's editor (undoable there). A saved query
 *  keeps its current text as a version first and then saves the restored
 *  one; a file's tab is left with changes to save (⌘S). False when the tab
 *  has no editor open. */
export async function restoreText(s: TimelineSource, text: string): Promise<boolean> {
  const bridge = editorBridge(s.tabId);
  if (!bridge) return false;
  if (s.kind === 'query') {
    const doc = queryDocs.get(s.tabId);
    if (doc) await doc.save();
    await timelineApi.checkpoint(s.queryId).catch(() => null);
    bridge.replace(text);
    await nextTick();
    if (doc) await doc.save();
  } else {
    bridge.replace(text);
  }
  timelineSeq.value++;
  return true;
}
