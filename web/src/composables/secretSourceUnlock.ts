import { ElMessageBox } from 'element-plus';
import { errorKind, type CommandErrorShape } from '../api/client';
import type { SecretSourceUnlock } from '../api/types';
import { tb } from '../i18n/backend';
import { t } from '../i18n';

// A KeePass database or Zoho Vault whose master password is set to "ask"
// (`secret_source_unlock`, docs/secret-sources.md): the user types it here.
// The caller decides where it goes: the backend's memory for a saved
// connection (`unlock_secret_source`), or only the test that asked.

/** The master password to ask for of a `secret_source_unlock` error, else null. */
export function unlockOf(e: unknown): SecretSourceUnlock | null {
  if (errorKind(e) !== 'secret_source_unlock') return null;
  const u = (e as CommandErrorShape).unlock;
  return u && typeof u === 'object' && typeof u.field === 'string' ? u : null;
}

/** Ask for it; null when cancelled. */
export async function askMasterPassword(u: SecretSourceUnlock): Promise<string | null> {
  const what = t(`connection:source.unlock.${u.kind === 'zoho' ? 'zoho' : 'keepass'}`, { field: tb(u.field_label), name: u.connection_name || '—' });
  const message = u.wrong ? `${t('connection:source.unlock.wrong')}\n\n${what}` : what;
  try {
    const { value } = await ElMessageBox.prompt(message, t('connection:source.unlock.title'), {
      inputType: 'password',
      confirmButtonText: t('common:connect'),
      cancelButtonText: t('common:cancel'),
      inputValidator: (v: string) => !!v || t('connection:source.unlock.empty'),
    });
    return value || null;
  } catch {
    return null;
  }
}
