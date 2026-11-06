import { invoke } from '@tauri-apps/api/core';
import type { ListedSshProfile, SshProfile } from './types';

// SSH tunnel profiles (src-tauri: commands/ssh_profiles.rs; docs/ssh-tunnels.md,
// "Profiles"). Secret values only go in (to the keychain); nothing reads them back.

/** One server of a `~/.ssh/config` alias's way (`ssh_config_resolve`). */
export interface SshConfigHop {
  name: string;
  host: string;
  port: number;
  user: string;
  /** What the first server is reached through, as it would run. */
  proxy_command: string | null;
}

export const sshProfilesApi = {
  /** The aliases of `~/.ssh/config`. */
  configHosts: () => invoke<string[]>('ssh_config_hosts'),
  /** The servers an alias goes through, in order, as `ssh` resolves them here. */
  configResolve: (alias: string) => invoke<SshConfigHop[]>('ssh_config_resolve', { args: { alias } }),
  /** The user confirmed the `ProxyCommand` an alias runs (`ssh_proxy_command`). */
  approveProxyCommand: (alias: string, command: string) =>
    invoke<void>('approve_ssh_proxy_command', { args: { alias, command } }),
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
