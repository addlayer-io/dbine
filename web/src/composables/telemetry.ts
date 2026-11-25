import { invoke } from '@tauri-apps/api/core';
import { locale } from '../i18n';
import { useSettingsStore } from '../stores/settings';

// Anonymous usage telemetry, on by default: TelemetryConsent tells what's
// sent once, and Configuración › General turns it off. The backend decides
// what can leave (src-tauri/src/commands/telemetry.rs); what's sent is listed
// in docs/telemetry.md.
//
// Each day of use counts like a launch: an app left open for days (a laptop
// that sleeps) sends `app_started` again on the first click or key of each
// new day, and that day's engines, modules and features count again.

/** `false` turns it off; `true` or unset (the default) shares. */
export const TELEMETRY_CONSENT = 'telemetry.consent';
/** The notice was shown (once). */
export const TELEMETRY_NOTICE = 'telemetry.noticeSeen';

export function telemetryAllowed(): boolean {
  return useSettingsStore().get<boolean | null>(TELEMETRY_CONSENT, null) !== false;
}

/** Engines already counted today: one event per engine, not per connect. */
const counted = new Set<string>();
/** Modules already counted today, same idea. */
const modules = new Set<string>();
/** Features already counted today, same idea. */
const features = new Set<string>();
let started = false;

/** When the day the counters are for ends: the next local midnight, in ms. */
let dayEnds = 0;
function startDay() {
  const now = new Date();
  dayEnds = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1).getTime();
  counted.clear();
  modules.clear();
  features.clear();
  started = false;
}
startDay();

/** On the user's clicks and keys: true on the first one of a new day, when
 *  the counters start over and `app_started` goes again (the caller counts
 *  again what's on screen). One comparison per event the rest of the day. */
export function newDayOfUse(): boolean {
  if (Date.now() < dayEnds) return false;
  startDay();
  trackAppStarted();
  return true;
}

type Props = { engine?: string; module?: string; feature?: string; provider?: string; firstRun?: boolean };
function send(event: 'app_started' | 'connection_opened' | 'module_opened' | 'feature_used', props: Props = {}) {
  if (!telemetryAllowed()) return;
  invoke('track_event', {
    args: {
      event, engine: props.engine ?? null, module: props.module ?? null, feature: props.feature ?? null,
      provider: props.provider ?? null, firstRun: props.firstRun ?? null, locale: locale(),
    },
  }).catch(() => {});
}

/** Once per day of use: how many people use DBine, on which OS and version.
 *  `firstRun` marks the first launch after installing. Every window asks;
 *  the backend sends one a day for all of them. */
export function trackAppStarted(firstRun = false) {
  if (started || !telemetryAllowed()) return;
  started = true;
  send('app_started', { firstRun: firstRun || undefined });
}

/** Which engines are used: only the driver id, nothing about the connection. */
export function trackConnectionOpened(engine: string) {
  if (counted.has(engine) || !telemetryAllowed()) return;
  counted.add(engine);
  send('connection_opened', { engine });
}

/** Features used (an action, not a screen): once a day per feature and,
 *  for the assistant, per kind of provider. The backend drops anything not
 *  on its lists (src-tauri/src/commands/telemetry.rs). */
export type Feature = 'ai_message' | 'schema_sync' | 'data_sync' | 'migration_run' | 'multi_db_run' | 'sample_opened' | 'sample_tour';
export function trackFeature(feature: Feature, provider?: string) {
  const key = `${feature}|${provider ?? ''}`;
  if (features.has(key) || !telemetryAllowed()) return;
  features.add(key);
  send('feature_used', { feature, provider });
}

/** Which parts of the workbench are used: the tab kind, once a day. */
export function trackModuleOpened(module: string) {
  if (modules.has(module) || !telemetryAllowed()) return;
  modules.add(module);
  send('module_opened', { module });
}
