import { ref } from 'vue';
import { accountsApi } from '../api/secretAccounts';
import type { SecretAccount } from '../api/types';
import { ZOHO_DATA_CENTERS } from './secretSources';

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
