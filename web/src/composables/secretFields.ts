import type { Field } from '../api/types';

/**
 * Whether a driver field holds a credential that must not show in clear:
 * password fields, and secret one-line text fields (a textarea, like a
 * service-account JSON, can't be masked).
 */
export function isMaskedField(f: Field): boolean {
  return f.kind.type === 'password' || (f.secret && f.kind.type === 'text');
}
