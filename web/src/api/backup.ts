import { invoke } from '@tauri-apps/api/core';
import type { ObjectRef } from './types';

// Backups (docs/backups.md): DBine's copies (a script file with the
// structure and the data) and the engine's own backups.

export interface BackupCopy {
  id: string;
  connection_id: string;
  database: string;
  path: string;
  created_at: string;
  size: number;
  objects: number;
  rows: number;
  data: boolean;
  duration_ms: number;
}

export interface BackupEntry {
  id: string;
  database: string | null;
  kind: string | null;
  started: string | null;
  finished: string | null;
  size: number | null;
  location: string | null;
  status: string | null;
  details: [string, string][];
  restorable: boolean;
}

export type BackupAction =
  | { action: 'backup'; database: string | null; options: Record<string, string> }
  | { action: 'restore'; source: string; database: string | null; options: Record<string, string> }
  | { action: 'delete'; source: string };

export interface BackupList {
  copies: BackupCopy[];
  native: BackupEntry[];
  native_error: string | null;
}

export const backupApi = {
  list: (connectionId: string, database: string) =>
    invoke<BackupList>('backup_list', { args: { connection_id: connectionId, database } }),
  /** `shown` hides the secret options (keys, passwords). */
  script: (connectionId: string, action: BackupAction) =>
    invoke<{ script: string; shown: string }>('backup_script', { args: { connection_id: connectionId, action } }),
  defaultPath: (connectionId: string, database: string) =>
    invoke<string>('backup_default_path', { args: { connection_id: connectionId, database } }),
  /** Progress: `script-progress` events with `backupId`; cancel: session `script:<backupId>`. */
  copy: (backupId: string, connectionId: string, database: string, objects: ObjectRef[], data: boolean, path: string) =>
    invoke<BackupCopy>('backup_copy', { args: { backup_id: backupId, connection_id: connectionId, database, objects, data, path } }),
  deleteCopy: (id: string, deleteFile: boolean) =>
    invoke<void>('backup_copy_delete', { args: { id, delete_file: deleteFile } }),
};
