import { invoke } from '@tauri-apps/api/core';
import type { SecretAccount, ZohoExchanged } from './types';

// Secret-manager accounts saved on this computer (src-tauri:
// secret_sources/accounts.rs; docs/secret-sources.md, "Accounts").

export const accountsApi = {
  list: () => invoke<SecretAccount[]>('list_secret_accounts'),
  rename: (id: string, label: string) => invoke<SecretAccount>('rename_secret_account', { args: { id, label } }),
  /** Deletes its keychain items too; connections naming it then ask for an account. */
  remove: (id: string) => invoke<void>('delete_secret_account', { args: { id } }),
  /** "Conectar Zoho Vault", step 3: exchange the code (the data centre is found here). */
  zohoExchange: (clientId: string, clientSecret: string, code: string) =>
    invoke<ZohoExchanged>('zoho_connect_exchange', { args: { client_id: clientId, client_secret: clientSecret, code } }),
  /** Step 4: check the passphrase and save (with `accountId`: reconnect it, keeping its ID). */
  zohoFinish: (a: { pending: string; accountId: string | null; label: string; master: 'keychain' | 'ask' | 'command'; masterCommand: string; passphrase: string }) =>
    invoke<SecretAccount>('zoho_connect_finish', {
      args: { pending: a.pending, account_id: a.accountId, label: a.label, master: a.master, master_command: a.masterCommand, passphrase: a.passphrase },
    }),
  /** The wizard closed before finishing: nothing is kept. */
  zohoCancel: (pending: string) => invoke<void>('zoho_connect_cancel', { args: { pending } }),
  /** Step 1: Zoho's API console in the browser. */
  openZohoConsole: () => invoke<void>('open_zoho_api_console'),
};
