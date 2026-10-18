import type { SecretSource, SecretSourceKind } from '../api/types';

// Where a connection's secret field takes its value (docs/secret-sources.md):
// kept in the keychain, asked every time (the password only), or a command /
// secret manager resolved at connect time. The definitions go in the
// connection's options as JSON (`secret_sources`); a Vault token (or a KeePass
// / Zoho Vault master password) typed in the form goes as
// `source_token:<field>`, Zoho's client secret and refresh token as
// `source_token:<field>#<slot>`, and the backend moves them to the keychain.
// Pure functions: the form (ConnectionView, SshTunnelSection) and the tests.

export const SOURCES_OPTION = 'secret_sources';
export const TOKEN_PREFIX = 'source_token:';

/** What the picker offers, in order. `ask` only for the password. */
export const SOURCE_KINDS = ['saved', 'ask', 'command', 'onepassword', 'vault', 'aws', 'azure', 'gcp', 'keepass', 'zoho'] as const;
export type DraftKind = (typeof SOURCE_KINDS)[number];

/** Where a KeePass / Zoho Vault master password comes from. */
export const MASTER_SOURCES = ['keychain', 'ask', 'command', 'none'] as const;
export type MasterKind = (typeof MASTER_SOURCES)[number];

/** Zoho's data centres (dbine_core::secret_sources::ZOHO_DATA_CENTERS): id → domain. */
export const ZOHO_DATA_CENTERS: [string, string][] = [
  ['com', 'zoho.com'], ['eu', 'zoho.eu'], ['in', 'zoho.in'], ['com.au', 'zoho.com.au'], ['jp', 'zoho.jp'], ['ca', 'zohocloud.ca'],
  ['sa', 'zoho.sa'], ['uk', 'zoho.uk'], ['ae', 'zoho.ae'], ['sg', 'zoho.sg'], ['com.cn', 'zoho.com.cn'],
];

/** The keychain key of a field's token (`slot` empty: the main one). */
export function tokenKey(field: string, slot = ''): string {
  return slot ? `${TOKEN_PREFIX}${field}#${slot}` : `${TOKEN_PREFIX}${field}`;
}

/** The form's state of one field's source: every provider's inputs, as text. */
export interface SourceDraft {
  kind: DraftKind;
  command: string;
  json_key: string;
  reference: string;
  account: string;
  address: string;
  path: string;
  key: string;
  namespace: string;
  kv_version: 'auto' | '1' | '2';
  ca_file: string;
  /** A Vault token typed now (never read back: the kept one stays if empty). */
  token: string;
  secret_id: string;
  region: string;
  profile: string;
  fill_username: boolean;
  vault_url: string;
  name: string;
  version: string;
  project: string;
  secret: string;
  /** KeePass. */
  file: string;
  entry: string;
  /** KeePass / Zoho Vault: the entry's or secret's field. */
  attribute: string;
  key_file: string;
  master: MasterKind;
  master_command: string;
  /** Zoho Vault. */
  data_center: string;
  secret_name: string;
  client_id: string;
  /** Typed now, like `token` (never read back). */
  client_secret: string;
  refresh_token: string;
}

export function emptyDraft(kind: DraftKind = 'saved'): SourceDraft {
  return {
    kind, command: '', json_key: '', reference: '', account: '', address: '', path: '', key: '', namespace: '', kv_version: 'auto',
    ca_file: '', token: '', secret_id: '', region: '', profile: '', fill_username: false, vault_url: '', name: '', version: '', project: '', secret: '',
    file: '', entry: '', attribute: '', key_file: '', master: 'keychain', master_command: '', data_center: '', secret_name: '', client_id: '',
    client_secret: '', refresh_token: '',
  };
}

/** The definitions in a config's options; `{}` when there are none (or they can't be read). */
export function readSources(options: Record<string, string> | undefined): Record<string, SecretSource> {
  const raw = options?.[SOURCES_OPTION];
  if (!raw) return {};
  try {
    const v = JSON.parse(raw);
    return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, SecretSource>) : {};
  } catch {
    return {};
  }
}

/** Store the definitions in the options (removed when there are none). */
export function writeSources(options: Record<string, string>, sources: Record<string, SecretSource>): void {
  if (Object.keys(sources).length) options[SOURCES_OPTION] = JSON.stringify(sources);
  else delete options[SOURCES_OPTION];
}

/** The draft of a stored definition (or of "saved" / "ask"). */
export function draftFrom(source: SecretSource | undefined, ask = false): SourceDraft {
  if (!source) return emptyDraft(ask ? 'ask' : 'saved');
  const d = emptyDraft(source.kind);
  for (const [k, v] of Object.entries(source)) {
    if (k === 'kind' || !(k in d)) continue;
    (d as unknown as Record<string, unknown>)[k] = k === 'fill_username' ? v === true : String(v ?? '');
  }
  if (!['auto', '1', '2'].includes(d.kv_version)) d.kv_version = 'auto';
  if (!(MASTER_SOURCES as readonly string[]).includes(d.master)) d.master = 'keychain';
  return d;
}

