import { invoke } from '@tauri-apps/api/core';

// Downloadable drivers (docs/drivers-bajo-demanda.md). A build that carries
// every driver inside answers `on_demand: false` and an empty list.

export interface DriverPackage {
  package: string;
  /** The engine it's named after ("SQL Server"). */
  label: string;
  /** The driver's own version ("1.2.0"), apart from the app's. */
  version: string;
  /** Its drivers' names. */
  drivers: string[];
  /** Download size, bytes. */
  size: number;
  /** Bytes on disk, when downloaded. */
  installed: number | null;
}

export interface DriverPackages {
  on_demand: boolean;
  packages: DriverPackage[];
}

export const driversApi = {
  packages: () => invoke<DriverPackages>('drivers_packages'),
  install: (pkg: string) => invoke<void>('drivers_install', { args: { package: pkg } }),
  remove: (pkg: string) => invoke<void>('drivers_remove', { args: { package: pkg } }),
};
