// A notebook's charts as images for its HTML / Markdown export: each SQL cell
// shown as a chart gets all its charts rendered (light theme, PNG, twice the
// pixels) from the run made here, else from what the file kept. The export
// itself is notebookApi.export (web/src/api/notebook.ts), which embeds them
// (HTML) or writes them next to the .md file.

import type { Cell, Notebook, NotebookCell, QueryOutcome, ResultColumn } from '../api/types';
import type { ExportImage } from '../api/notebook';
import { t } from '../i18n';
import { readCellCharts } from './chartSettings';
import { chartPng } from './chartRender';

function firstSet(cell: NotebookCell, live: QueryOutcome | null): { columns: ResultColumn[]; rows: Cell[][]; cut: boolean } | null {
  const l = live?.results.find((r) => r.columns.length);
  if (l) return { columns: l.columns, rows: l.rows, cut: false };
  const s = cell.result?.results.find((r) => r.columns.length);
  return s ? { columns: s.columns.map((c) => ({ name: c.name, type_name: c.type_name })), rows: s.rows, cut: s.truncated } : null;
}

/** Each kept cell's charts (when the cell shows its chart), as PNGs. */
export function chartImages(nb: Notebook, liveOf: (cellId: string) => QueryOutcome | null): ExportImage[] {
  if (!nb.save_results) return [];
  const out: ExportImage[] = [];
  for (const cell of nb.cells) {
    if (cell.kind !== 'sql' || !cell.keep_result) continue;
    const stored = readCellCharts(cell.chart);
    if (stored.view !== 'chart' || !stored.charts.length) continue;
    const live = liveOf(cell.id);
    const data = firstSet(cell, live);
    stored.charts.forEach((ch, i) => {
      if (!ch.settings) return;
      // Whole-result points, or the kept aggregate of a cut preview, come first.
      const preferPoints = !!ch.points && (!!ch.settings.whole || (!live && !!data?.cut));
      const png = chartPng(data?.columns ?? [], data?.rows ?? null, ch.settings, ch.points, preferPoints);
      if (png) out.push({ cellId: cell.id, title: ch.title || t('notebook:chart.untitled', { n: i + 1 }), png });
    });
  }
  return out;
}
