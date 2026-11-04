import { ElMessageBox } from 'element-plus';
import { errorDetail } from '../api/client';
import { t } from '../i18n';

// A server that only speaks TLS 1.0/1.1 (old SQL Server): the driver fails
// with code `TLS_LEGACY_ONLY` and the app asks once whether to connect with
// legacy TLS anyway. Only a yes turns it on (docs/tls-options.md): a failed
// TLS 1.2 handshake never falls back by itself, since a man in the middle
// could make it fail on purpose.

/** The code of a connect failure from a server without TLS 1.2. */
export const LEGACY_TLS_ONLY = 'TLS_LEGACY_ONLY';

/** The server a `TLS_LEGACY_ONLY` failure names ("SQL Server 2012, 11.0.2100"), else null. */
export function legacyTlsServer(e: unknown): string | null {
  if (errorDetail(e)?.code !== LEGACY_TLS_ONLY) return null;
  // The raw (Spanish) message from the backend; never the translated one.
  const raw = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : '';
  return /^Este servidor \((.+?)\) solo acepta TLS/.exec(raw)?.[1] ?? 'SQL Server';
}

/** Ask whether to connect with legacy TLS; true on yes. */
export async function askAllowLegacyTls(e: unknown): Promise<boolean> {
  const server = legacyTlsServer(e);
  if (!server) return false;
  try {
    await ElMessageBox.confirm(
      `<p>${escape(t('connection:legacyTls.ask', { server }))}</p><p style="font-size:12px;opacity:.85">${escape(t('connection:legacyTls.askDetail'))}</p>`,
      t('connection:legacyTls.askTitle'),
      { confirmButtonText: t('connection:legacyTls.askConfirm'), cancelButtonText: t('common:cancel'), dangerouslyUseHTMLString: true, type: 'warning' },
    );
    return true;
  } catch {
    return false;
  }
}

function escape(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
}
