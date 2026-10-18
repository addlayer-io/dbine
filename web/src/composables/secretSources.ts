import type { SecretSource, SecretSourceKind } from '../api/types';

// Where a connection's secret field takes its value (docs/secret-sources.md):
// kept in the keychain, asked every time (the password only), or a command /
// secret manager resolved at connect time. The definitions go in the
// connection's options as JSON (`secret_sources`); a Vault token typed in the
// form goes as `source_token:<field>` and the backend moves it to the keychain.
// Pure functions: the form (ConnectionView, SshTunnelSection) and the tests.

export const SOURCES_OPTION = 'secret_sources';
export const TOKEN_PREFIX = 'source_token:';

/** What the picker offers, in order. `ask` only for the password. */
export const SOURCE_KINDS = ['saved', 'ask', 'command', 'onepassword', 'vault', 'aws', 'azure', 'gcp'] as const;
export type DraftKind = (typeof SOURCE_KINDS)[number];

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
}

export function emptyDraft(kind: DraftKind = 'saved'): SourceDraft {
  return {
    kind, command: '', json_key: '', reference: '', account: '', address: '', path: '', key: '', namespace: '', kv_version: 'auto',
    ca_file: '', token: '', secret_id: '', region: '', profile: '', fill_username: false, vault_url: '', name: '', version: '', project: '', secret: '',
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
};

/** The required inputs a draft left empty. */
export function missingInputs(d: SourceDraft | undefined): (keyof SourceDraft)[] {
  if (!d || !isExternal(d)) return [];
  return REQUIRED[d.kind as SecretSourceKind].filter((k) => String(d[k] ?? '').trim() === '');
}

/** A definition's inputs, in the form's order, for the confirmation dialog:
 *  `[input, value]`, every one that has a value. */
export function sourceRows(s: SecretSource): [string, string][] {
  const order = ['command', 'reference', 'account', 'address', 'path', 'key', 'namespace', 'kv_version', 'ca_file', 'secret_id', 'region', 'profile', 'vault_url', 'name', 'project', 'secret', 'version', 'json_key', 'fill_username'];
  const rec = s as unknown as Record<string, unknown>;
  return order.filter((k) => rec[k] !== undefined && rec[k] !== '' && rec[k] !== false).map((k) => [k, String(rec[k])]);
}
