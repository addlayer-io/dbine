import { invoke } from '@tauri-apps/api/core';
import { locale } from '../i18n';
import { useSettingsStore } from '../stores/settings';

// Anonymous usage telemetry, on by default: TelemetryConsent tells what's
// sent once, and Configuración › General turns it off. The backend decides
// what can leave (src-tauri/src/commands/telemetry.rs); what's sent is listed
// in docs/telemetria.md.

/** `false` turns it off; `true` or unset (the default) shares. */
export const TELEMETRY_CONSENT = 'telemetry.consent';
/** The notice was shown (once). */
export const TELEMETRY_NOTICE = 'telemetry.noticeSeen';

export function telemetryAllowed(): boolean {
  return useSettingsStore().get<boolean | null>(TELEMETRY_CONSENT, null) !== false;
}

/** Engines already counted this run: one event per engine, not per connect. */
const counted = new Set<string>();
/** Modules already counted this run, same idea. */
const modules = new Set<string>();
let started = false;

function send(event: 'app_started' | 'connection_opened' | 'module_opened', props: { engine?: string; module?: string } = {}) {
  if (!telemetryAllowed()) return;
  invoke('track_event', { args: { event, engine: props.engine ?? null, module: props.module ?? null, locale: locale() } }).catch(() => {});
}

/** Once per run: how many people use DBine, on which OS and version. */
export function trackAppStarted() {
  if (started || !telemetryAllowed()) return;
  started = true;
  send('app_started');
}

/** Which engines are used: only the driver id, nothing about the connection. */
export function trackConnectionOpened(engine: string) {
  if (counted.has(engine) || !telemetryAllowed()) return;
  counted.add(engine);
  send('connection_opened', { engine });
}

/** Which parts of the workbench are used: the tab kind, once per run. */
export function trackModuleOpened(module: string) {
  if (modules.has(module) || !telemetryAllowed()) return;
  modules.add(module);
  send('module_opened', { module });
}
