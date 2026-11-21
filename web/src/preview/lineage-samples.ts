// view=lineage[&list=1]: the "Dependencias" tab over a fake backend, in
// lineage mode (or the list with &list=1). The view in the middle reads two
// tables, calls a function and comes from a SQL Server table outside the
// database; a procedure and a view use it. Every card answers both sides
// with a few more, so levels can be opened. The procedure is run by a SQL
// Server Agent job (&root=proc puts the procedure in the middle, &root=job
// the job). &scope=db opens the whole database's lineage ("Linaje", ~40 objects, views,
// procedures, a job and an outside source); &big=1 makes it 300 objects in 8 schemas (opens
// as an overview by schema); &ask=1 uses an engine without a catalog query ("Analizar");
// &schema=rep scopes it to a schema.

import type { DependencyReport, Dependent, JobDependent, LineageEdge, LineageGraph, ObjectRef } from '../api/types';
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

const ref = (kind: string, schema: string, name: string): ObjectRef => ({ kind, schema, name });
const ledge = (from: ObjectRef, to: ObjectRef, confidence: LineageEdge['confidence'] = 'confirmed', relation: LineageEdge['relation'] = 'code'): LineageEdge => ({
  from, to, relation, confidence, detail: relation === 'foreign_key' ? `FK_${to.name} (id) → ${from.name} (id)` : null,
  mentions: relation === 'code' ? [{ line: 4, text: `FROM ${from.schema}.${from.name}`, dynamic: false }] : [],
});

/** ~40 objects: tables with foreign keys, views over them, procedures, a job, an outside source. */
function smallGraph(): LineageGraph {
  const objects: ObjectRef[] = [];
  const edges: LineageEdge[] = [];
  const tables = ['Clientes', 'Pedidos', 'Productos', 'Tarifas', 'Proveedores', 'Compras', 'Stock', 'Empleados', 'Sucursales', 'Pagos'].map((n) => ref('table', 'dbo', n));
  objects.push(...tables);
  const fk = (a: number, b: number) => edges.push(ledge(tables[b], tables[a], 'confirmed', 'foreign_key'));
  fk(1, 0); fk(1, 2); fk(5, 4); fk(5, 2); fk(6, 2); fk(6, 8); fk(9, 1); fk(7, 8);
  const views = ['v_ventas', 'v_stock', 'v_compras', 'v_kpis', 'v_clientes_top', 'v_margen', 'v_pagos_mes', 'v_resumen'].map((n) => ref('view', 'rep', n));
  objects.push(...views);
  const uses: [number, number[]][] = [[0, [1, 0, 2]], [1, [6, 2]], [2, [5, 4]], [3, [0]], [4, [0, 1]], [5, [0, 3]], [6, [9]], [7, [3, 4]]];
  for (const [v, ts] of uses) for (const t of ts) edges.push(ledge(tables[t], views[v], v % 3 ? 'probable' : 'confirmed'));
  edges.push(ledge(views[0], views[3]), ledge(views[0], views[7], 'probable'), ledge(views[6], views[7]));
  const procs = ['sp_reporte', 'sp_cierre', 'sp_cargar_medidas', 'sp_alta_cliente'].map((n) => ref('procedure', 'dbo', n));
  objects.push(...procs);
  edges.push(ledge(views[3], procs[0], 'probable'), ledge(tables[0], procs[3], 'probable'), ledge(tables[1], procs[1], 'probable'), ledge(views[0], procs[1], 'probable'));
  const stage = ['stg_ventas', 'stg_clientes', 'stg_stock'].map((n) => ref('table', 'etl', n));
  objects.push(...stage, ref('function', 'dbo', 'fn_total'), ref('table', 'etl', 'huerfana1'), ref('table', 'etl', 'huerfana2'), ref('view', 'rep', 'v_vieja'), ref('sequence', 'dbo', 'seq_pedidos'));
  edges.push(ledge(stage[0], procs[2], 'probable'), ledge(stage[1], procs[2], 'probable'), ledge(procs[2], tables[6], 'probable'), ledge(ref('function', 'dbo', 'fn_total'), views[0], 'probable'));
  return {
    objects, edges,
    external: [{ source: { system: 'SQL Server', server: 'srv-dw01', database: 'indicadores', schema: 'dbo', name: 'medidas_origen', detail: 'Servidor vinculado DW' }, to: stage[0] }],
    jobs: [{
      id: 'A1F3', name: 'Carga nocturna', kind: 'SQL Server Agent', group: null, external: [],
      uses: [{ ...dep('procedure', 'dbo', 'sp_cargar_medidas', 'probable'), mentions: [{ line: 1, text: 'EXEC dbo.sp_cargar_medidas;', dynamic: false }] }],
    }],
    scanned: 40, unreadable: [], truncated: false, notes: [],
  };
}

