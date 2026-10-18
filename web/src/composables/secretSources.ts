import type { SecretSource, SecretSourceKind } from '../api/types';

// Where a connection's secret field takes its value (docs/secret-sources.md):
// kept in the keychain, asked every time (the password only), or a command /
// secret manager resolved at connect time. The definitions go in the
// connection's options as JSON (`secret_sources`); a Vault token (or a KeePass
// / Zoho Vault master password) typed in the form goes as
// `source_token:<field>`, Zoho's client secret and refresh token as
// `source_token:<field>#<slot>` (Bitwarden's client secret too), and the
// backend moves them to the keychain.
// Pure functions: the form (ConnectionView, SshTunnelSection) and the tests.

export const SOURCES_OPTION = 'secret_sources';
export const TOKEN_PREFIX = 'source_token:';

/** What the picker offers, in order. `ask` only for the password. */
export const SOURCE_KINDS = ['saved', 'ask', 'command', 'onepassword', 'vault', 'aws', 'azure', 'gcp', 'keepass', 'zoho', 'vault_db', 'conjur', 'ccp', 'delinea', 'bitwarden', 'bitwarden_sm', 'ssm'] as const;
export type DraftKind = (typeof SOURCE_KINDS)[number];

/** How DBine signs in to HashiCorp Vault (KV and database credentials). */
export const VAULT_AUTHS = ['token', 'approle'] as const;

/** Where a KeePass / Zoho Vault master password (a Delinea password) comes from. */
export const MASTER_SOURCES = ['keychain', 'ask', 'command', 'none'] as const;
export type MasterKind = (typeof MASTER_SOURCES)[number];

/** Zoho's data centres (dbine_core::secret_sources::ZOHO_DATA_CENTERS): id → domain. */
export const ZOHO_DATA_CENTERS: [string, string][] = [
  ['com', 'zoho.com'], ['eu', 'zoho.eu'], ['in', 'zoho.in'], ['com.au', 'zoho.com.au'], ['jp', 'zoho.jp'], ['ca', 'zohocloud.ca'],
  ['sa', 'zoho.sa'], ['uk', 'zoho.uk'], ['ae', 'zoho.ae'], ['sg', 'zoho.sg'], ['com.cn', 'zoho.com.cn'],
];

/** Bitwarden's clouds (dbine_core::secret_sources::BITWARDEN_CLOUDS); else a self-hosted URL. */
export const BITWARDEN_CLOUDS = ['bitwarden.com', 'bitwarden.eu'] as const;

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
  /** Vault: a token, or AppRole (the role ID here; its secret ID typed below). */
  auth: 'token' | 'approle';
  approle_mount: string;
  role_id: string;
  /** Typed now, like `token` (kept in the `secret_id` slot). */
  approle_secret_id: string;
  /** Vault database credentials. */
  mount: string;
  role: string;
  static_role: boolean;
  /** CyberArk Conjur, CCP and Delinea: the service's URL. */
  url: string;
  conjur_account: string;
  login: string;
  variable: string;
  username_variable: string;
  app_id: string;
  safe: string;
  folder: string;
  object: string;
  account_user: string;
  account_address: string;
  client_cert: string;
  client_key: string;
  /** Delinea. */
  platform_url: string;
  username: string;
  domain: string;
  /** Bitwarden (and Secrets Manager): `bitwarden.com`, `bitwarden.eu` or a URL. */
  server: string;
  item: string;
  /** The entry's field the user comes from (KeePass, Bitwarden, 1Password,
   *  Zoho Vault, Delinea); empty: the provider's own. */
  user_field: string;
  /** AWS Parameter Store. */
  parameter: string;
  list_index: string;
}

