import { ElMessageBox } from 'element-plus';
import { errorKind } from '../api/client';
import { t } from '../i18n';

// An SSH tunnel reached a server it doesn't know (`ssh_unknown_host`): show
// the key's fingerprint and let the user trust it (docs/ssh-tunnels.md).

/** The server and fingerprint of an `ssh_unknown_host` error, else null. */
export function unknownSshHost(e: unknown): { host: string; fingerprint: string } | null {
  if (errorKind(e) !== 'ssh_unknown_host') return null;
  // The raw (Spanish) message from the backend; never the translated one.
  const raw = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : '';
  const fingerprint = /SHA256:[A-Za-z0-9+/=]+/.exec(raw)?.[0];
  const host = /SSH (\S+) no es conocido/.exec(raw)?.[1] ?? '';
  return fingerprint ? { host, fingerprint } : null;
}

/** Ask whether to trust the server; resolves to its fingerprint if so. */
export async function askTrustSshHost(e: unknown): Promise<string | null> {
  const u = unknownSshHost(e);
  if (!u) return null;
  try {
    await ElMessageBox.confirm(
      `<p>${escape(t('tunnel:unknownBody', { host: u.host }))}</p><p><code style="user-select:all;word-break:break-all">${escape(u.fingerprint)}</code></p>`,
      t('tunnel:unknownTitle'),
      { confirmButtonText: t('tunnel:unknownTrust'), cancelButtonText: t('common:cancel'), dangerouslyUseHTMLString: true, type: 'warning' },
    );
    return u.fingerprint;
  } catch {
    return null;
  }
}

function escape(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
}
