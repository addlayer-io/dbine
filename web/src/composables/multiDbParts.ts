import type { MultiDbResponse, StatementResult } from '../api/types';

// The result sets of a run on several databases ("Ejecutar en varias
// bases…", multiDb.ts), in the order the results pane shows them, and what
// exporting each one whole runs again. No Vue or Tauri here: a node test
// checks it.

/** What "Exportar todo" runs again for a result of a multi-database run:
 *  the script on these databases, one after the other, writing each one's
 *  `set`-th result set with columns; `base`: after its database's name, as
 *  the merged grid shows it (the backend's `DatabasesExport`). */
export interface MultiDbExport {
  databases: string[];
  set: number;
  base: boolean;
}

/** One result set of the run. `database`: null for the merged grid.
 *  `index` / `count`: its place among its database's own result sets.
 *  `export`: null for a database that didn't end well (running the script
 *  again there wouldn't give the rows its tab shows). */
export interface MultiDbPart {
  result: StatementResult;
  database: string | null;
  index: number;
  count: number;
  export: MultiDbExport | null;
}

export function multiDbParts(r: MultiDbResponse): MultiDbPart[] {
  const parts: MultiDbPart[] = [];
  if (r.merged) {
    // The grid has the first result set of every database that ended well.
    const ok = r.databases.filter((d) => d.status === 'ok').map((d) => d.database);
    parts.push({ result: r.merged, database: null, index: 0, count: 1, export: { databases: ok, set: 0, base: true } });
  }
  for (const d of r.databases) {
    const sets = d.results.filter((x) => x.columns.length);
    // The merged grid took an OK database's first one.
    const before = r.merged && d.status === 'ok' ? 1 : 0;
    sets.forEach((s, i) => parts.push({
      result: s, database: d.database, index: i, count: sets.length,
      export: d.status === 'ok' ? { databases: [d.database], set: i + before, base: false } : null,
    }));
  }
  return parts;
}
