import type { ConnectionConfig, DriverInfo, Field, TlsLevel } from '../api/types';

// The engine's own TLS options in the connection form (dbine_driver::TlsRole;
// docs/engine-support.md, "Connection encryption"). A saved connection from
// before a driver had them has no value for them: it behaves as its legacy
// `encrypt` / `trust_server_certificate` pair says (the field's `legacy`
// table), and saving it again without touching the SSL tab must leave it
// exactly as it was: no native key written, the two bools kept.

/** A form value: checkboxes hold booleans, everything else text. */
export type FormValue = string | boolean;

/** Where (encrypt, trust) falls in a legacy table: [ff, ft, tf, tt]. */
export function legacyIndex(encrypt: boolean, trust: boolean): number {
  return (encrypt ? 2 : 0) + (trust ? 1 : 0);
}

/** A field's value for a connection saved before it existed; null: none. */
export function legacyValue(f: Field, encrypt: boolean, trust: boolean): string | null {
  const v = f.legacy?.[legacyIndex(encrypt, trust)];
  return v ? v : null;
}

/** The text a form value is saved as. */
export function asText(v: FormValue | undefined | null): string {
  return typeof v === 'boolean' ? (v ? 'true' : 'false') : String(v ?? '');
}

/** The form value of a saved text, for that field. */
export function formValue(f: Field, v: string): FormValue {
  return f.kind.type === 'bool' ? v === 'true' : v;
}

/** The driver has its engine's own TLS options (it's migrated). */
export function hasTlsFields(d: DriverInfo): boolean {
  return d.fields.some((f) => !!f.tls);
}

/** What an existing connection had before the form changed anything. */
export interface TlsOriginal {
  encrypt: boolean;
  trust: boolean;
  /** The TLS fields it had no value for, with the legacy value shown. */
  legacy: Record<string, string>;
}

/** An existing connection's TLS fields that it has no value for: each one
 *  shows its legacy value (and is remembered, to not write it back). */
export function legacyTlsValues(d: DriverInfo, cfg: ConnectionConfig): TlsOriginal {
  const legacy: Record<string, string> = {};
  for (const f of d.fields) {
    if (!f.tls || !f.legacy) continue;
    if ((cfg.options[f.key] ?? '') !== '') continue;
    const v = legacyValue(f, cfg.encrypt, cfg.trust_server_certificate);
    if (v !== null) legacy[f.key] = v;
  }
  return { encrypt: cfg.encrypt, trust: cfg.trust_server_certificate, legacy };
}

const ORDER: TlsLevel[] = ['off', 'optional', 'unverified', 'verify_ca', 'verify_full'];
const lower = (a: TlsLevel, b: TlsLevel): TlsLevel => (ORDER.indexOf(a) <= ORDER.indexOf(b) ? a : b);

/** The level a set of values gives (as ConnectionConfig::tls_level): the
 *  mode field's, lowered by a skip-verify / skip-hostname that is on. Null
 *  when there's no mode field or its value isn't one of its own. */
export function tlsLevel(fields: Field[], get: (key: string) => FormValue | undefined): TlsLevel | null {
  const mode = fields.find((f) => f.tls?.role === 'mode');
  if (!mode || mode.tls?.role !== 'mode') return null;
  const value = asText(get(mode.key));
  let level = mode.tls.levels.find(([v]) => v === value)?.[1] ?? null;
  if (level === null || level === 'off' || level === 'optional') return level;
  for (const f of fields) {
    const r = f.tls;
    if (!r) continue;
    const on = asText(get(f.key)) === 'true';
    if (r.role === 'skip_verify' && on !== !!r.inverted) level = lower(level, 'unverified');
    if (r.role === 'skip_hostname' && on) level = lower(level, 'verify_ca');
  }
  return level;
}

/** The closest legacy (encrypt, trust) pair of a level (TlsLevel::legacy_pair). */
export function legacyPair(level: TlsLevel): [boolean, boolean] {
  if (level === 'off' || level === 'optional') return [false, false];
  if (level === 'unverified') return [true, true];
  return [true, false];
}

