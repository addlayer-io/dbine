// "hace 5 min", "hace 2 h", "hace unos segundos": how old a saved answer is, as a unit and a
// count the caller turns into text in the interface language (the `dependencies:age.*` keys).
export type AgeUnit = 'seconds' | 'minutes' | 'hours' | 'days';

export function ageParts(iso: string, now = Date.now()): { unit: AgeUnit; n: number } {
  const t = new Date(iso).getTime();
  const s = Number.isNaN(t) ? 0 : Math.max(0, (now - t) / 1000);
  if (s < 60) return { unit: 'seconds', n: 0 };
  if (s < 3600) return { unit: 'minutes', n: Math.floor(s / 60) };
  if (s < 86400) return { unit: 'hours', n: Math.floor(s / 3600) };
  return { unit: 'days', n: Math.floor(s / 86400) };
}
