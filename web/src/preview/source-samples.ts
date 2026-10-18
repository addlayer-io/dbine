// Development only: a saved connection whose secrets come from a source
// (view=connection&source=…) and what "Examinar" lists for it.
import type { SavedConnection, SecretAccount, SecretBrowseListing } from '../api/types';

/** The accounts of this computer (list_secret_accounts): Zoho Vault's and one or two of each other manager. */
const acct = (n: number) => `6f1c2d4e-0000-4000-8000-0000000000${String(n).padStart(2, '0')}`;
const at = { created_at: '', updated_at: '' };
export const SAMPLE_ACCOUNTS: SecretAccount[] = [
  { id: acct(1), kind: 'zoho', label: 'Zoho Vault · ana@example.com', identity: 'ana@example.com', data_center: 'eu', client_id: '1000.XMDEG1KBDO9SE2IFTMDFSRPYYJ5L0U', ...at },
  { id: acct(2), kind: 'zoho', label: 'Zoho · equipo de datos', identity: 'datos@example.com', data_center: 'com', client_id: '1000.B', master: 'ask', ...at },
  { id: acct(10), kind: 'bitwarden', label: 'Bitwarden · ana@example.com', identity: 'ana@example.com', server: 'bitwarden.eu', client_id: 'user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04', ...at },
  { id: acct(11), kind: 'bitwarden_sm', label: 'Secrets Manager · producción', identity: 'a0a1a2a3-b0b1-4c0c-9d0d-e0e1e2e3e4e5', server: 'bitwarden.com', ...at },
  { id: acct(12), kind: 'vault', label: 'HashiCorp Vault · vault.example.com:8200', identity: 'approle', address: 'https://vault.example.com:8200', namespace: 'admin/datos', auth: 'approle', role_id: 'dbine', ...at },
  { id: acct(13), kind: 'vault', label: 'Vault local', identity: 'token', address: 'http://127.0.0.1:8200', ...at },
  { id: acct(14), kind: 'delinea', label: 'Delinea · svc_dbine', identity: 'svc_dbine', url: 'https://acme.secretservercloud.com', username: 'svc_dbine', domain: 'CORP', master: 'ask', ...at },
  { id: acct(15), kind: 'conjur', label: 'CyberArk Conjur · host/apps/dbine (myorg)', identity: 'host/apps/dbine (myorg)', url: 'https://conjur.example.com', conjur_account: 'myorg', login: 'host/apps/dbine', ...at },
  { id: acct(16), kind: 'ccp', label: 'CyberArk CCP · DBine · ccp.example.com', identity: 'DBine', url: 'https://ccp.example.com', app_id: 'DBine', client_cert: '~/certs/dbine.p12', ...at },
  { id: acct(17), kind: 'onepassword', label: '1Password · ana@example.com', identity: 'ana@example.com', account: 'my.1password.com', ...at },
  { id: acct(18), kind: 'aws', label: 'AWS · prod · eu-west-1', identity: 'arn:aws:iam::123456789012:user/ana', profile: 'prod', region: 'eu-west-1', ...at },
  { id: acct(19), kind: 'azure', label: 'Azure Key Vault · kv-ventas', identity: 'ana@contoso.com', tenant: 'contoso.onmicrosoft.com', vault_url: 'https://kv-ventas.vault.azure.net', ...at },
  { id: acct(20), kind: 'gcp', label: 'Google Cloud · ventas-prod', identity: 'ana@example.com', project: 'ventas-prod', ...at },
  { id: acct(21), kind: 'keepass', label: 'KeePass · databases.kdbx', identity: 'databases.kdbx', file: '~/Passwords/databases.kdbx', master: 'ask', ...at },
];

