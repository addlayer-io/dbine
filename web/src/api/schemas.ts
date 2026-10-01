import { invoke } from '@tauri-apps/api/core';
import type { SchemaSpec } from './types';

// "Nuevo esquema…" / "Borrar esquema…" in the explorer: the scripts, which
// the UI shows and runs only on the user's click.

/** One grant on the new schema. */
export interface SchemaGrant {
  principal: string;
  privileges: string[];
  grantable: boolean;
}

export const schemasApi = {
  spec: (connectionId: string) => invoke<SchemaSpec | null>('schema_spec', { args: { connection_id: connectionId } }),
  /** Create + each grant + the owner, as one script; `database`: the one the menu was opened on. */
  createScript: (connectionId: string, database: string, name: string, owner: string | null, grants: SchemaGrant[]) =>
    invoke<string>('create_schema_script', { args: { connection_id: connectionId, database: database || null, name, owner, grants } }),
  dropScript: (connectionId: string, database: string, name: string, cascade: boolean) =>
    invoke<string>('drop_schema_script', { args: { connection_id: connectionId, database: database || null, name, cascade } }),
  /** How many objects the schema holds (read from the server). */
  objectCount: (connectionId: string, database: string, schema: string) =>
    invoke<number>('schema_object_count', { args: { connection_id: connectionId, database, schema } }),
};
