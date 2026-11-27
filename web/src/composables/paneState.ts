import type { SortKey } from '../api/types';
import type { ChartState, MapSettings } from './chartSettings';

/** What a result grid shows besides its rows. */
export interface GridState {
  top: number;
  left: number;
  widths: number[];
  hidden: string[];
  selection: { r: number; c: number } | null;
  anchor: { r: number; c: number } | null;
  rowSel: { from: number; to: number } | null;
  cellExtra: { r0: number; r1: number; c0: number; c1: number }[];
  rowExtra: { from: number; to: number }[];
}

/** How a result pane looks, kept while its tab is paused (pauseRuns.ts). */
export interface PaneState {
  active: number | 'messages' | 'plan';
  view: 'table' | 'chart' | 'tree' | 'map';
  /** The local sort of each result set, by its index. */
  sorts: Record<number, SortKey[]>;
  grid: GridState | null;
  chart: ChartState | null;
  map: MapSettings | null;
}