export function emptyDraft(kind: DraftKind = 'saved'): SourceDraft {
  return {
    kind, command: '', json_key: '', reference: '', account: '', address: '', path: '', key: '', namespace: '', kv_version: 'auto',
    ca_file: '', token: '', secret_id: '', region: '', profile: '', fill_username: false, vault_url: '', name: '', version: '', project: '', secret: '',
    file: '', entry: '', attribute: '', key_file: '', master: 'keychain', master_command: '', data_center: '', secret_name: '', client_id: '',
    client_secret: '', refresh_token: '', auth: 'token', approle_mount: '', role_id: '', approle_secret_id: '', mount: '', role: '', static_role: false, url: '',
    conjur_account: '', login: '', variable: '', username_variable: '', app_id: '', safe: '', folder: '', object: '', account_user: '', account_address: '',
    client_cert: '', client_key: '', platform_url: '', username: '', domain: '', server: '', item: '', user_field: '', parameter: '', list_index: '',
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
    (d as unknown as Record<string, unknown>)[k] = k === 'fill_username' || k === 'static_role' ? v === true : String(v ?? '');
  }
  if (!['auto', '1', '2'].includes(d.kv_version)) d.kv_version = 'auto';
  if (!(VAULT_AUTHS as readonly string[]).includes(d.auth)) d.auth = 'token';
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
    case 'onepassword': return opt({
      kind: 'onepassword' as const, reference: t(d.reference), account: t(d.account), json_key: t(d.json_key),
      fill_username: field === 'password' && d.fill_username, user_field: field === 'password' && d.fill_username ? t(d.user_field) : '',
    });
    case 'vault': return opt({
      kind: 'vault' as const, address: t(d.address), path: t(d.path), key: t(d.key), namespace: t(d.namespace),
      kv_version: d.kv_version === 'auto' ? '' : d.kv_version, ca_file: t(d.ca_file), ...vaultAuth(d),
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
      fill_username: field === 'password' && d.fill_username, user_field: field === 'password' && d.fill_username ? t(d.user_field) : '',
    }) as SecretSource;
    case 'zoho': return opt({
      kind: 'zoho' as const, data_center: t(d.data_center), secret_name: t(d.secret_name), attribute: t(d.attribute), client_id: t(d.client_id),
      master: d.master === 'keychain' || d.master === 'none' ? '' : d.master, master_command: d.master === 'command' ? t(d.master_command) : '',
      fill_username: field === 'password' && d.fill_username, user_field: field === 'password' && d.fill_username ? t(d.user_field) : '',
    }) as SecretSource;
    case 'vault_db': return opt({
      kind: 'vault_db' as const, address: t(d.address), mount: t(d.mount), role: t(d.role), static_role: d.static_role, namespace: t(d.namespace),
      ca_file: t(d.ca_file), ...vaultAuth(d),
    }) as SecretSource;
    case 'conjur': return opt({
      kind: 'conjur' as const, url: t(d.url), conjur_account: t(d.conjur_account), login: t(d.login), variable: t(d.variable),
      username_variable: field === 'password' ? t(d.username_variable) : '', json_key: t(d.json_key), ca_file: t(d.ca_file),
    }) as SecretSource;
    case 'ccp': return opt({
      kind: 'ccp' as const, url: t(d.url), app_id: t(d.app_id), safe: t(d.safe), folder: t(d.folder), object: t(d.object),
      account_user: t(d.account_user), account_address: t(d.account_address), client_cert: t(d.client_cert),
      client_key: t(d.client_cert) ? t(d.client_key) : '', ca_file: t(d.ca_file), fill_username: field === 'password' && d.fill_username,
    }) as SecretSource;
    case 'delinea': return opt({
      kind: 'delinea' as const, url: t(d.url), platform_url: t(d.platform_url), username: t(d.username), domain: t(d.domain),
      secret_name: t(d.secret_name), attribute: t(d.attribute),
      master: d.master === 'keychain' || d.master === 'none' ? '' : d.master, master_command: d.master === 'command' ? t(d.master_command) : '',
      fill_username: field === 'password' && d.fill_username, user_field: field === 'password' && d.fill_username ? t(d.user_field) : '',
    }) as SecretSource;
    case 'bitwarden': return opt({
      kind: 'bitwarden' as const, server: t(d.server), item: t(d.item), attribute: t(d.attribute), json_key: t(d.json_key), client_id: t(d.client_id),
      master: d.master === 'keychain' || d.master === 'none' ? '' : d.master, master_command: d.master === 'command' ? t(d.master_command) : '',
      fill_username: field === 'password' && d.fill_username, user_field: field === 'password' && d.fill_username ? t(d.user_field) : '',
    }) as SecretSource;
    case 'bitwarden_sm': return opt({ kind: 'bitwarden_sm' as const, server: t(d.server), secret: t(d.secret), json_key: t(d.json_key) });
    case 'ssm': return opt({
      kind: 'ssm' as const, parameter: t(d.parameter), version: t(d.version), region: t(d.region), profile: t(d.profile), list_index: t(d.list_index),
      json_key: t(d.json_key), fill_username: field === 'password' && d.fill_username,
    });
    default: return null;
  }
}

/** Vault's sign-in inputs: none for a token (the default), else AppRole's. */
function vaultAuth(d: SourceDraft): Record<string, string> {
  if (d.auth !== 'approle') return {};
  return { auth: 'approle', approle_mount: d.approle_mount.trim(), role_id: d.role_id.trim() };
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
  vault_db: ['address', 'role'],
  conjur: ['url', 'conjur_account', 'login', 'variable'],
  ccp: ['url', 'app_id'],
  delinea: ['url', 'username', 'secret_name'],
  bitwarden: ['item', 'client_id'],
  bitwarden_sm: ['secret'],
  ssm: ['parameter'],
};

/** The required inputs a draft left empty (with what its master password
 *  choice needs: the command, or a key file when there's no password). */
