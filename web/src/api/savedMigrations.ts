import { invoke } from '@tauri-apps/api/core';
import type { CopyOrder, DeltaDepth, MigrationMode, PlanOptions } from './migration';

// Saved migrations: the "Migraciones" node of a database (src-tauri/src/commands/saved_migrations.rs,
// crates/dbine-core/src/state.rs `SavedMigration`). Each one keeps the Migrate screen's
// configuration and the ids of the runs started from it (the runs: `migrationApi.runs`).

/** The Migrate screen's form, as the entry keeps it (the backend doesn't read it). */
export interface MigrationConfig {
  target_driver: string;
  mode: MigrationMode;
  target_connection_id: string;
  target_database: string;
  /** Picked tables (`schema.name`, schema '' when none); `null` = all of them. */
  tables: string[] | null;
  options: PlanOptions;
  sync: { depth: DeltaDepth; max_cores: number };
  /** Sync keys picked per table (`schema.name` → columns joined by a comma). */
  sync_keys: Record<string, string>;
  transfer: { parallel: number; order: CopyOrder; commit_rows: number };
}

export interface SavedMigration {
  id: string;
  connection_id: string;
  database: string;
  name: string;
  /** `null`: an entry whose document couldn't be read (it opens as a new draft). */
  config: MigrationConfig | null;
  /** Runs started from it, oldest first; the last is the current one. Machine-local. */
  run_ids: string[];
  created_at: string;
  updated_at: string;
}

export const savedMigrationsApi = {
  list: (connectionId: string, database: string) =>
    invoke<SavedMigration[]>('list_saved_migrations', { args: { connection_id: connectionId, database } }),
  get: (id: string) => invoke<SavedMigration>('get_saved_migration', { args: { id } }),
  save: (migration: SavedMigration) => invoke<SavedMigration>('save_saved_migration', { args: { migration } }),
  rename: (id: string, name: string) => invoke<SavedMigration>('rename_saved_migration', { args: { id, name } }),
  linkRun: (id: string, runId: string) => invoke<SavedMigration>('link_saved_migration_run', { args: { id, run_id: runId } }),
  duplicate: (id: string, name: string) => invoke<SavedMigration>('duplicate_saved_migration', { args: { id, name } }),
  delete: (id: string) => invoke<void>('delete_saved_migration', { args: { id } }),
};
