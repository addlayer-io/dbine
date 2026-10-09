import { ElMessageBox } from 'element-plus';
import { errorKind } from '../api/client';
import { t } from '../i18n';

// An SSH tunnel reached a server it doesn't know (`ssh_unknown_host`): show
// the key's fingerprint and let the user trust it (docs/ssh-tunnels.md).

/** The server and fingerprint of an `ssh_unknown_host` error, else null. */
export function unknownSshHost(e: unknown): { host: string; port: number; fingerprint: string } | null {
  if (errorKind(e) !== 'ssh_unknown_host') return null;
  // The raw (Spanish) message from the backend; never the translated one.
  const raw = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : '';
  const fingerprint = /SHA256:[A-Za-z0-9+/=]+/.exec(raw)?.[0];
  // "el servidor SSH {host}:{port} no es conocido": the port is the last
  // `:digits` (an IPv6 host has colons of its own).
  const server = /SSH (\S+):(\d+) no es conocido/.exec(raw);
  const port = server ? Number(server[2]) : NaN;
  if (!fingerprint || !server || !server[1] || !(port > 0 && port < 65536)) return null;
  return { host: server[1], port, fingerprint };
}

/**
 * The `ssh.trusted` entry for a key: bound to the server it was accepted for,
 * so it never vouches for another hop of the tunnel (crates/dbine-tunnel).
 */
export function trustedEntry(host: string, port: number, fingerprint: string): string {
  return `[${host}]:${port} ${fingerprint}`;
}

/** The server and fingerprint of an `ssh.trusted` entry; null for a bare fingerprint of older versions. */
export function parseTrusted(entry: string): { host: string; port: number; fingerprint: string } | null {
  const m = /^\[(.+)\]:(\d+) (SHA256:\S+)$/.exec(entry.trim());
  return m ? { host: m[1], port: Number(m[2]), fingerprint: m[3] } : null;
}

/**
 * `ssh.trusted` (comma-separated) with `entry` added, dropping bare
 * fingerprints of older versions (no server matches them any more). Null if
 * another key is already accepted for the same server: a changed key is
 * never swapped in from the first-connection prompt, the user forgets the old
 * one in the form first. Same rules as the backend's (`add_trusted`).
 */
export function addTrusted(trusted: string, entry: string): string | null {
  const mine = parseTrusted(entry);
  if (!mine) return null;
  const kept = trusted
    .split(',')
    .map((e) => e.trim())
    .filter((e) => parseTrusted(e) !== null);
  const same = kept.map(parseTrusted).filter((s) => s!.port === mine.port && s!.host.toLowerCase() === mine.host.toLowerCase());
  if (same.some((s) => s!.fingerprint !== mine.fingerprint)) return null;
  return (same.length ? kept : [...kept, entry.trim()]).join(',');
}

/** `ssh.trusted` without the entry at `index` (the user forgets that server). */
export function forgetTrusted(trusted: string, index: number): string {
  return trusted
    .split(',')
    .map((e) => e.trim())
    .filter(Boolean)
    .filter((_, i) => i !== index)
    .join(',');
}

/**
 * Ask whether to trust the server; resolves to its `ssh.trusted` entry
 * (`[host]:port SHA256:…`) if so.
 */
export async function askTrustSshHost(e: unknown): Promise<string | null> {
  const u = unknownSshHost(e);
  if (!u) return null;
  try {
    await ElMessageBox.confirm(
      `<p>${escape(t('tunnel:unknownBody', { host: `${u.host}:${u.port}` }))}</p><p><code style="user-select:all;word-break:break-all">${escape(u.fingerprint)}</code></p>`,
      t('tunnel:unknownTitle'),
      { confirmButtonText: t('tunnel:unknownTrust'), cancelButtonText: t('common:cancel'), dangerouslyUseHTMLString: true, type: 'warning' },
    );
    return trustedEntry(u.host, u.port, u.fingerprint);
  } catch {
    return null;
  }
}

function escape(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
}
