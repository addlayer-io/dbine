// Drag and drop in the explorer: where a drop lands and the new order of
// that level. Pure, so it can be reasoned about (and tried) without the tree.
//
// Each level shows its folders first, then its connections; a drop reorders
// one kind at one level and never mixes them.

export type ExplorerKind = 'connection' | 'folder';
export type DropZone = 'before' | 'after' | 'into';

export interface DragItem { kind: ExplorerKind; id: string }
/** What the pointer is over: a row, or the header / empty area (`root`). */
export type DropOn = { kind: ExplorerKind; id: string } | 'root';

interface FolderLike { id: string; parent_id: string | null }
interface ConnectionLike { id: string; folder_id: string | null }

export interface ReorderPlan {
  kind: ExplorerKind;
  /** The level; null = top level. */
  parentId: string | null;
  /** Every item of that kind at that level, in the new order. */
  ids: string[];
}

/** The zone of a row under the pointer: the top quarter inserts before, the
 *  bottom quarter after, the middle goes into (folders only; on a connection
 *  the middle splits in halves). */
export function dropZone(offsetY: number, height: number, isFolder: boolean): DropZone {
  const h = height > 0 ? height : 1;
  const y = offsetY / h;
  if (isFolder) return y < 0.25 ? 'before' : y > 0.75 ? 'after' : 'into';
  return y < 0.5 ? 'before' : 'after';
}

/** The level a connection shows at: its folder, or the top level when the folder is gone. */
function connectionLevel(c: ConnectionLike, folders: FolderLike[]): string | null {
  return c.folder_id && folders.some((f) => f.id === c.folder_id) ? c.folder_id : null;
}

/** The ordered list of siblings a drop produces, or null when the drop does
 *  nothing (or isn't allowed: a folder inside its own subtree). */
export function planDrop(
  folders: FolderLike[],
  connections: ConnectionLike[],
  dragged: DragItem,
  on: DropOn,
  zone: DropZone,
): ReorderPlan | null {
  if (on !== 'root' && on.kind === dragged.kind && on.id === dragged.id) return null;

  // The level and, when the target is a sibling of the same kind, where next to it.
  let parentId: string | null;
  let anchor: { id: string; after: boolean } | null = null;
  let atStart = false;
  if (on === 'root') {
    parentId = null;
  } else if (on.kind === 'folder' && zone === 'into') {
    parentId = on.id;
  } else {
    const after = zone === 'after';
    if (on.kind === 'folder') {
      const f = folders.find((x) => x.id === on.id);
      if (!f) return null;
      parentId = f.parent_id ?? null;
      // A connection dropped among folders lands first among the connections.
      if (dragged.kind === 'folder') anchor = { id: on.id, after };
      else atStart = true;
    } else {
      const c = connections.find((x) => x.id === on.id);
      if (!c) return null;
      parentId = connectionLevel(c, folders);
      // A folder dropped among connections lands last among the folders.
      if (dragged.kind === 'connection') anchor = { id: on.id, after };
    }
  }

  if (dragged.kind === 'folder' && parentId) {
    // Not inside itself or one of its descendants.
    for (let at: string | null = parentId; at; at = folders.find((f) => f.id === at)?.parent_id ?? null) {
      if (at === dragged.id) return null;
    }
  }

  const current = dragged.kind === 'folder'
    ? folders.filter((f) => (f.parent_id ?? null) === parentId).map((f) => f.id)
    : connections.filter((c) => connectionLevel(c, folders) === parentId).map((c) => c.id);
  const ids = current.filter((id) => id !== dragged.id);
  let at = ids.length;
  if (atStart) at = 0;
  else if (anchor) {
    const i = ids.indexOf(anchor.id);
    if (i >= 0) at = anchor.after ? i + 1 : i;
  }
  ids.splice(at, 0, dragged.id);

  if (ids.length === current.length && ids.every((id, i) => id === current[i])) return null;
  return { kind: dragged.kind, parentId, ids };
}
