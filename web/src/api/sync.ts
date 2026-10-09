import { invoke } from '@tauri-apps/api/core';

// Cloud backup and user preferences (src-tauri/src/commands/{sync,settings}.rs,
// docs/sync.md). Field names are snake_case, as in Rust.

export type ProviderKind = 'google_drive' | 'onedrive' | 'folder';

export interface SyncConfig {
  provider: ProviderKind | null;
  folder: string | null;
  account: string | null;
  enabled: boolean;
  auto: boolean;
}

export type SyncAction =
  | { action: 'up_to_date' }
  | { action: 'uploaded'; previous_kept: boolean }
  | { action: 'downloaded'; local_backup: string | null; device: string };

export interface RunStatus {
  running: boolean;
  last_error: string | null;
  /** `wrong_passphrase` (type it again), `sync_auth` (sign in again), `sync`… */
  last_error_kind: string | null;
  last_action: SyncAction | null;
  last_run_at: string | null;
}

export interface SyncStatus {
  config: SyncConfig;
  providers: { kind: ProviderKind; label: string; available: boolean }[];
  status: RunStatus;
  dirty: boolean;
  last_sync_at: string | null;
}

export interface RemoteInfo {
  updated_at: string;
  device: string;
  app_version: string;
}

export interface LocalBackup {
  path: string;
  updated_at: string;
  device: string;
  size: number;
}

export const syncApi = {
  status: () => invoke<SyncStatus>('sync_status'),
  connect: (provider: ProviderKind, folder: string | null = null) =>
    invoke<{ account: string; remote: RemoteInfo | null }>('sync_connect', { args: { provider, folder } }),
  cancelConnect: () => invoke<void>('sync_cancel_connect'),
  setup: (passphrase: string, mode: 'upload' | 'restore') => invoke<SyncAction>('sync_setup', { args: { passphrase, mode } }),
  now: () => invoke<SyncAction>('sync_now'),
  uploadNow: () => invoke<SyncAction>('sync_upload_now'),
  restoreNow: () => invoke<SyncAction>('sync_restore_now'),
  setAuto: (auto: boolean) => invoke<void>('sync_set_auto', { args: { auto } }),
  setPassphrase: (passphrase: string) => invoke<void>('sync_set_passphrase', { args: { passphrase } }),
  changePassphrase: (current: string, next: string) =>
    invoke<SyncAction>('sync_change_passphrase', { args: { current, new: next } }),
  disconnect: (deleteRemote: boolean) => invoke<void>('sync_disconnect', { args: { delete_remote: deleteRemote } }),
  localBackups: () => invoke<LocalBackup[]>('sync_local_backups'),
  restoreLocal: (path: string) => invoke<void>('sync_restore_local', { args: { path } }),

  listSettings: () => invoke<Record<string, unknown>>('list_settings'),
  setSetting: (key: string, value: unknown) => invoke<void>('set_setting', { args: { key, value } }),
};