const SOURCES: Record<string, Record<string, unknown>> = {
  keepass: { kind: 'keepass', file: '~/Passwords/databases.kdbx', entry: 'Databases/Prod/pg-main', master: 'ask', fill_username: true },
  vault_db: { kind: 'vault_db', address: 'https://vault.example.com:8200', role: 'readonly' },
  'keepass-legacy': { kind: 'keepass', file: '~/Passwords/databases.kdbx', entry: 'Databases/Prod/pg-main', master: 'ask' },
  bitwarden: { kind: 'bitwarden', server: 'bitwarden.com', item: 'pg-main', client_id: 'user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04', fill_username: true },
  vault: { kind: 'vault', address: 'https://vault.example.com:8200', path: 'secret/myapp/db', key: 'password' },
  command: { kind: 'command', command: 'pass show bases/produccion' },
  aws: { kind: 'aws', secret_id: 'prod/myapp/db', region: 'us-east-1', json_key: 'password' },
  zoho: { kind: 'zoho', secret_name: 'pg-main', fill_username: true, account: '6f1c2d4e-0000-4000-8000-000000000001' },
  'zoho-missing': { kind: 'zoho', secret_name: 'pg-main', fill_username: true, account: '6f1c2d4e-0000-4000-8000-00000000dead' },
  'zoho-manual': { kind: 'zoho', data_center: 'eu', secret_name: 'pg-main', client_id: '1000.XMDEG1KBDO9SE2IFTMDFSRPYYJ5L0U', fill_username: true },
  // Every other manager through a saved account (and one not on this computer).
  'vault-account': { kind: 'vault', path: 'secret/myapp/db', key: 'password', secret_account: acct(12) },
  'vault_db-account': { kind: 'vault_db', role: 'readonly', secret_account: acct(12) },
  'bitwarden-account': { kind: 'bitwarden', item: 'pg-main', fill_username: true, secret_account: acct(10) },
  'aws-account': { kind: 'aws', secret_id: 'prod/myapp/db', json_key: 'password', secret_account: acct(18) },
  'keepass-account': { kind: 'keepass', entry: 'Databases/Prod/pg-main', fill_username: true, secret_account: acct(21) },
  'onepassword-account': { kind: 'onepassword', reference: 'op://Private/pg-main/password', fill_username: true, secret_account: acct(17) },
  'vault-missing': { kind: 'vault', path: 'secret/myapp/db', key: 'password', secret_account: acct(99) },
};

export function sourceConnection(kind: string, driver: string): SavedConnection {
  const src = SOURCES[kind] ?? SOURCES.keepass;
  const sources: Record<string, unknown> = { password: src };
  if (kind.startsWith('keepass')) sources['ssh.password'] = { ...src, entry: 'Servers/bastion', fill_username: undefined };
  return {
    id: 'c-src', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: [], save_password: true, updated_at: '',
    config: {
      driver, host: 'db-prod-01', port: 5432, database: 'ventas', username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false,
      options: {
        secret_sources: JSON.stringify(sources),
        'ssh.enabled': 'true', 'ssh.host': 'bastion.example.com', 'ssh.user': 'deploy', 'ssh.auth': 'password',
      },
    },
  };
}

const item = (id: string, name: string, path = '', detail = '') => ({ id, name, path, kind: 'item' as const, detail });

export function browseSample(a: { source: { kind: string }; item?: string; path?: string }): SecretBrowseListing {
  const base = { complete: true, truncated: false, search: false };
  if (a.item) {
    const fields: Record<string, string[]> = {
      keepass: ['Title', 'UserName', 'Password', 'URL', 'Notes', 'API token', 'Login (admin)', 'Password (admin)'],
      bitwarden: ['password', 'username', 'notes', 'Port', 'API token'],
    };
    return { ...base, entries: [], fields: fields[a.source.kind] ?? ['password', 'username'] };
  }
  switch (a.source.kind) {
    case 'vault_db':
      return { ...base, entries: [item('readonly', 'readonly', 'roles'), item('reporting', 'reporting', 'roles'), item('app', 'app', 'static-roles')] };
    case 'bitwarden':
      return { ...base, entries: [
        item('pg-main', 'pg-main', 'Bases de datos', '2b6e1f0a-6c1d-4e0b-9a7e-1d2f3c4b5a69'),
        item('3c7f2a1b-7d2e-4f1c-8b8f-2e3a4d5c6b70', 'twin', 'Bases de datos', '3c7f2a1b-7d2e-4f1c-8b8f-2e3a4d5c6b70'),
        item('org-db', 'org-db', 'Acme', '4d8a3b2c-8e3f-4a2d-9c9a-3f4b5e6d7c81'),
      ] };
    default:
      return { ...base, entries: [
        item('Databases/Prod/pg-main', 'pg-main', 'Databases/Prod', '53f5b102-441f-42d5-8c97-79edd12aecde'),
        item('Databases/Prod/mysql', 'mysql', 'Databases/Prod', '1a2b3c4d-441f-42d5-8c97-79edd12aec01'),
        item('Databases/Staging/pg-main', 'pg-main', 'Databases/Staging', '9e8d7c6b-441f-42d5-8c97-79edd12aec02'),
        item('Servers/bastion', 'bastion', 'Servers', '7f6e5d4c-441f-42d5-8c97-79edd12aec03'),
      ] };
  }
}
