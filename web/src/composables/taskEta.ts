// Remaining-time estimate for a long task, from the progress it reports.
// Pure (no Vue, no Pinia, no i18n) so it can be tested with plain node.
//
// The store feeds one sample per `progress()` call: (time, done) for the
// current counter — a (unit, total) pair. A new unit or total, or `done`
// going backwards (the counter restarted for a new step), starts a new
// series: the old rate says nothing about the new work. `phase` is not part
// of the key: adopters put the item being processed there ("tabla x",
// "12 sentencias"), which changes on every update.
//
// Rate: progress over the `WINDOW_MS` before the last update (robust to
// bursts, since it only looks at the window's ends), with an EWMA of 1 s+
// steps as fallback when the window has too little. A stall makes the ETA
// grow: once nothing has moved for longer than the usual gap between
// updates, the same window's progress is spread up to *now* instead of up to
// the last update.

export interface EtaSample {
  t: number;
  done: number;
}

export interface EtaSeries {
  /** `unit|total` of the counter these samples belong to. */
  key: string;
  /** When this series started (its first sample, or the task start). */
  start: number;
  samples: EtaSample[];
  /** How many times the series restarted during the task (0 = one counter for the whole task). */
  resets: number;
  /** Smoothed rate in units/ms, from steps of at least `EWMA_STEP_MS`. */
  ewma?: number;
  /** Where the current EWMA step began. */
  anchor: EtaSample;
  /** Last time `done` went up. */
  lastMoveAt: number;
}

export interface Eta {
  remainingMs: number;
  confidence: 'low' | 'ok';
}

export const WINDOW_MS = 60_000;
export const MAX_SAMPLES = 120;
/** Samples span two windows, so there is always one at or before the window's start. */
const MIN_SPACING_MS = (2 * WINDOW_MS) / MAX_SAMPLES;
const EWMA_STEP_MS = 1_000;
/** EWMA time constant: a step's weight is 1 - e^(-dt/τ). */
const EWMA_TAU_MS = 30_000;
/** Minimum time in the series before any estimate. */
export const MIN_ELAPSED_MS = 5_000;
/** Never call it a stall before this much silence. */
const MIN_STALL_MS = 5_000;

export function etaKey(unit: string | undefined, total: number | undefined): string {
  return `${unit ?? ''}|${total ?? ''}`;
}

function newSeries(key: string, first: EtaSample, resets: number): EtaSeries {
  return { key, start: first.t, samples: [first], resets, anchor: first, lastMoveAt: first.t };
}

/**
 * Add a sample; returns the series to keep (a new one when the counter
 * changed). `origin` seeds a brand-new series (e.g. `{ t: startedAt, done: 0 }`
 * for the task's first counter) so the time before the first report counts.
 */
export function recordSample(
  series: EtaSeries | undefined,
  key: string,
  done: number,
  now: number,
  origin?: EtaSample,
): EtaSeries {
  const s: EtaSample = { t: now, done };
  if (!series) {
    if (origin && origin.t <= now && origin.done <= done) {
      const fresh = newSeries(key, origin, 0);
      return recordSample(fresh, key, done, now);
    }
    return newSeries(key, s, 0);
  }
  const last = series.samples[series.samples.length - 1];
  if (series.key !== key || done < last.done) return newSeries(key, s, series.resets + 1);
  if (done === last.done) {
    // No movement: nothing to learn (the stall is measured from lastMoveAt).
    return series;
  }
  // Keep samples at least `MIN_SPACING_MS` apart, so `MAX_SAMPLES` spans two
  // windows even with many updates a second: while the newest one is still
  // too close to the one before it, a new sample replaces it (never the
  // series' first).
  const n = series.samples.length;
  if (now <= last.t) last.done = done;
  else if (n >= 2 && last.t - series.samples[n - 2].t < MIN_SPACING_MS) series.samples[n - 1] = s;
  else series.samples.push(s);
  series.lastMoveAt = now;
  if (series.samples.length > MAX_SAMPLES) series.samples.splice(1, series.samples.length - MAX_SAMPLES);
  const dt = now - series.anchor.t;
  if (dt >= EWMA_STEP_MS) {
    const inst = (done - series.anchor.done) / dt;
    const w = 1 - Math.exp(-dt / EWMA_TAU_MS);
    series.ewma = series.ewma == null ? inst : w * inst + (1 - w) * series.ewma;
    series.anchor = s;
  }
  return series;
}

