import type { DriverInfo } from '../api/types';

// Whether "Comparar esquemas" applies changes to a driver's objects, per
// kind (`Driver::supports_schema_sync_for`): Redis applies its search
// indexes' (`index`) but has no schema for its keys (`key`). The explorer's
// "Modificar…", the designer and "Borrar índice" keep asking
// `supports_schema_sync`, which works on the designer's kind. No Vue
// imports, so `node --test` loads it (schemaSync.test.mjs).

type SyncInfo = Pick<DriverInfo, 'supports_schema_sync' | 'schema_sync_kinds'>;

/** Whether it applies changes to objects of `kind`. */
export function syncsKind(d: SyncInfo, kind: string): boolean {
  return d.schema_sync_kinds ? d.schema_sync_kinds.includes(kind) : d.supports_schema_sync;
}

/** Whether it applies changes to every one of `kinds`; with none (only
 *  code objects, or nothing pending), as `supports_schema_sync` says. */
export function syncsKinds(d: SyncInfo, kinds: Iterable<string>): boolean {
  const all = [...kinds];
  return all.length ? all.every((k) => syncsKind(d, k)) : d.supports_schema_sync;
}