export function missingInputs(d: SourceDraft | undefined): (keyof SourceDraft)[] {
  if (!d || !isExternal(d)) return [];
  const need = [...REQUIRED[d.kind as SecretSourceKind]];
  if (d.kind === 'keepass' || d.kind === 'zoho' || d.kind === 'delinea' || d.kind === 'bitwarden') {
    if (d.master === 'command') need.push('master_command');
    if (d.kind === 'keepass' && d.master === 'none') need.push('key_file');
  }
  if ((d.kind === 'vault' || d.kind === 'vault_db') && d.auth === 'approle') need.push('role_id');
  // The CCP needs an account to ask for: its object, or its user or address.
  if (d.kind === 'ccp' && ![d.object, d.account_user, d.account_address].some((v) => v.trim())) need.push('object');
  return need.filter((k) => String(d[k] ?? '').trim() === '');
}

/** What a draft typed for the keychain, by slot: the Vault token, the
 *  Secrets Manager access token or the master password kept there (`''`),
 *  Zoho's client secret and refresh token, Bitwarden's client secret. */
export function typedTokens(d: SourceDraft): Record<string, string> {
  const out: Record<string, string> = {};
  const keepsMaster = (d.kind === 'keepass' || d.kind === 'zoho' || d.kind === 'delinea' || d.kind === 'bitwarden') && d.master === 'keychain';
  const vault = d.kind === 'vault' || d.kind === 'vault_db';
  // Vault's token, a master password, Conjur's API key, the CCP key's passphrase, Bitwarden Secrets Manager's access token.
  if (((vault && d.auth !== 'approle') || keepsMaster || d.kind === 'conjur' || d.kind === 'ccp' || d.kind === 'bitwarden_sm') && d.token) out[''] = d.token;
  if (vault && d.auth === 'approle' && d.approle_secret_id) out.secret_id = d.approle_secret_id;
  if (d.kind === 'bitwarden' && d.client_secret) out.client_secret = d.client_secret;
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
    'fill_username', 'mount', 'role', 'static_role', 'auth', 'approle_mount', 'role_id', 'url', 'platform_url', 'conjur_account', 'login', 'variable',
    'username_variable', 'app_id', 'safe', 'folder', 'object', 'account_user', 'account_address', 'client_cert', 'client_key', 'username', 'domain', 'server', 'item', 'user_field', 'parameter', 'list_index',
  ];
  const rec = s as unknown as Record<string, unknown>;
  return order.filter((k) => rec[k] !== undefined && rec[k] !== '' && rec[k] !== false).map((k) => [k, String(rec[k])]);
}

// ---- the selector ------------------------------------------------------------

/** The selector's first group, in this order: kept in the keychain, asked
 *  every time (the database password only), a command line. */
export const LOCAL_KINDS = ['saved', 'ask', 'command'] as const satisfies readonly DraftKind[];

/** The selector's options: the local ones first (`ask` only when it
 *  applies), then the secret managers sorted by their visible name in the
 *  UI's language. `label` gives that name. */
export function orderedKinds(label: (k: DraftKind) => string, locale: string, allowAsk: boolean): { local: DraftKind[]; managers: DraftKind[] } {
  const local = LOCAL_KINDS.filter((k) => k !== 'ask' || allowAsk);
  const managers = SOURCE_KINDS.filter((k) => !(LOCAL_KINDS as readonly string[]).includes(k))
    .map((k) => [k, label(k)] as const)
    .sort((a, b) => a[1].localeCompare(b[1], locale, { sensitivity: 'base' }))
    .map(([k]) => k);
  return { local, managers };
}

// ---- the user ------------------------------------------------------------------

/** Sources that can also give the connection's user ("Usar también el
 *  usuario de la entrada"). */
export const FILLS_USER: readonly DraftKind[] = ['aws', 'keepass', 'zoho', 'ccp', 'delinea', 'bitwarden', 'ssm', 'onepassword'];

/** Sources whose item is an entry with its own user: as the database
 *  credentials they give the user and the password, always (new
 *  definitions). An older definition without `fill_username` keeps the
 *  connection's user field. */
export const ENTRY_KINDS: readonly DraftKind[] = ['keepass', 'bitwarden', 'onepassword', 'delinea', 'zoho', 'ccp'];

/** The entry's own user and password fields (`user_field` / the password's
 *  field left empty means these). */
export const ENTRY_FIELDS: Partial<Record<DraftKind, { user: string; password: string }>> = {
  keepass: { user: 'UserName', password: 'Password' },
  bitwarden: { user: 'username', password: 'password' },
  onepassword: { user: 'username', password: 'password' },
  zoho: { user: 'username', password: 'password' },
  delinea: { user: 'username', password: 'password' },
};

/** The draft's password field for an entry source (1Password: the
 *  reference's field after `op://<vault>/<item>/`). */
