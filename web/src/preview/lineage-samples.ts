// view=lineage[&list=1]: the "Dependencias" tab over a fake backend, in
// lineage mode (or the list with &list=1). The view in the middle reads two
// tables, calls a function and comes from a SQL Server table outside the
// database; a procedure and a view use it. Every card answers both sides
// with a few more, so levels can be opened. The procedure is run by a SQL
// Server Agent job (&root=proc puts the procedure in the middle, &root=job
// the job).

import type { DependencyReport, Dependent, JobDependent } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DependenciesTab } from '../stores/tabs';

type Args = Record<string, unknown>;
const dep = (kind: string, schema: string, name: string, confidence: Dependent['confidence'] = 'probable', relation: Dependent['relation'] = 'code', line = 3): Dependent => ({
  kind, schema, name, parent: null, relation, confidence, detail: relation === 'foreign_key' ? `FK_${name} (id) → ${schema}.${name} (id)` : null,
  mentions: relation === 'code' ? [{ line, text: `FROM ${schema}.${name} m`, dynamic: false }] : [],
});
const report = (items: Dependent[], extra: Partial<DependencyReport> = {}): DependencyReport => ({ items, scanned: 1, unreadable: [], note: null, ...extra });

/** What each object uses. */
const USES: Record<string, DependencyReport> = {
  v_medidas_resumen: report([dep('table', 'indicadores', 'medidas', 'confirmed'), dep('table', 'dbo', 'Pedidos'), dep('function', 'dbo', 'fn_total')], {
    external: [{ system: 'SQL Server', server: 'srv-dw01', database: 'indicadores_directorio', schema: 'dbo', name: 'medidas_origen', detail: 'Servidor vinculado DW' }],
  }),
  Pedidos: report([dep('table', 'dbo', 'Clientes', 'confirmed', 'foreign_key'), dep('table', 'dbo', 'Productos', 'confirmed', 'foreign_key')]),
  sp_reporte: report([dep('view', 'dbo', 'v_medidas_resumen'), dep('table', 'dbo', 'Clientes')]),
  fn_total: report([dep('table', 'dbo', 'Tarifas')]),
};
/** The job that runs the procedure. */
const NIGHTLY: JobDependent = {
  id: 'A1F3', name: 'Carga nocturna', kind: 'SQL Server Agent', group: null, steps: ['Cargar ventas'], confidence: 'probable',
  mentions: [{ line: 1, text: 'EXEC dbo.sp_reporte @dia = 1;', dynamic: false }],
};
/** What uses each object. */
const USED_BY: Record<string, DependencyReport> = {
  sp_reporte: report([], { jobs: [NIGHTLY] }),
  v_medidas_resumen: report([dep('procedure', 'dbo', 'sp_reporte', 'probable', 'code', 7), dep('view', 'dbo', 'v_kpis', 'confirmed')]),
  v_kpis: report([dep('procedure', 'dbo', 'sp_tablero')]),
  medidas: report([dep('view', 'dbo', 'v_medidas_resumen', 'confirmed'), dep('procedure', 'etl', 'sp_cargar_medidas')]),
};

export function installLineagePreview(params: URLSearchParams) {
  const conns = useConnectionsStore();
  conns.live.c1 = { status: 'connected', serverVersion: 'SQL Server 2022', databases: ['ventas'], defaultDatabase: 'ventas', error: null };
  const tabs = useTabsStore();
  const tab: DependenciesTab = {
    id: 'lin1', kind: 'dependencies', connectionId: 'c1', database: 'ventas', preview: false,
    object: { kind: 'view', schema: 'dbo', name: 'v_medidas_resumen' }, column: null, view: params.get('list') ? 'list' : 'lineage',
  };
  if (params.get('root') === 'proc') tab.object = { kind: 'procedure', schema: 'dbo', name: 'sp_reporte' };
  if (params.get('root') === 'job') {
    tab.object = { kind: 'job', schema: null, name: NIGHTLY.name };
    tab.jobId = NIGHTLY.id;
  }
  tabs.tabs.push(tab);
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    invoke: async (cmd: string, payload?: { args?: Args }) => {
      const a = payload?.args ?? {};
      const name = String(((a.target as Args | undefined)?.object as Args | undefined)?.name ?? '');
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'get_references': await new Promise((r) => setTimeout(r, 250)); return USES[name] ?? report([]);
        case 'get_dependents': await new Promise((r) => setTimeout(r, 300)); return USED_BY[name] ?? report([]);
        case 'get_job_references': await new Promise((r) => setTimeout(r, 250)); return report([dep('procedure', 'dbo', 'sp_reporte', 'confirmed', 'code', 1)]);
        case 'list_settings': case 'get_settings': return {};
        default: return null;
      }
    },
  };
  return tabs.tabs.find((x) => x.id === 'lin1') as DependenciesTab;
}
