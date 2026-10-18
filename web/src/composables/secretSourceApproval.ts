import { ElMessageBox } from 'element-plus';
import { api, errorKind, type CommandErrorShape } from '../api/client';
import type { SecretSourceApproval } from '../api/types';
import { tb } from '../i18n/backend';
import { t } from '../i18n';
import { sourceRows } from './secretSources';

// A secret field's source that came from outside this computer (an import, a
// restored backup) doesn't run until the user confirms it here
// (`secret_source_approval`, docs/secret-sources.md). The dialog shows the
// exact definition; the confirmation is bound to it, so any later change to
// the text asks again.

/** The definition to confirm of a `secret_source_approval` error, else null. */
export function approvalOf(e: unknown): SecretSourceApproval | null {
  if (errorKind(e) !== 'secret_source_approval') return null;
  const a = (e as CommandErrorShape).approval;
  return a && typeof a === 'object' && a.source && typeof a.field === 'string' ? a : null;
}

function escape(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
}

/**
 * Show what would run and, if the user confirms it, record the confirmation.
 * True when confirmed (the caller tries again).
 */
export async function askApproveSecretSource(e: unknown): Promise<boolean> {
  const a = approvalOf(e);
  if (!a) return false;
  const rows = sourceRows(a.source)
    .map(([k, v]) => {
      const label = escape(t(`connection:source.input.${k}`));
      const value = k === 'fill_username' ? escape(t('common:yes')) : escape(v);
      return `<div style="margin-top:6px"><div style="font-size:11.5px;opacity:.75">${label}</div><code style="display:block;white-space:pre-wrap;word-break:break-all;user-select:all">${value}</code></div>`;
    })
    .join('');
  const body = escape(t('connection:source.approval.body', { name: a.connection_name || '—', field: tb(a.field_label) }));
  const kind = escape(t(`connection:source.kind.${a.source.kind}`));
  try {
    await ElMessageBox.confirm(
      `<p>${body}</p><p><strong>${kind}</strong></p>${rows}<p style="margin-top:10px">${escape(t('connection:source.approval.warning'))}</p>`,
      t('connection:source.approval.title'),
      {
        confirmButtonText: t('connection:source.approval.confirm'),
        cancelButtonText: t('common:cancel'),
        dangerouslyUseHTMLString: true,
        type: 'warning',
        // No button focused: Enter doesn't confirm it by accident.
        autofocus: false,
      },
    );
  } catch {
    return false;
  }
  await api.approveSecretSource(a.connection_id, a.field, a.source);
  return true;
}
