import { invoke } from '@tauri-apps/api/core';
import type { ChartAggregate, ChartKind } from './types';

// Charts over the whole result of their query (results grid and notebook
// cells). Mirrors src-tauri/src/commands/chart.rs. Each run opens a
// read-only session of its own, `chart:<chartId>`: it never touches the
// tab's or the cell's session (nor its transaction), and `cancel` stops it.
// A DataFrame SQL cell's chart (`notebookTab`) is aggregated by DuckDB on
// that notebook tab's session instead, over the tables its last run read;
// `cancelNotebook` stops it.

export const chartApi = {
  /** Aggregate `chart` over every row `query` returns, on the server where
   *  the engine can (`method: 'server'`), else in DBine (up to 1,000,000
   *  rows, `rows_capped`). `chartId`: any id unique among running charts. */
  aggregate: (a: { chartId: string; connectionId: string; database: string; query: string; chart: ChartKind; notebookTab?: string | null }) =>
    invoke<ChartAggregate>('chart_aggregate', {
      args: { chart_id: a.chartId, connection_id: a.connectionId, database: a.database, query: a.query, chart: a.chart, notebook_tab: a.notebookTab ?? null },
    }),
  /** Stop the run started with `chartId` (it then fails with kind 'cancelled'). */
  cancel: (chartId: string) => invoke<void>('cancel_query', { args: { session_id: `chart:${chartId}` } }),
  /** Stop a DataFrame SQL cell's chart (what runs on the tab's DuckDB session). */
  cancelNotebook: (tabId: string) => invoke<void>('cancel_query', { args: { session_id: `nbx:${tabId}` } }),
};
