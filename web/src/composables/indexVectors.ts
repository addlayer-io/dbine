import { reactive } from 'vue';
import type { IndexUsage } from '../api/types';
import type { VectorIndex } from '../api/schema-types';
import { findTable, relationsSchema } from './fkJump';
import { objKey, useConnectionsStore } from '../stores/connections';
import { indexableVectorType, vectorIndexTag } from './vector';
import { metricShortLabel, vectorIndexLine } from './vectorLabels';

// A table's vector indexes (algorithm, distance, build parameters) for the
// views that list indexes from their usage (the explorer's "Índices"
// folder, the "Índices" tab, the drop dialog). `IndexUsage.vector` when
// the driver reports it; else (a driver that doesn't fill it yet) the
// table's schema (`database_schema`, cached a few minutes per database and
// shared with the grid's foreign-key jumps), read only for engines with
// vectors and tables that may have a vector column.

type TableRef = { schema?: string | null; name: string };
const keyOf = (connectionId: string, database: string, table: TableRef) =>
  [connectionId, database, table.schema ?? '', table.name].join('\u0001');

/** By table: each vector index's name and how it searches. */
const found = reactive<Record<string, Record<string, VectorIndex>>>({});
const asked = new Set<string>();

/** The index's vector index, or null (not one, or not known yet). */
export function indexVector(connectionId: string, database: string, table: TableRef, index: IndexUsage | string): VectorIndex | null {
  if (typeof index !== 'string' && index.vector) return index.vector;
  const name = typeof index === 'string' ? index : index.name;
  return found[keyOf(connectionId, database, table)]?.[name] ?? null;
}

/** "HNSW · Coseno", "IVFFLAT · L2", "DISKANN · Coseno": an index's tag
 *  with its algorithm and distance, short, for the explorer. */
export const vectorTag = (tag: string, v: VectorIndex | null) => vectorIndexTag(tag, v, metricShortLabel);

/** The whole line: `hnsw · Coseno · m = 16, ef_construction = 64`. */
export const vectorLine = (v: VectorIndex | null) => (v ? vectorIndexLine(v) : '');

/**
 * Reads which of the table's indexes are vector indexes, once per table
 * (again with `force`), when the engine has vectors, the usage doesn't say
 * it, and the table's columns (when known) include one a vector index can
 * take. Never fails: without it the views show the kind alone.
 */
export function loadIndexVectors(connectionId: string, database: string, table: TableRef, indexes: IndexUsage[], force = false): void {
  const conns = useConnectionsStore();
  if (!indexes.length || !conns.driverOf(connectionId)?.capabilities?.vectors) return;
  if (indexes.some((i) => i.vector)) return;
  // Columns read (not still loading, as when the explorer opens a table) and none a vector index takes.
  const cols = conns.columns[objKey(connectionId, database, table.schema ?? null, table.name)];
  if (cols?.status === 'ready' && !cols.items.some((c) => indexableVectorType(c.data_type))) return;
  const k = keyOf(connectionId, database, table);
  if (asked.has(k) && !force) return;
  asked.add(k);
  relationsSchema(connectionId, database, force)
    .then((tables) => {
      const t = findTable(tables, { schema: table.schema ?? null, name: table.name });
      found[k] = Object.fromEntries((t?.indexes ?? []).flatMap((x) => (x.vector ? [[x.name, x.vector]] : [])));
    })
    .catch(() => { asked.delete(k); });
}
