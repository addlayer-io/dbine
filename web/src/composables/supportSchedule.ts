// When "Apoyá el proyecto" (SupportReminder) shows: never on the first days,
// only once DBine has been used on FIRST_AFTER_DAYS different days, then once
// every EVERY_DAYS days whatever the answer. The first launch is for getting
// started (importing connections), not for asking. Plain functions, no Vue.

/** Days of use before the first reminder. */
export const FIRST_AFTER_DAYS = 7;
/** Days between reminders after the first one. */
export const EVERY_DAYS = 30;

/** A day of use, in local time (YYYY-MM-DD). */
export function localDay(d = new Date()): string {
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** Today counted once: the days of use and the last day after it. */
export function countDay(daysUsed: number, lastDay: string | null, today: string): { daysUsed: number; lastDay: string } {
  return lastDay === today ? { daysUsed, lastDay } : { daysUsed: daysUsed + 1, lastDay: today };
}

/** The reminder is due: when `next` (set once it was shown) has passed, or,
 *  before it was ever shown, after FIRST_AFTER_DAYS days of use. */
export function supportDue(daysUsed: number, next: string | null, now: number): boolean {
  const n = Date.parse(next ?? '');
  if (Number.isFinite(n)) return now >= n;
  return daysUsed >= FIRST_AFTER_DAYS;
}
