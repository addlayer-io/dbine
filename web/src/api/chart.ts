import { invoke } from '@tauri-apps/api/core';
import type { ChartAggregate, ChartKind } from './types';

// Charts over the whole result of their query (results grid and notebook
// cells). Mirrors src-tauri/src/commands/chart.rs. Each run opens a
// read-only session of its own, `chart:<chartId>`: it never touches the
// tab's or the cell's session (nor its transaction), and `cancel` stops it.

export const chartApi = {
  /** Aggregate `chart` over every row `query` returns, on the server where
   *  the engine can (`method: 'server'`), else in DBine (up to 1,000,000
   *  rows, `rows_capped`). `chartId`: any id unique among running charts. */
  aggregate: (a: { chartId: string; connectionId: string; database: string; query: string; chart: ChartKind }) =>
    invoke<ChartAggregate>('chart_aggregate', {
      args: { chart_id: a.chartId, connection_id: a.connectionId, database: a.database, query: a.query, chart: a.chart },
    }),
  /** Stop the run started with `chartId` (it then fails with kind 'cancelled'). */
  cancel: (chartId: string) => invoke<void>('cancel_query', { args: { session_id: `chart:${chartId}` } }),
};
