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
// Across connections (&cross=1, mocked lineage_resolve_source / lineage_link_source /
// lineage_locate_source / get_cross_users): the SQL view in the middle, with "Incluir otras
// conexiones" turned on, shows the Power BI table that reads it, its model, and the model's report
// and dashboard. &cross=1&root=pbi puts the Power BI table in the middle: its SQL Server source
// resolves to the "Ventas" connection (&match=many: two candidates, &match=none: no connection,
// with a prefill) and "Ver su linaje" turns the grey card into the real object.

import type { CrossUsers, DependencyReport, Dependent, JobDependent, LineageEdge, LineageGraph, ObjectRef, SourceMatch } from '../api/types';
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
  medidas: report([dep('table', 'etl', 'stg_medidas', 'confirmed'), dep('view', 'dbo', 'v_base')]),
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

/** The Power BI side of the cross-connection flows (connection `pbi1`, model `Indicadores`). */
const PBI = { id: 'pbi1', name: 'Power BI Indicadores', database: 'Indicadores' };
const PBI_TABLE = { kind: 'table', schema: null, name: 'medidas pbi' };
const SQL_SOURCE = { system: 'SQL Server', server: 'srv-dw01', database: 'indicadores', schema: 'dbo', name: 'medidas', detail: 'Partición medidas pbi' };
const PBI_USES: Record<string, DependencyReport> = {
  'medidas pbi': report([], { external: [SQL_SOURCE] }),
};
const PBI_USED_BY: Record<string, DependencyReport> = {
  'medidas pbi': report([{ ...dep('model', '', 'Indicadores', 'confirmed'), schema: null, mentions: [] }]),
  Indicadores: report([], {
    external_users: [
      { system: 'Power BI report', name: 'Tablero de medidas', detail: 'https://app.powerbi.com/groups/me/reports/r1' },
      { system: 'Power BI dashboard', name: 'Panel gerencial', detail: 'https://app.powerbi.com/groups/me/dashboards/d1' },
    ],
  }),
};
const CROSS_USERS: CrossUsers = {
  users: [{ connection_id: PBI.id, connection_name: PBI.name, database: PBI.database, object: PBI_TABLE, detail: 'Partición medidas pbi' }],
  searched: [{ connection_id: PBI.id, name: PBI.name, models: 3, cached: false }],
  notes: [],
};
const sqlMatch = (id: string, name: string) => ({ connection_id: id, name, driver: 'sqlserver', via: 'auto' as const });

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
  const cross = !!params.get('cross');
  if (cross) {
    for (const [id, name, driver] of [['c1', 'Ventas', 'sqlserver'], ['c2', 'Ventas (réplica)', 'sqlserver'], [PBI.id, PBI.name, 'powerbi']]) {
      if (!conns.list.some((c) => c.id === id)) conns.list.push({ id, name, config: { driver } } as never);
      conns.live[id] = { status: 'connected', serverVersion: '', databases: ['ventas'], defaultDatabase: 'ventas', error: null };
    }
    for (const d of ['sqlserver', 'powerbi']) if (!conns.drivers.some((x) => x.id === d)) conns.drivers.push({ id: d, object_kinds: [], fields: [] } as never);
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
  if (cross && params.get('root') === 'pbi') {
    tab.connectionId = PBI.id;
    tab.database = PBI.database;
    tab.object = PBI_TABLE;
  }
  if (params.get('scope') !== 'db') tabs.tabs.push(tab);
  const linked: { signature: string; id: string | null }[] = [];
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    invoke: async (cmd: string, payload?: { args?: Args }) => {
      const a = payload?.args ?? {};
      const name = String(((a.target as Args | undefined)?.object as Args | undefined)?.name ?? '');
      const onPbi = a.connection_id === PBI.id;
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'get_lineage_graph': await new Promise((r) => setTimeout(r, 400)); return params.get('big') ? bigGraph() : smallGraph();
        case 'get_references': await new Promise((r) => setTimeout(r, 250)); return (onPbi ? PBI_USES : USES)[name] ?? report([]);
        case 'get_dependents': await new Promise((r) => setTimeout(r, 300)); return (onPbi ? PBI_USED_BY : USED_BY)[name] ?? report([]);
        case 'get_cross_users': await new Promise((r) => setTimeout(r, 900)); return CROSS_USERS;
        case 'lineage_resolve_source': {
          await new Promise((r) => setTimeout(r, 350));
          const signature = 'sqlserver|srv-dw01|1433||indicadores';
          const mode = params.get('match');
          const own = linked.find((l) => l.signature === signature);
          const m: SourceMatch = own?.id
            ? { status: 'one', signature, candidates: [{ ...sqlMatch(own.id, own.id === 'c2' ? 'Ventas (réplica)' : 'Ventas'), via: 'link' }], prefill: null }
            : mode === 'many' ? { status: 'many', signature, candidates: [sqlMatch('c1', 'Ventas'), sqlMatch('c2', 'Ventas (réplica)')], prefill: null }
            : mode === 'none' ? { status: 'none', signature, candidates: [], prefill: { driver: 'sqlserver', host: 'srv-dw01', port: 1433, database: 'indicadores', username: null, password: null, encrypt: true, trust_server_certificate: false, read_only: true, options: {} } }
            : { status: 'one', signature, candidates: [sqlMatch('c1', 'Ventas')], prefill: null };
          return m;
        }
        case 'lineage_open_source': await new Promise((r) => setTimeout(r, 300)); return { connection_id: 'c1', name: 'indicadores · srv-dw01', driver: 'sqlserver', needs_user: false };
        case 'lineage_link_source': {
          const i = linked.findIndex((l) => l.signature === a.signature);
          if (i >= 0) linked.splice(i, 1);
          if (a.connection_id) linked.push({ signature: String(a.signature), id: String(a.connection_id) });
          return null;
        }
        case 'lineage_locate_source': await new Promise((r) => setTimeout(r, 300)); return { database: 'indicadores', object: ref('table', 'dbo', 'medidas') };
        case 'get_job_references': await new Promise((r) => setTimeout(r, 250)); return report([dep('procedure', 'dbo', 'sp_reporte', 'confirmed', 'code', 1)]);
        case 'list_settings': case 'get_settings': return {};
        default: return null;
      }
    },
  };
  return tabs.tabs.find((x) => x.id === 'lin1') as DependenciesTab;
}