/** Remaining time for `total`, or null while there isn't enough to go on. */
export function estimate(series: EtaSeries | undefined, total: number | undefined, now: number): Eta | null {
  if (!series || !total || total <= 0) return null;
  const samples = series.samples;
  const last = samples[samples.length - 1];
  const first = samples[0];
  const done = last.done;
  if (done <= 0 || done >= total) return null;
  const elapsed = now - series.start;
  if (elapsed < MIN_ELAPSED_MS) return null;
  if (done / total < 0.02 && samples.length < 3) return null;
  const moved = done - first.done;
  if (moved <= 0) return null;

  // Window rate: from the newest sample at or before the window's start
  // (or the first one) to the last sample. The window ends at the last
  // sample, not at `now`, so the rate only changes with new progress and the
  // stall bound below makes the ETA grow steadily.
  const from = last.t - WINDOW_MS;
  let base = first;
  for (const x of samples) {
    if (x.t <= from) base = x; else break;
  }
  let rate: number | undefined;
  if (last.t - base.t >= EWMA_STEP_MS && last.done > base.done) rate = (last.done - base.done) / (last.t - base.t);
  if (rate == null) rate = series.ewma;
  if (rate == null || rate <= 0) {
    const span = last.t - first.t;
    if (span <= 0) return null;
    rate = moved / span;
  }

  // Stall: silence well past the usual gap between updates bounds the rate
  // by the progress over the whole span up to now, so the ETA keeps growing.
  // The usual gap comes from the recent samples: the first one may be the
  // task's start, hours back, once the buffer has been trimmed.
  const quiet = now - series.lastMoveAt;
  const n = samples.length;
  const gap = n > 2 ? (last.t - samples[1].t) / (n - 2) : n > 1 ? last.t - first.t : 0;
  const stalled = quiet > Math.max(MIN_STALL_MS, 3 * gap);
  if (stalled) {
    const ref = last.done > base.done ? base : first;
    rate = Math.min(rate, (last.done - ref.done) / Math.max(1, now - ref.t));
  }

  const remainingMs = (total - done) / rate;
  if (!Number.isFinite(remainingMs)) return null;

  // 'ok' once there's a real history and the short- and long-term rates agree.
  let confidence: Eta['confidence'] = 'low';
  if (!stalled && elapsed >= 15_000 && samples.length >= 5) {
    const ref = series.ewma ?? rate;
    const ratio = rate / ref;
    if (ratio > 0.5 && ratio < 2) confidence = 'ok';
  }
  return { remainingMs: Math.max(0, remainingMs), confidence };
}

export type EtaRounded =
  | { kind: 'lessThanMinute' }
  | { kind: 'minutes'; m: number }
  | { kind: 'hours'; h: number; m: number }
  | { kind: 'days'; d: number; h: number };

/** Round for display: whole minutes under an hour, 5 min steps under a day, hours past it. */
export function roundEta(ms: number): EtaRounded {
  const min = ms / 60_000;
  if (min < 1) return { kind: 'lessThanMinute' };
  if (min < 59.5) return { kind: 'minutes', m: Math.max(1, Math.round(min)) };
  const step = Math.round(min / 5) * 5;
  if (step < 24 * 60) return { kind: 'hours', h: Math.floor(step / 60), m: step % 60 };
  const h = Math.round(min / 60);
  return { kind: 'days', d: Math.floor(h / 24), h: h % 24 };
}

export type ElapsedParts =
  | { kind: 'seconds'; s: number }
  | { kind: 'minutes'; m: number; s: number }
  | { kind: 'hours'; h: number; m: number }
  | { kind: 'days'; d: number; h: number };

/** "45 s", "12 min 03 s", "3 h 07 min", "1 d 2 h" — as parts, for i18n. */
export function elapsedParts(ms: number): ElapsedParts {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return { kind: 'seconds', s };
  const m = Math.floor(s / 60);
  if (m < 60) return { kind: 'minutes', m, s: s % 60 };
  const h = Math.floor(m / 60);
  if (h < 24) return { kind: 'hours', h, m: m % 60 };
  return { kind: 'days', d: Math.floor(h / 24), h: h % 24 };
}