/** TLS is required at this level (not plaintext, not "if offered"). */
export const tlsOn = (level: TlsLevel | null): boolean => level !== null && level !== 'off' && level !== 'optional';

/** A new connection's port: the TLS one when the default mode is TLS. */
export function newConnectionPort(d: DriverInfo): string {
  const level = tlsLevel(d.fields, (k) => {
    const f = d.fields.find((x) => x.key === k);
    return f ? formValue(f, f.default) : undefined;
  });
  const port = d.tls_port && tlsOn(level) ? d.tls_port : d.default_port;
  return String(port || '');
}

/** The port after the mode changed to `level`: the other default swapped
 *  for this one's (8123 ↔ 8443), anything the user typed kept. */
export function portForLevel(d: DriverInfo, port: string, level: TlsLevel | null): string {
  if (!d.tls_port || !d.default_port || d.tls_port === d.default_port) return port;
  const [from, to] = tlsOn(level) ? [d.default_port, d.tls_port] : [d.tls_port, d.default_port];
  return port.trim() === String(from) ? String(to) : port;
}

/** What the SSL tab's chip says: 'off' (not encrypted), 'unverified' (the
 *  certificate isn't checked) or null (nothing to warn about). The legacy
 *  `encrypt` / `trust_server_certificate` checkboxes count too. */
export function tlsWarning(d: DriverInfo, get: (key: string) => FormValue | undefined): 'off' | 'unverified' | null {
  if (hasTlsFields(d)) {
    const level = tlsLevel(d.fields, get);
    if (level === null) return null;
    if (!tlsOn(level)) return 'off';
    return level === 'unverified' ? 'unverified' : null;
  }
  if (!d.fields.some((f) => f.key === 'encrypt')) return null;
  if (get('encrypt') !== true) return 'off';
  return d.fields.some((f) => f.key === 'trust_server_certificate') && get('trust_server_certificate') === true ? 'unverified' : null;
}

/** Writes the TLS fields into `cfg` (buildConfig). `shown`: the fields that
 *  apply (their `when`). `original`: the existing connection's, null for a
 *  new one.
 *
 *  - An existing connection whose legacy-resolved fields are all unchanged
 *    writes none of them and keeps its encrypt / trust bools: it behaves
 *    exactly as before (design risk R1).
 *  - Otherwise every shown TLS field is written with its value, and the
 *    bools get the closest legacy pair of the level (dual-write), so an
 *    older app or driver gets the nearest behaviour instead of plaintext. */
export function writeTls(
  shown: Field[],
  values: Record<string, FormValue | undefined>,
  original: TlsOriginal | null,
  cfg: ConnectionConfig,
): void {
  const tls = shown.filter((f) => !!f.tls);
  if (!tls.length) return;
  // Only a CA file or a server name (the legacy checkboxes still set the
  // bools): written as they are.
  if (!tls.some((f) => f.legacy || f.tls?.role === 'mode')) {
    for (const f of tls) if (asText(values[f.key]) !== '') cfg.options[f.key] = asText(values[f.key]);
    return;
  }
  const untouched = (f: Field) => original !== null && f.key in original.legacy && asText(values[f.key]) === original.legacy[f.key];
  const keepLegacy = original !== null && tls.filter((f) => f.legacy).every(untouched);
  for (const f of tls) {
    if (keepLegacy && untouched(f)) continue;
    const v = asText(values[f.key]);
    if (v !== '') cfg.options[f.key] = v;
  }
  if (keepLegacy) {
    cfg.encrypt = original!.encrypt;
    cfg.trust_server_certificate = original!.trust;
    return;
  }
  const level = tlsLevel(shown, (k) => values[k]);
  if (level !== null) [cfg.encrypt, cfg.trust_server_certificate] = legacyPair(level);
  else if (original) {
    cfg.encrypt = original.encrypt;
    cfg.trust_server_certificate = original.trust;
  }
}