/** 300 objects in 8 schemas: each layer's tables feed the next layer's views. */
function bigGraph(): LineageGraph {
  const schemas = ['raw', 'stg', 'core', 'dwh', 'rep', 'mkt', 'fin', 'ops'];
  const objects: ObjectRef[] = [];
  const edges: LineageEdge[] = [];
  schemas.forEach((s, si) => {
    for (let i = 0; i < 37; i++) {
      const o = ref(i % 3 === 0 ? 'view' : 'table', s, `${i % 3 === 0 ? 'v' : 't'}_${s}_${i}`);
      objects.push(o);
      if (si > 0 && i % 4 !== 3) edges.push(ledge(objects[(si - 1) * 37 + ((i * 5) % 37)], o, i % 5 ? 'probable' : 'confirmed'));
      if (si > 1 && i % 6 === 0) edges.push(ledge(objects[(si - 2) * 37 + ((i * 7) % 37)], o, 'probable'));
    }
  });
  objects.push(...Array.from({ length: 4 }, (_, i) => ref('table', 'ops', `suelta_${i}`)));
  return { ...smallGraph(), objects, edges, external: [], jobs: [], scanned: 300, notes: [] };
}

export function installLineagePreview(params: URLSearchParams) {
  const conns = useConnectionsStore();
  conns.live.c1 = { status: 'connected', serverVersion: 'SQL Server 2022', databases: ['ventas'], defaultDatabase: 'ventas', error: null };
  const tabs = useTabsStore();
  if (params.get('scope') === 'db') {
    if (!params.get('ask')) {
      conns.list.push({ id: 'c1', name: 'Ventas', config: { driver: 'sqlserver' } } as never);
      conns.drivers.push({ id: 'sqlserver', object_kinds: [] } as never);
    }
    const schema = params.get('schema');
    tabs.tabs.push({
      id: 'lin1', kind: 'dependencies', connectionId: 'c1', database: 'ventas', preview: false, object: { kind: 'database', schema: null, name: 'ventas' },
      column: null, view: 'lineage', scope: schema ? { schemas: [schema] } : {},
    });
  }
  const tab: DependenciesTab = {
    id: 'lin1', kind: 'dependencies', connectionId: 'c1', database: 'ventas', preview: false,
    object: { kind: 'view', schema: 'dbo', name: 'v_medidas_resumen' }, column: null, view: params.get('list') ? 'list' : 'lineage',
  };
  if (params.get('root') === 'proc') tab.object = { kind: 'procedure', schema: 'dbo', name: 'sp_reporte' };
  if (params.get('root') === 'job') {
    tab.object = { kind: 'job', schema: null, name: NIGHTLY.name };
    tab.jobId = NIGHTLY.id;
  }
  if (params.get('scope') !== 'db') tabs.tabs.push(tab);
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
        case 'get_lineage_graph': await new Promise((r) => setTimeout(r, 400)); return params.get('big') ? bigGraph() : smallGraph();
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
