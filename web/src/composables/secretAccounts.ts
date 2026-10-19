import { ref } from 'vue';
import { accountsApi } from '../api/secretAccounts';
import type { SecretAccount, SecretAccountKind } from '../api/types';
import { ZOHO_DATA_CENTERS } from './secretSources';
import { hostOf } from './accountWizards';

// The secret-manager accounts of this computer, shared by the connection
// form, the confirmation dialog and Settings › Gestores de secretos.

export const secretAccounts = ref<SecretAccount[]>([]);
/** Loaded at least once (before, a definition's account can't be called missing). */
export const secretAccountsLoaded = ref(false);

export async function loadSecretAccounts(): Promise<SecretAccount[]> {
  try {
    secretAccounts.value = await accountsApi.list();
  } catch {
    secretAccounts.value = [];
  }
  secretAccountsLoaded.value = true;
  return secretAccounts.value;
}

/** An account's label, else its ID (one this computer doesn't have). */
export function accountLabel(id: string): string {
  return secretAccounts.value.find((a) => a.id === id)?.label ?? id;
}

/** The domain of a Zoho data centre (`eu` → `zoho.eu`). */
export function zohoDomain(dc: string): string {
  return ZOHO_DATA_CENTERS.find(([id]) => id === dc)?.[1] ?? dc;
}

/** Every kind of account, in Settings' order. */
export const ACCOUNT_KINDS: SecretAccountKind[] = ['zoho', 'bitwarden', 'bitwarden_sm', 'vault', 'delinea', 'conjur', 'ccp', 'onepassword', 'aws', 'azure', 'gcp', 'keepass'];

export { PROVIDER } from './accountWizards';

/** The accounts of a kind, by label. */
export function accountsOf(list: SecretAccount[], kind: SecretAccountKind | undefined): SecretAccount[] {
  return kind ? list.filter((a) => a.kind === kind) : [];
}

/** Settings' groups: each kind that has accounts, in order. */
export function groupedAccounts(list: SecretAccount[]): { kind: SecretAccountKind; accounts: SecretAccount[] }[] {
  return ACCOUNT_KINDS.map((kind) => ({ kind, accounts: accountsOf(list, kind) })).filter((g) => g.accounts.length);
}

/** What Settings shows of an account besides its name and identity, as
 *  `[key, value]`: the key is a `secretAccounts:facts.<key>` label (with
 *  `{{value}}`); `master.*` keys are whole texts. Never a secret. */
export function accountFacts(a: SecretAccount): [string, string][] {
  const r = a as unknown as Record<string, string | undefined>;
  const v = (k: string) => (r[k] ?? '').trim();
  const master = (): [string, string] => [`master.${v('master') || 'keychain'}`, ''];
  switch (a.kind) {
    case 'zoho': return [['dataCenter', zohoDomain(v('data_center'))], [`passphrase.${v('master') || 'keychain'}`, '']];
    case 'bitwarden': return [['server', v('server') || 'bitwarden.com'], master()];
    case 'bitwarden_sm': return [['server', v('server') || 'bitwarden.com']];
    case 'vault': return [['server', hostOf(v('address'))], ['auth', v('auth') === 'approle' ? 'AppRole' : 'token'], ...(v('namespace') ? [['namespace', v('namespace')] as [string, string]] : [])];
    case 'delinea': return [['server', hostOf(v('platform_url')) || hostOf(v('url'))], ['user', v('username')], master()];
    case 'conjur': return [['server', hostOf(v('url'))], ['user', v('login')]];
    case 'ccp': return [['server', hostOf(v('url'))], ['appId', v('app_id')], ...(v('client_cert') ? [['certificate', v('client_cert')] as [string, string]] : [])];
    case 'onepassword': return [['opAccount', v('account')]];
    case 'aws': return [['profile', v('profile') || 'default'], ['region', v('region')]];
    case 'azure': return [['vault', hostOf(v('vault_url'))], ...(v('tenant') ? [['tenant', v('tenant')] as [string, string]] : [])];
    // Saved before the gcloud account was kept: gcloud's active one (reconnect to pin one).
    case 'gcp': return [['project', v('project')], v('account') ? ['gcloudAccount', v('account')] : ['gcloudActive', '']];
    case 'keepass': return [['file', v('file')], master()];
    default: return [];
  }
}