export function passwordField(d: SourceDraft): string {
  if (d.kind === 'onepassword') return d.reference.trim().replace(/^op:\/\//, '').split('/').slice(2).join('/');
  return d.attribute;
}
export function setPasswordField(d: SourceDraft, f: string): void {
  if (d.kind === 'onepassword') {
    const item = onePasswordItem(d.reference);
    d.reference = item ? `${item}/${f || 'password'}` : d.reference;
  } else d.attribute = f === ENTRY_FIELDS[d.kind]?.password ? '' : f;
}
/** `op://<vault>/<item>` of a 1Password reference ('' when it isn't one). */
export function onePasswordItem(reference: string): string {
  const parts = reference.trim().replace(/^op:\/\//, '').split('/');
  return reference.trim().startsWith('op://') && parts.length >= 2 && parts[0] && parts[1] ? `op://${parts[0]}/${parts[1]}` : '';
}

/** The password's source gives the user too: the form hides its user field. */
export function userFromSource(d: SourceDraft | undefined): boolean {
  if (!d) return false;
  if (d.kind === 'vault_db') return true;
  if (d.kind === 'conjur') return !!d.username_variable.trim();
  return FILLS_USER.includes(d.kind) && d.fill_username;
}

// ---- browsing ------------------------------------------------------------------

/** Per source that can list its items: the input the chosen item goes to,
 *  and the one its chosen field (or JSON key) goes to. */
export const BROWSE: Partial<Record<DraftKind, { item: keyof SourceDraft; field?: keyof SourceDraft }>> = {
  keepass: { item: 'entry', field: 'attribute' },
  onepassword: { item: 'reference' },
  bitwarden: { item: 'item', field: 'attribute' },
  bitwarden_sm: { item: 'secret', field: 'json_key' },
  vault: { item: 'path', field: 'key' },
  vault_db: { item: 'role' },
  aws: { item: 'secret_id', field: 'json_key' },
  ssm: { item: 'parameter', field: 'json_key' },
  azure: { item: 'name', field: 'json_key' },
  gcp: { item: 'secret', field: 'json_key' },
  zoho: { item: 'secret_name', field: 'attribute' },
  conjur: { item: 'variable', field: 'json_key' },
  delinea: { item: 'secret_name', field: 'attribute' },
};

/** The field step offers "the whole value" (a JSON key is optional there). */
export function wholeValueAllowed(kind: DraftKind): boolean {
  return BROWSE[kind]?.field === 'json_key';
}

/** What still has to be filled before browsing: the required inputs but
 *  the item (and its field) the browse chooses. */
export function missingForBrowse(d: SourceDraft, editing = false): (keyof SourceDraft)[] {
  const b = BROWSE[d.kind];
  if (!b) return [];
  const need = missingInputs(d).filter((k) => k !== b.item && k !== b.field);
  // A new connection has nothing kept: what signs in has to be typed.
  if (!editing) {
    const masterKept = ['keepass', 'delinea', 'bitwarden'].includes(d.kind) && d.master === 'keychain';
    if ((masterKept || d.kind === 'conjur') && !d.token) need.push('token');
    if ((d.kind === 'bitwarden' || d.kind === 'zoho') && !d.client_secret) need.push('client_secret');
    if (d.kind === 'zoho' && !d.refresh_token) need.push('refresh_token');
  }
  return need;
}

/** Put what was chosen in the draft. `field`: the chosen field name, `''`
 *  for the whole value, `null` when the provider has no field step. */
export function applyBrowse(d: SourceDraft, entry: { id: string; path?: string }, field: string | null, userField?: string): void {
  const b = BROWSE[d.kind];
  if (!b) return;
  const defaults = ENTRY_FIELDS[d.kind];
  if (userField !== undefined) d.user_field = userField === defaults?.user ? '' : userField;
  if (d.kind === 'onepassword') {
    // The entry is `op://<vault>/<item>`; the field goes after it.
    d.reference = field ? `${entry.id}/${field}` : entry.id;
    return;
  }
  (d as unknown as Record<string, string>)[b.item] = entry.id;
  if (d.kind === 'vault_db') d.static_role = entry.path === 'static-roles';
  // The entry's own password field is left empty (the default).
  if (b.field && field !== null) (d as unknown as Record<string, string>)[b.field] = defaults && field === defaults.password ? '' : field;
}

/** The `item` / `path` a browse asks an entry's field names with, or null
 *  while there's no entry. */
export function fieldsQuery(d: SourceDraft): { item: string; path?: string } | null {
  if (d.kind === 'onepassword') {
    const item = onePasswordItem(d.reference);
    return item ? { item, path: item.slice('op://'.length).split('/')[0] } : null;
  }
  const b = BROWSE[d.kind];
  const v = b ? String(d[b.item] ?? '').trim() : '';
  return v ? { item: v } : null;
}
