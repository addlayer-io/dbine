// A query parameter's value (queryParams.ts): its type, whether it can be
// used, and whether a run may use the tab's remembered values without
// asking. No runtime imports, so `node --test` loads it directly.

export type ParamType = 'text' | 'number' | 'date' | 'null';

export interface ParamValue {
  type: ParamType;
  value: string;
}

const NUMBER = /^[-+]?(\d+(\.\d*)?|\.\d+)([eE][-+]?\d+)?$/;
/** `YYYY-MM-DD` with an optional time: the date and the time. */
export const DATE = /^(\d{4}-\d{2}-\d{2})(?:[ T](\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?))?$/;

/** Why a value can't be used, or null. */
export function paramProblem(v: ParamValue): 'number' | 'date' | null {
  if (v.type === 'number' && !NUMBER.test(v.value.trim())) return 'number';
  if (v.type === 'date' && !DATE.test(v.value.trim())) return 'date';
  return null;
}

/** What a run uses without asking (issue #111): with "No volver a
 *  preguntar" on, the tab's remembered value of every parameter in `keys`;
 *  null when it's off, or one has no value or one that can't be used (that
 *  run asks). */
export function silentValues(keys: string[], last: Record<string, ParamValue>, silent: boolean): Record<string, ParamValue> | null {
  if (!silent) return null;
  const out: Record<string, ParamValue> = {};
  for (const k of keys) {
    const v = last[k];
    if (!v || paramProblem(v)) return null;
    out[k] = v;
  }
  return out;
}
