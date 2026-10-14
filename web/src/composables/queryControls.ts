import { reactive } from 'vue';
import type { TxState } from '../api/types';

// What a query or file tab (QueryView) lets the status bar change: its
// connection, database, transaction mode and the row limit. QueryView
// registers it per tab; the status bar shows the active tab's.

/** "Todas": no practical limit (and safe for any engine's integer). */
export const ALL_ROWS = 2_147_483_647;

export interface QueryControls {
  /** A run is in progress: the connection and database stay. */
  running: boolean;
  txBusy: boolean;
  /** A file tab with no base yet. */
  unbound: boolean;
  /** The driver has databases to pick (its label isn't ''). */
  showDatabase: boolean;
  databases: string[];
  loadingDatabases: boolean;
  offersManualTx: boolean;
  manualTx: boolean;
  txOpen: boolean;
  txState: TxState | null;
  maxRows: number;
  /** Connect if needed so the database list is complete. */
  loadDatabases(): Promise<void>;
  changeConnection(connectionId: string): Promise<void>;
  changeDatabase(db: string): Promise<void>;
  setManualTx(on: boolean): Promise<void>;
  setMaxRows(n: number): void;
}

/** By tab id. */
export const queryControls = reactive(new Map<string, QueryControls>());
