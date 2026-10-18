// Development only: a saved connection whose secrets come from a source
// (view=connection&source=…) and what "Examinar" lists for it.
import type { SavedConnection, SecretAccount, SecretBrowseListing } from '../api/types';

/** Zoho Vault accounts of this computer (list_secret_accounts). */
export const SAMPLE_ACCOUNTS: SecretAccount[] = [
  { id: '6f1c2d4e-0000-4000-8000-000000000001', kind: 'zoho', label: 'Zoho Vault · ana@example.com', identity: 'ana@example.com', data_center: 'eu', client_id: '1000.XMDEG1KBDO9SE2IFTMDFSRPYYJ5L0U', created_at: '', updated_at: '' },
  { id: '6f1c2d4e-0000-4000-8000-000000000002', kind: 'zoho', label: 'Zoho · equipo de datos', identity: 'datos@example.com', data_center: 'com', client_id: '1000.B', master: 'ask', created_at: '', updated_at: '' },
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
