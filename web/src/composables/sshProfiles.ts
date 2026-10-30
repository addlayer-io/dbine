import { ref } from 'vue';
import { sshProfilesApi } from '../api/sshProfiles';
import type { ListedSshProfile } from '../api/types';
import { readSources, writeSources, sourceFrom, tokenKey, typedTokens, type SourceDraft } from './secretSources';
import type { SshValues } from '../components/SshTunnelSection.vue';

// SSH tunnel profiles (docs/ssh-tunnels.md, "Profiles"): the list every
// window shares (reloaded on `ssh_profile` changes, composables/crossWindow.ts)
// and the conversions between a profile's options and the tunnel form.

export const sshProfiles = ref<ListedSshProfile[]>([]);

export async function loadSshProfiles(): Promise<ListedSshProfile[]> {
  try {
    sshProfiles.value = await sshProfilesApi.list();
  } catch {
    sshProfiles.value = [];
  }
  return sshProfiles.value;
}

export function profileById(id: string): ListedSshProfile | undefined {
  return sshProfiles.value.find((p) => p.id === id);
}

/** The tunnel form's values for a profile's options (secrets never come back). */
export function valuesFromOptions(o: Record<string, string>): SshValues {
  return {
    enabled: true, profile: '', host: o['ssh.host'] ?? '', port: o['ssh.port'] ?? '', user: o['ssh.user'] ?? '',
    auth: (o['ssh.auth'] as SshValues['auth']) || 'password', password: '', key_path: o['ssh.key_path'] ?? '', passphrase: '',
    jump: o['ssh.jump'] ?? '', trusted: o['ssh.trusted'] ?? '',
  };
}

/** The tunnel's secret fields' sources in a profile's (or connection's) options. */
export function tunnelSources(o: Record<string, string>) {
  const all = readSources(o);
  return { 'ssh.password': all['ssh.password'], 'ssh.passphrase': all['ssh.passphrase'] };
}

/** A profile's options from the tunnel form: its settings, the sources of
 *  its secrets and what was typed for them (the backend moves those to the
 *  keychain). */
export function optionsFromValues(s: SshValues, drafts: Record<string, SourceDraft>): Record<string, string> {
  const o: Record<string, string> = {};
  const set = (k: string, v: string) => { if (v.trim()) o[`ssh.${k}`] = v.trim(); };
  set('host', s.host); set('port', s.port); set('user', s.user); set('auth', s.auth); set('jump', s.jump); set('trusted', s.trusted);
  const sources: Record<string, NonNullable<ReturnType<typeof sourceFrom>>> = {};
  const sourced = (key: string): boolean => {
    const d = drafts[key];
    const src = d ? sourceFrom(d, key) : null;
    if (!src) return false;
    sources[key] = src;
    for (const [slot, v] of Object.entries(typedTokens(d))) o[tokenKey(key, slot)] = v;
    return true;
  };
  if (s.auth === 'key') {
    set('key_path', s.key_path);
    if (!sourced('ssh.passphrase') && s.passphrase) o['ssh.passphrase'] = s.passphrase;
  }
  if (s.auth === 'password' && !sourced('ssh.password') && s.password) o['ssh.password'] = s.password;
  writeSources(o, sources);
  return o;
}

/** A short line describing a profile: `user@host:port` and its jump hosts. */
export function profileSummary(o: Record<string, string>): string {
  const port = o['ssh.port'] && o['ssh.port'] !== '22' ? `:${o['ssh.port']}` : '';
  const at = `${o['ssh.user'] ?? ''}@${o['ssh.host'] ?? ''}${port}`;
  return o['ssh.jump'] ? `${o['ssh.jump']} → ${at}` : at;
}