/** Whether the field's value comes from a command or a secret manager. */
export function isExternal(d: SourceDraft | undefined): boolean {
  return !!d && d.kind !== 'saved' && d.kind !== 'ask';
}

/** The definition a draft stands for (`null` for saved / ask). Optional
 *  inputs left empty are left out, as the backend stores them. */
export function sourceFrom(d: SourceDraft, field: string): SecretSource | null {
  const t = (s: string) => s.trim();
  const opt = <T extends object>(o: T): T => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== '' && v !== false && v !== undefined)) as T;
  switch (d.kind) {
    case 'command': return opt({ kind: 'command' as const, command: t(d.command), json_key: t(d.json_key) });
    case 'onepassword': return opt({ kind: 'onepassword' as const, reference: t(d.reference), account: t(d.account), json_key: t(d.json_key) });
    case 'vault': return opt({
      kind: 'vault' as const, address: t(d.address), path: t(d.path), key: t(d.key), namespace: t(d.namespace),
      kv_version: d.kv_version === 'auto' ? '' : d.kv_version, ca_file: t(d.ca_file),
    }) as SecretSource;
    case 'aws': return opt({
      kind: 'aws' as const, secret_id: t(d.secret_id), region: t(d.region), profile: t(d.profile), json_key: t(d.json_key),
      fill_username: field === 'password' && d.fill_username,
    });
    case 'azure': return opt({ kind: 'azure' as const, vault_url: t(d.vault_url), name: t(d.name), version: t(d.version), json_key: t(d.json_key) });
    case 'gcp': return opt({ kind: 'gcp' as const, project: t(d.project), secret: t(d.secret), version: t(d.version), json_key: t(d.json_key) });
    case 'keepass': return opt({
      kind: 'keepass' as const, file: t(d.file), entry: t(d.entry), attribute: t(d.attribute), key_file: t(d.key_file),
      master: d.master === 'keychain' ? '' : d.master, master_command: d.master === 'command' ? t(d.master_command) : '',
      fill_username: field === 'password' && d.fill_username,
    }) as SecretSource;
    case 'zoho': return opt({
      kind: 'zoho' as const, data_center: t(d.data_center), secret_name: t(d.secret_name), attribute: t(d.attribute), client_id: t(d.client_id),
      master: d.master === 'keychain' || d.master === 'none' ? '' : d.master, master_command: d.master === 'command' ? t(d.master_command) : '',
      fill_username: field === 'password' && d.fill_username,
    }) as SecretSource;
    default: return null;
  }
}

/** Inputs each kind can't do without, for "Falta completar". */
export const REQUIRED: Record<SecretSourceKind, (keyof SourceDraft)[]> = {
  command: ['command'],
  onepassword: ['reference'],
  vault: ['address', 'path', 'key'],
  aws: ['secret_id'],
  azure: ['vault_url', 'name'],
  gcp: ['project', 'secret'],
  keepass: ['file', 'entry'],
  zoho: ['data_center', 'secret_name', 'client_id'],
};

/** The required inputs a draft left empty (with what its master password
 *  choice needs: the command, or a key file when there's no password). */
export function missingInputs(d: SourceDraft | undefined): (keyof SourceDraft)[] {
  if (!d || !isExternal(d)) return [];
  const need = [...REQUIRED[d.kind as SecretSourceKind]];
  if (d.kind === 'keepass' || d.kind === 'zoho') {
    if (d.master === 'command') need.push('master_command');
    if (d.kind === 'keepass' && d.master === 'none') need.push('key_file');
  }
  return need.filter((k) => String(d[k] ?? '').trim() === '');
}

/** What a draft typed for the keychain, by slot: the Vault token or the
 *  master password kept there (`''`), Zoho's client secret and refresh token. */
export function typedTokens(d: SourceDraft): Record<string, string> {
  const out: Record<string, string> = {};
  const keepsMaster = (d.kind === 'keepass' || d.kind === 'zoho') && d.master === 'keychain';
  if ((d.kind === 'vault' || keepsMaster) && d.token) out[''] = d.token;
  if (d.kind === 'zoho') {
    if (d.client_secret) out.client_secret = d.client_secret;
    if (d.refresh_token) out.refresh_token = d.refresh_token;
  }
  return out;
}

/** A definition's inputs, in the form's order, for the confirmation dialog:
 *  `[input, value]`, every one that has a value. */
export function sourceRows(s: SecretSource): [string, string][] {
  const order = [
    'command', 'reference', 'account', 'address', 'path', 'key', 'namespace', 'kv_version', 'ca_file', 'secret_id', 'region', 'profile', 'vault_url', 'name',
    'project', 'secret', 'version', 'file', 'entry', 'data_center', 'secret_name', 'client_id', 'attribute', 'key_file', 'master', 'master_command', 'json_key',
    'fill_username',
  ];
  const rec = s as unknown as Record<string, unknown>;
  return order.filter((k) => rec[k] !== undefined && rec[k] !== '' && rec[k] !== false).map((k) => [k, String(rec[k])]);
}
