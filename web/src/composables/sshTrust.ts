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

/**
 * `ssh.trusted` (comma-separated) with `entry` added, replacing a key accepted
 * before for the same server and dropping bare fingerprints of older versions
 * (no server matches them any more). Same rules as the backend's.
 */
export function addTrusted(trusted: string, entry: string): string {
  const server = (e: string) => /^\[(.+)\]:(\d+) SHA256:\S+$/.exec(e.trim());
  const mine = server(entry);
  if (!mine) return trusted;
  const kept = trusted
    .split(',')
    .map((e) => e.trim())
    .filter((e) => {
      const s = server(e);
      return s !== null && !(s[2] === mine[2] && s[1].toLowerCase() === mine[1].toLowerCase());
    });
  return [...kept, entry.trim()].join(',');
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
