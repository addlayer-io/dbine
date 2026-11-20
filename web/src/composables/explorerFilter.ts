// The explorer's filter beyond the loaded folders: where what it found goes
// in the tree, and the code search's hits by object. The tree reads folder by
// folder, so the filter also looks for the text in the explorer cache and on
// the server (names) and, with "En el código", in the objects' definitions;
// what it finds shows in its kind folder like the loaded objects.

import type { DbObject } from '../api/types';

/** A tree folder: a schema's objects of one kind (`schema` null: of every
 *  schema; `noSchema`: those without one; `kind` null: any kind; `other`:
 *  the kinds that aren't these). As `FolderSpec` in stores/connections.ts. */
export interface FolderOf {
  schema: string | null;
  noSchema?: boolean;
  kind: string | null;
  other?: string[];
}

/** One line of an object's definition where the text is (`search_database`). */
export interface CodeHit {
  kind: string;
  schema: string | null;
  name: string;
  parent?: string | null;
  line: number;
  text: string;
}

export const objectId = (o: { kind: string; schema: string | null; name: string }) => `${o.kind}\u0000${o.schema ?? ''}\u0000${o.name}`;

/** Whether `o` belongs in folder `f`. */
export function inFolder(o: DbObject, f: FolderOf): boolean {
  if (f.noSchema) {
    if (o.schema != null) return false;
  } else if (f.schema != null && o.schema !== f.schema) {
    return false;
  }
  if (f.other) return !f.other.includes(o.kind);
  return f.kind == null || o.kind === f.kind;
}

/** What `found` adds to folder `f`: the objects that belong in it and aren't
 *  among `have` (its loaded ones), each once. */
export function foundFor(found: DbObject[], f: FolderOf, have: DbObject[]): DbObject[] {
  const seen = new Set(have.map(objectId));
  const out: DbObject[] = [];
  for (const o of found) {
    if (!inFolder(o, f)) continue;
    const id = objectId(o);
    if (seen.has(id)) continue;
    seen.add(id);
    out.push(o);
  }
  return out;
}

/** The code search's hits as objects (each once, in order) and each one's
 *  first matching line. */
export function codeObjects(hits: CodeHit[]): { objects: DbObject[]; lines: Map<string, { line: number; text: string }> } {
  const objects: DbObject[] = [];
  const lines = new Map<string, { line: number; text: string }>();
  for (const h of hits) {
    const id = objectId(h);
    if (lines.has(id)) continue;
    lines.set(id, { line: h.line, text: h.text });
    objects.push({ kind: h.kind, schema: h.schema, name: h.name, parent: h.parent ?? null });
  }
  return { objects, lines };
}
