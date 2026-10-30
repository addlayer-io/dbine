import { invoke } from '@tauri-apps/api/core';
import type { ListedSshProfile, SshProfile } from './types';

// SSH tunnel profiles (src-tauri: commands/ssh_profiles.rs; docs/ssh-tunnels.md,
// "Profiles"). Secret values only go in (to the keychain); nothing reads them back.

export const sshProfilesApi = {
  list: () => invoke<ListedSshProfile[]>('list_ssh_profiles'),
  /** `profile.id` empty: a new one. Secrets typed in `options` go to the
   *  keychain; empty ones keep the stored value or, with `fromConnection`
   *  ("Guardar como perfil"), are copied from that connection's keychain entry. */
  save: (profile: SshProfile, fromConnection: string | null = null) =>
    invoke<SshProfile>('save_ssh_profile', { args: { profile, from_connection: fromConnection } }),
  /** The connections that use it get a copy of it as their own settings; returns their ids. */
  remove: (id: string) => invoke<string[]>('delete_ssh_profile', { args: { id } }),
  /** Trust an SSH server's key in the profile (`[host]:port SHA256:…`). */
  trustHost: (profileId: string, fingerprint: string) =>
    invoke<void>('trust_ssh_profile_host', { args: { profile_id: profileId, fingerprint } }),
};
