import { invoke } from '@tauri-apps/api/core';

// Downloadable drivers (docs/on-demand-drivers.md). A build that carries
// every driver inside answers `on_demand: false` and an empty list.

/** Drivers have versions of their own, published apart from the app. */
export type DriverStatus =
  | { kind: 'up_to_date' }
  /** A newer version is downloading in the background. */
  | { kind: 'downloading' }
  /** A newer version is ready: the next connection uses it. */
  | { kind: 'ready_next_connection' }
  /** A newer version needs a newer app. */
  | { kind: 'needs_app'; min_app: string }
  /** `from` failed (`reason`; "user" when the user went back) and the previous one runs. */
  | { kind: 'rolled_back'; from: string; reason: string }
  /** The version in use has options this session didn't load. */
  | { kind: 'restart_for_new_options' };

export interface DriverPackage {
  package: string;
  /** The engine it's named after ("SQL Server"). */
  label: string;
  /** The driver's own version in use ("1.2.0"), apart from the app's; the one a download would get when it isn't downloaded. */
  version: string;
  /** Its drivers' names. */
  drivers: string[];
  /** Download size, bytes. */
  size: number;
  /** Bytes on disk, when downloaded. */
  installed: number | null;
  /** The newest version this app can run. */
  available: string;
  /** What "Volver a la anterior" goes back to. */
  previous: string | null;
  status: DriverStatus;
  /** A newer version needs this app version. */
  min_app_needed: string | null;
}

export interface DriverPackages {
  on_demand: boolean;
  packages: DriverPackage[];
}

export const driversApi = {
  packages: () => invoke<DriverPackages>('drivers_packages'),
  install: (pkg: string) => invoke<void>('drivers_install', { args: { package: pkg } }),
  remove: (pkg: string) => invoke<void>('drivers_remove', { args: { package: pkg } }),
  /** Fetch the drivers index now; updates download in the background (`drivers-changed`). */
  checkUpdates: () => invoke<void>('drivers_check_updates'),
  /** Drop the version in use for the previous one. */
  rollback: (pkg: string) => invoke<void>('drivers_rollback', { args: { package: pkg } }),
};
