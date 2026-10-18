// "Conectar <gestor>" (components/AccountConnectWizard.vue) for every
// secret manager but Zoho Vault (its own wizard, zohoWizard.ts): the steps
// of each, what each step needs before going on, and what the last step
// sends (`connect_secret_account`: the account's config and the secrets,
// by keychain slot). Pure, for the wizard and its tests.
//
// The labels quoted in the wizard's texts are the providers' own, checked
// against their documentation (docs/secret-sources.md, "Accounts").

import type { SecretAccount, SecretAccountConfig, SecretAccountKind } from '../api/types';

/** The manager's name (brand names: the same in every language). */
export const PROVIDER: Record<SecretAccountKind, string> = {
  zoho: 'Zoho Vault', bitwarden: 'Bitwarden', bitwarden_sm: 'Bitwarden Secrets Manager', vault: 'HashiCorp Vault', delinea: 'Delinea',
  conjur: 'CyberArk Conjur', ccp: 'CyberArk CCP', onepassword: '1Password', aws: 'AWS', azure: 'Azure Key Vault', gcp: 'Google Cloud', keepass: 'KeePass',
};

/** `host[:port]` of a URL ('' when it isn't one). */
export function hostOf(url: string): string {
  const m = /^[a-z]+:\/\/([^/?#]+)/i.exec((url ?? '').trim());
  return m ? m[1] : '';
}

export type WizardKind = 'bitwarden' | 'bitwarden_sm' | 'vault' | 'delinea' | 'conjur' | 'ccp' | 'onepassword' | 'aws' | 'azure' | 'gcp' | 'keepass';

/** Every kind with this wizard, in Settings' order. */
export const WIZARD_KINDS: WizardKind[] = ['bitwarden', 'bitwarden_sm', 'vault', 'delinea', 'conjur', 'ccp', 'onepassword', 'aws', 'azure', 'gcp', 'keepass'];

/** The steps of each kind; the last one checks live and saves. */
export const STEPS: Record<WizardKind, string[]> = {
  bitwarden: ['server', 'apiKey', 'master', 'connect'],
  bitwarden_sm: ['server', 'token', 'connect'],
  vault: ['server', 'auth', 'connect'],
  delinea: ['server', 'user', 'connect'],
  conjur: ['server', 'identity', 'connect'],
  ccp: ['server', 'certificate', 'connect'],
  onepassword: ['cli', 'connect'],
  aws: ['profile', 'connect'],
  azure: ['vault', 'connect'],
  gcp: ['project', 'connect'],
  keepass: ['database', 'master', 'connect'],
};

export type Master = 'keychain' | 'ask' | 'command' | 'none';

/** Everything a wizard asks, as text. Secrets (`client_secret`, `passphrase`,
 *  `token`, `secret_id`, `password`, `api_key`) are wiped when it closes. */
export interface WizardForm {
  /** 1-based. */
  step: number;
  /** Bitwarden: `bitwarden.com`, `bitwarden.eu` or `self` (then `server_url`). */
  server: string;
  server_url: string;
  client_id: string;
  client_secret: string;
  master: Master;
  master_command: string;
  passphrase: string;
  token: string;
  address: string;
  namespace: string;
  ca_file: string;
  auth: 'token' | 'approle';
  approle_mount: string;
  role_id: string;
  secret_id: string;
  /** Delinea: Secret Server's own sign-in, or Delinea Platform's. */
  delinea_mode: 'server' | 'platform';
  url: string;
  platform_url: string;
  username: string;
  domain: string;
  password: string;
  conjur_account: string;
  login: string;
  api_key: string;
  app_id: string;
  client_cert: string;
  client_key: string;
  op_account: string;
  profile: string;
  region: string;
  tenant: string;
  vault_url: string;
  project: string;
  file: string;
  key_file: string;
  label: string;
}

export function emptyForm(): WizardForm {
  return {
    step: 1, server: 'bitwarden.com', server_url: '', client_id: '', client_secret: '', master: 'keychain', master_command: '', passphrase: '', token: '',
    address: '', namespace: '', ca_file: '', auth: 'token', approle_mount: '', role_id: '', secret_id: '', delinea_mode: 'server', url: '', platform_url: '',
    username: '', domain: '', password: '', conjur_account: '', login: '', api_key: '', app_id: '', client_cert: '', client_key: '', op_account: '',
    profile: '', region: '', tenant: '', vault_url: '', project: '', file: '', key_file: '', label: '',
  };
}

/** The secret inputs, wiped when the wizard closes. */
export const SECRET_INPUTS: (keyof WizardForm)[] = ['client_secret', 'passphrase', 'token', 'secret_id', 'password', 'api_key'];

// ---- what's well formed (as the backend checks it) ----------------------------------

const LOCAL = /^(localhost|127(\.\d{1,3}){3}|\[::1\])(:\d{1,5})?$/i;

/** `https://host[:port][/path]`, or `http://` to this computer only; no
 *  credentials, query, fragment or spaces. */
export function serviceUrl(u: string): boolean {
  const m = /^(https?):\/\/([^/?#@\s\\]+)(\/[^?#@\s\\]*)?$/i.exec(u.trim());
  if (!m || /\/\.\.(\/|$)/.test(m[3] ?? '')) return false;
  return m[1].toLowerCase() === 'https' || LOCAL.test(m[2]);
}
export const validBitwardenClientId = (v: string) => /^user\.[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(v.trim());
/** A machine account's access token: `0.<id>.<secret>:<key>`. */
export const validAccessToken = (v: string) => /^0\.[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.[^:\s]+:[A-Za-z0-9+/=]+$/i.test(v.trim());
export const validAwsRegion = (v: string) => /^[a-z0-9]+(-[a-z0-9]+)+-\d+$/.test(v.trim());
export const validAwsProfile = (v: string) => /^[A-Za-z0-9.@+_][A-Za-z0-9.@+_-]{0,127}$/.test(v.trim());
export const validAzureVault = (v: string) => /^https:\/\/[a-z0-9-]+\.vault\.(azure\.net|azure\.cn|usgovcloudapi\.net|microsoftazure\.de)\/?$/i.test(v.trim());
/** A Microsoft Entra tenant, its ID or a domain: dbine_core's `valid_azure_tenant`. */
export const validTenant = (v: string) => /^[A-Za-z0-9][A-Za-z0-9.-]{0,254}$/.test(v.trim());
export const validGcpProject = (v: string) => /^[A-Za-z0-9._-]{1,255}$/.test(v.trim());
export const validDelineaPlatform = (v: string) => /^https:\/\/[a-z0-9-]+\.delinea\.app\/?$/i.test(v.trim());
export const validOpAccount = (v: string) => /^[A-Za-z0-9._@][A-Za-z0-9._@-]{0,254}$/.test(v.trim());

// ---- Azure: the tenants the CLI knows ------------------------------------------------

/** A tenant from `detect_secret_account` (`az account tenant list`). */
export interface AzureTenant {
  id: string;
  display_name: string;
  default_domain: string;
}

/** The detected tenants, well formed only, once each; the CLI's current one
 *  is always among them. */
export function azureTenants(detected: Record<string, unknown> | null | undefined): AzureTenant[] {
  const str = (v: unknown) => (typeof v === 'string' ? v.trim() : '');
  const out: AzureTenant[] = [];
  const add = (t: AzureTenant) => {
    if (validTenant(t.id) && !out.some((o) => o.id.toLowerCase() === t.id.toLowerCase())) out.push(t);
  };
  for (const t of Array.isArray(detected?.tenants) ? (detected!.tenants as Record<string, unknown>[]) : []) {
    if (t && typeof t === 'object') add({ id: str(t.id), display_name: str(t.display_name), default_domain: str(t.default_domain) });
  }
  const current = str(detected?.tenant);
  if (current && !out.some((o) => o.id.toLowerCase() === current.toLowerCase())) add({ id: current, display_name: '', default_domain: '' });
  return out;
}

/** The tenants matching what's typed in the select (name, domain or ID). */
export function filterTenants(list: AzureTenant[], query: string): AzureTenant[] {
  const q = query.trim().toLowerCase();
  if (!q) return list;
  return list.filter((t) => [t.id, t.display_name, t.default_domain].some((v) => v.toLowerCase().includes(q)));
}

/** The select's options: the listed tenants matching what's typed, then
 *  what's typed itself as one more option, only when it's a well-formed
 *  tenant (valid_azure_tenant) that no listed tenant's ID or domain equals.
 *  It goes last, so Enter (the first option) picks a listed tenant whenever
 *  one matches. `selected`: a typed tenant chosen before stays an option. */
export function tenantChoices(list: AzureTenant[], query: string, selected = ''): (AzureTenant & { typed?: boolean })[] {
  const out: (AzureTenant & { typed?: boolean })[] = [...filterTenants(list, query)];
  const known = (v: string) => list.some((t) => [t.id, t.default_domain].some((x) => x && x.toLowerCase() === v.toLowerCase()));
  for (const v of [query.trim(), query.trim() ? '' : selected.trim()]) {
    if (v && validTenant(v) && !known(v) && !out.some((t) => t.id.toLowerCase() === v.toLowerCase())) out.push({ id: v, display_name: '', default_domain: '', typed: true });
  }
  return out;
}

/** A tenant in the select once chosen: its name and domain, else its ID. */
export function tenantLabel(t: AzureTenant): string {
  return [t.display_name, t.default_domain].filter(Boolean).join(' · ') || t.id;
}

/** The tenant of the backend's "no sign-in in that tenant" message
 *  (azure.rs `no_sign_in`: «az login --tenant <id>», the same in every
 *  language), or null for any other message. */
export function tenantSignIn(message: string): string | null {
  const m = /az login --tenant ([A-Za-z0-9][A-Za-z0-9.-]*[A-Za-z0-9]|[A-Za-z0-9])/.exec(message ?? '');
  return m && validTenant(m[1]) ? m[1] : null;
}

/** Bitwarden's server as the account keeps it. */
export function bitwardenServer(s: WizardForm): string {
  return s.server === 'self' ? s.server_url.trim().replace(/\/+$/, '') : s.server;
}

/** The master password's inputs: the password itself unless it's a command (or none). */
function masterProblems(s: WizardForm, secret: keyof WizardForm, allowNone: boolean, out: string[]) {
  if (s.master === 'none' && !allowNone) out.push('master');
  else if (s.master === 'command') {
    if (!s.master_command.trim()) out.push('master_command');
    else if (/[\r\n]/.test(s.master_command)) out.push('master_command_format');
  } else if (s.master !== 'none' && String(s[secret]) === '') out.push(secret);
}

/** What step `step` still lacks: input names, or `<input>_format` for one
 *  that isn't well formed. */
export function stepProblems(kind: WizardKind, s: WizardForm, step = s.step): string[] {
  const out: string[] = [];
  const key = STEPS[kind][step - 1];
  const empty = (k: keyof WizardForm) => String(s[k]).trim() === '';
  const need = (k: keyof WizardForm, ok?: (v: string) => boolean) => {
    if (empty(k)) out.push(k);
    else if (ok && !ok(String(s[k]))) out.push(`${k}_format`);
  };
  switch (`${kind}.${key}`) {
    case 'bitwarden.server':
    case 'bitwarden_sm.server':
      if (s.server === 'self') need('server_url', serviceUrl);
      break;
    case 'bitwarden.apiKey':
      need('client_id', validBitwardenClientId);
      need('client_secret');
      break;
    case 'bitwarden.master':
      masterProblems(s, 'passphrase', false, out);
      break;
    case 'bitwarden_sm.token':
      need('token', validAccessToken);
      break;
    case 'vault.server':
      need('address', serviceUrl);
      break;
    case 'vault.auth':
      // A token may be left empty: VAULT_TOKEN or ~/.vault-token then.
      if (s.auth === 'approle') need('role_id');
      break;
    case 'delinea.server':
      need('url', serviceUrl);
      if (s.delinea_mode === 'platform') need('platform_url', validDelineaPlatform);
      break;
    case 'delinea.user':
      need('username');
      masterProblems(s, 'password', false, out);
      break;
    case 'conjur.server':
      need('url', serviceUrl);
      need('conjur_account', (v) => !/[/?#]/.test(v));
      break;
    case 'conjur.identity':
      need('login');
      need('api_key');
      break;
    case 'ccp.server':
      need('url', serviceUrl);
      need('app_id');
      break;
    case 'ccp.certificate':
      if (!s.client_cert.trim() && s.client_key.trim()) out.push('client_cert');
      break;
    case 'onepassword.cli':
      need('op_account', validOpAccount);
      break;
    case 'aws.profile':
      if (!empty('profile') && !validAwsProfile(s.profile)) out.push('profile_format');
      need('region', validAwsRegion);
      break;
    case 'azure.vault':
      need('vault_url', validAzureVault);
      if (!empty('tenant') && !validTenant(s.tenant)) out.push('tenant_format');
      break;
    case 'gcp.project':
      need('project', validGcpProject);
      break;
    case 'keepass.database':
      need('file');
      break;
    case 'keepass.master':
      if (s.master === 'none' && !s.key_file.trim()) out.push('key_file');
      masterProblems(s, 'passphrase', true, out);
      break;
    default:
      break;
  }
  return out;
}

export function canAdvance(kind: WizardKind, s: WizardForm): boolean {
  return stepProblems(kind, s).length === 0;
}

export function isLast(kind: WizardKind, s: WizardForm): boolean {
  return s.step === STEPS[kind].length;
}

/** The next step, when this one is complete (never past the last). */
export function next(kind: WizardKind, s: WizardForm): boolean {
  if (!canAdvance(kind, s) || isLast(kind, s)) return false;
  s.step += 1;
  return true;
}

/** Back one step (nothing is pending server side: the check is the last call). */
export function back(s: WizardForm): void {
  if (s.step > 1) s.step -= 1;
}

const opt = <T extends object>(o: T): T => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== '' && v !== undefined)) as T;

/** The account's config, as `connect_secret_account` takes it. */
export function configOf(kind: WizardKind, s: WizardForm): SecretAccountConfig {
  const t = (v: string) => v.trim();
  const master = (allowNone: boolean) => ({
    master: s.master === 'keychain' || (s.master === 'none' && !allowNone) ? undefined : s.master,
    master_command: s.master === 'command' ? t(s.master_command) : undefined,
  });
  switch (kind) {
    case 'bitwarden': return opt({ kind, server: bitwardenServer(s), client_id: t(s.client_id), ...master(false) }) as SecretAccountConfig;
    case 'bitwarden_sm': return { kind, server: bitwardenServer(s) };
    case 'vault': return opt({
      kind, address: t(s.address).replace(/\/+$/, ''), namespace: t(s.namespace), ca_file: t(s.ca_file),
      ...(s.auth === 'approle' ? { auth: 'approle' as const, approle_mount: t(s.approle_mount), role_id: t(s.role_id) } : {}),
    });
    case 'delinea': return opt({
      kind, url: t(s.url).replace(/\/+$/, ''), platform_url: s.delinea_mode === 'platform' ? t(s.platform_url).replace(/\/+$/, '') : '',
      username: t(s.username), domain: s.delinea_mode === 'platform' ? '' : t(s.domain), ...master(false),
    }) as SecretAccountConfig;
    case 'conjur': return opt({ kind, url: t(s.url).replace(/\/+$/, ''), conjur_account: t(s.conjur_account), login: t(s.login), ca_file: t(s.ca_file) });
    case 'ccp': return opt({ kind, url: t(s.url).replace(/\/+$/, ''), app_id: t(s.app_id), client_cert: t(s.client_cert), client_key: t(s.client_cert) ? t(s.client_key) : '', ca_file: t(s.ca_file) });
    case 'onepassword': return { kind, account: t(s.op_account) };
    case 'aws': return opt({ kind, profile: t(s.profile), region: t(s.region) });
    case 'azure': return opt({ kind, tenant: t(s.tenant), vault_url: t(s.vault_url).replace(/\/+$/, '') });
    case 'gcp': return { kind, project: t(s.project) };
    case 'keepass': return opt({ kind, file: t(s.file), key_file: t(s.key_file), ...master(true) }) as SecretAccountConfig;
  }
}

/** The secrets the check signs in with, by keychain slot: only what the
 *  kind uses, only what was typed (reconnecting, an empty one keeps the
 *  account's, while it goes to the same place). A master password asked
 *  when connecting goes too: it's checked, never kept. */
export function secretsOf(kind: WizardKind, s: WizardForm): Record<string, string> {
  const out: Record<string, string> = {};
  const put = (slot: string, v: string) => { if (v !== '') out[slot] = v; };
  const masterTyped = s.master === 'keychain' || s.master === 'ask';
  switch (kind) {
    case 'bitwarden':
      put('client_secret', s.client_secret.trim());
      if (masterTyped) put('passphrase', s.passphrase);
      break;
    case 'bitwarden_sm': put('token', s.token.trim()); break;
    case 'vault':
      if (s.auth === 'approle') put('secret_id', s.secret_id.trim());
      else put('token', s.token.trim());
      break;
    case 'delinea': if (masterTyped) put('password', s.password); break;
    case 'conjur': put('api_key', s.api_key.trim()); break;
    case 'ccp': if (s.client_cert.trim()) put('passphrase', s.passphrase); break;
    case 'keepass': if (masterTyped) put('passphrase', s.passphrase); break;
    default: break;
  }
  return out;
}

/** A wizard reconnecting an account: its inputs again (never its secrets). */
export function formFromAccount(a: SecretAccount): Partial<WizardForm> {
  const r = a as unknown as Record<string, string | undefined>;
  const v = (k: string) => r[k] ?? '';
  const master = (r.master as Master | undefined) ?? 'keychain';
  switch (a.kind) {
    case 'bitwarden':
    case 'bitwarden_sm': {
      const sv = v('server');
      const cloud = sv === '' || sv === 'bitwarden.com' || sv === 'bitwarden.eu';
      return { server: cloud ? sv || 'bitwarden.com' : 'self', server_url: cloud ? '' : sv, client_id: v('client_id'), master, master_command: v('master_command'), label: a.label };
    }
    case 'vault': return { address: v('address'), namespace: v('namespace'), ca_file: v('ca_file'), auth: r.auth === 'approle' ? 'approle' : 'token', approle_mount: v('approle_mount'), role_id: v('role_id'), label: a.label };
    case 'delinea': return { url: v('url'), platform_url: v('platform_url'), delinea_mode: v('platform_url') ? 'platform' : 'server', username: v('username'), domain: v('domain'), master, master_command: v('master_command'), label: a.label };
    case 'conjur': return { url: v('url'), conjur_account: v('conjur_account'), login: v('login'), ca_file: v('ca_file'), label: a.label };
    case 'ccp': return { url: v('url'), app_id: v('app_id'), client_cert: v('client_cert'), client_key: v('client_key'), ca_file: v('ca_file'), label: a.label };
    case 'onepassword': return { op_account: v('account'), label: a.label };
    case 'aws': return { profile: v('profile'), region: v('region'), label: a.label };
    case 'azure': return { tenant: v('tenant'), vault_url: v('vault_url'), label: a.label };
    case 'gcp': return { project: v('project'), label: a.label };
    case 'keepass': return { file: v('file'), key_file: v('key_file'), master, master_command: v('master_command'), label: a.label };
    default: return { label: a.label };
  }
}

/** Reconnecting, a secret left empty keeps the account's: only when the
 *  account goes to the same place (as the backend decides it). */
export function keepsSecrets(kind: WizardKind, a: SecretAccount | null | undefined, s: WizardForm): boolean {
  if (!a || a.kind !== kind) return false;
  const now = configOf(kind, s) as unknown as Record<string, unknown>;
  const was = a as unknown as Record<string, unknown>;
  const same = (keys: string[]) => keys.every((k) => String(now[k] ?? '').replace(/\/+$/, '') === String(was[k] ?? '').replace(/\/+$/, ''));
  switch (kind) {
    case 'bitwarden': return same(['server', 'client_id']);
    case 'bitwarden_sm': return same(['server']);
    case 'vault': return same(['address', 'namespace', 'auth', 'approle_mount', 'role_id']);
    case 'delinea': return same(['url', 'platform_url', 'username', 'domain']);
    case 'conjur': return same(['url', 'conjur_account', 'login']);
    case 'ccp': return same(['url', 'client_cert', 'client_key']);
    case 'keepass': return same(['file', 'key_file']);
    default: return true;
  }
}

/** Backend messages that send the user back to a step (by step key). */
export function stepForError(kind: WizardKind, message: string): number | null {
  const at = (key: string) => {
    const i = STEPS[kind].indexOf(key);
    return i < 0 ? null : i + 1;
  };
  const m = message.toLowerCase();
  if (m.includes('contraseña maestra') && kind !== 'delinea') return at('master');
  if (m.includes('clave de api') && kind === 'bitwarden') return at('apiKey');
  if (m.includes('token de acceso de la cuenta de máquina') || m.includes('token de acceso de bitwarden secrets manager')) return at('token');
  if (kind === 'vault' && (m.includes('token') || m.includes('role_id') || m.includes('secret_id'))) return at('auth');
  if (kind === 'delinea' && m.includes('rechazó el usuario')) return at('user');
  if (kind === 'conjur' && (m.includes('rechazó el login') || m.includes('clave de api'))) return at('identity');
  if (kind === 'ccp' && (m.includes('certificado') || m.includes('clave privada') || m.includes('pkcs#12'))) return at('certificate');
  if (kind === 'azure' && tenantSignIn(message)) return at('vault');
  if (m.includes('no se pudo conectar')) return 1;
  return null;
}

/** Where the last step sends the credentials (shown before it runs): the
 *  server typed, a provider's fixed endpoint, or the local tool that holds
 *  them (1Password's, Azure's and Google's CLIs; a file for KeePass). */
export function destinations(kind: WizardKind, s: WizardForm): string[] {
  const region = s.region.trim() || 'us-east-1';
  switch (kind) {
    case 'bitwarden':
    case 'bitwarden_sm':
      return s.server === 'self' ? [hostOf(s.server_url)].filter(Boolean) : [`identity.${s.server}`, `api.${s.server}`];
    case 'vault': return [hostOf(s.address)].filter(Boolean);
    case 'delinea': return [s.delinea_mode === 'platform' ? hostOf(s.platform_url) : '', hostOf(s.url)].filter(Boolean);
    case 'conjur':
    case 'ccp': return [hostOf(s.url)].filter(Boolean);
    case 'onepassword': return ['op (1Password CLI)'];
    case 'aws': return [region.startsWith('cn-') ? `sts.${region}.amazonaws.com.cn` : `sts.${region}.amazonaws.com`];
    case 'azure': return ['az (Microsoft Entra ID)', hostOf(s.vault_url)].filter(Boolean);
    case 'gcp': return ['gcloud', 'secretmanager.googleapis.com'];
    case 'keepass': return [s.file.trim()].filter(Boolean);
  }
}

/** The name the account gets when the label is left empty (the backend
 *  makes it the same way, with who signed in). */
export function suggestedLabel(kind: WizardKind, s: WizardForm, identity: string): string {
  const p = PROVIDER[kind];
  const who = (() => {
    switch (kind) {
      case 'bitwarden_sm': return s.server === 'self' ? hostOf(s.server_url) : s.server;
      case 'vault': return hostOf(s.address);
      case 'ccp': return [s.app_id.trim(), hostOf(s.url)].filter(Boolean).join(' · ');
      case 'aws': return `${s.profile.trim() || 'default'} · ${s.region.trim()}`;
      case 'azure': return hostOf(s.vault_url).split('.')[0];
      case 'gcp': return s.project.trim();
      default: return identity;
    }
  })();
  return who.trim() ? `${p} · ${who.trim()}` : p;
}
