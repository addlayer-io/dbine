import { toRaw } from 'vue';

/** The order each result's rows arrived in, so a local sort can be undone.
 *  Keyed by the plain array: a reactive result hands out a proxy of the one
 *  it was given. */
export function arrivalOrder<T>() {
  const first = new WeakMap<object, T[]>();
  return {
    /** The rows in the order they arrived; those that came later (saved inserts) last. */
    of(rows: T[]): T[] {
      const kept = first.get(toRaw(rows));
      if (!kept) return rows;
      const live = new Set(rows);
      const known = new Set(kept);
      return kept.filter((row) => live.has(row)).concat(rows.filter((row) => !known.has(row)));
    },
    /** `before` is about to be replaced by `after`, a new order of it. */
    moved(before: T[], after: T[]) {
      const kept = first.get(toRaw(before)) ?? before.slice();
      first.set(toRaw(before), kept);
      first.set(toRaw(after), kept);
    },
  };
}
