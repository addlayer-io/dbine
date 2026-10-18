// view=nbperm[&set=one|many|glob|unsaved|sidecars|settings|notebook] (preview/main.ts):
// "Este notebook quiere leer estos archivos" with one file, several, a
// glob, an unsaved notebook, a shapefile with its sidecars; the "Archivos que los notebooks pueden leer"
// list of Configuración › Notebooks and of one notebook, over a fake backend.
import { h } from 'vue';
import type { DataFile, FileApprovalRequest, FileGrant } from '../api/types';
import NotebookFileApproval from '../components/notebook/NotebookFileApproval.vue';
import NotebookFileGrants from '../components/notebook/NotebookFileGrants.vue';
import NotebookSettings from '../components/notebook/NotebookSettings.vue';

const at = (s: string) => new Date(s).getTime();
const file = (path: string, size: number, when: string, pattern?: string): DataFile => ({
  path, display: path, size, mtime_ms: at(when), ...(pattern ? { pattern } : {}),
});

const GLOB = '/Users/ana/exports/ventas_*.csv';
const REQUESTS: Record<string, FileApprovalRequest> = {
  one: { token: 't1', saved: true, files: [file('/Users/ana/Downloads/CustomerTest.csv', 48_213, '2026-10-17T09:12:00')] },
  many: {
    token: 't2', saved: true, files: [
      file('/Users/ana/Downloads/CustomerTest.csv', 48_213, '2026-10-17T09:12:00'),
      file('/Users/ana/Proyectos/ventas/datos/pedidos_2026.parquet', 12_582_912, '2026-10-16T18:40:00'),
      file('/Users/ana/Proyectos/ventas/datos/eventos.ndjson', 873_004, '2026-10-18T08:03:00'),
      file('/Volumes/Compartido/Finanzas/Presupuesto 2027 (borrador final).xlsx', 2_310_144, '2026-10-12T11:25:00'),
    ],
  },
  glob: {
    token: 't3', saved: true, files: [
      file('/Users/ana/Downloads/CustomerTest.csv', 48_213, '2026-10-17T09:12:00'),
      file('/Users/ana/exports/ventas_2026-08.csv', 1_204_332, '2026-09-01T07:00:00', GLOB),
      file('/Users/ana/exports/ventas_2026-09.csv', 1_350_870, '2026-10-01T07:00:00', GLOB),
      file('/Users/ana/exports/ventas_2026-10.csv', 402_118, '2026-10-18T07:00:00', GLOB),
    ],
  },
  sidecars: {
    token: 't5', saved: true, files: [
      {
        ...file('/Users/ana/geo/zonas.shp', 1_482_204, '2026-10-14T10:05:00'),
        sidecars: [
          file('/Users/ana/geo/zonas.cpg', 5, '2026-10-14T10:05:00'),
          file('/Users/ana/geo/zonas.dbf', 88_410, '2026-10-14T10:05:00'),
          file('/Users/ana/geo/zonas.prj', 145, '2026-10-14T10:05:00'),
          file('/Users/ana/geo/zonas.shx', 1_220, '2026-10-14T10:05:00'),
        ],
      },
      file('/Users/ana/Proyectos/crm/crm.db', 3_145_728, '2026-10-18T09:40:00'),
    ],
  },
  unsaved: { token: 't4', saved: false, files: [file('/Users/ana/Downloads/CustomerTest.csv', 48_213, '2026-10-17T09:12:00'), file('/Users/ana/Downloads/precios.parquet', 92_311, '2026-10-15T16:20:00')] },
};

const GRANTS: FileGrant[] = [
  { notebook: '/Users/ana/Proyectos/ventas/notebooks/Cierre mensual.ipynb', target: GLOB, pattern: true, granted_at: '2026-10-02T10:14:00Z' },
  { notebook: '/Users/ana/Proyectos/ventas/notebooks/Cierre mensual.ipynb', target: '/Users/ana/Downloads/CustomerTest.csv', pattern: false, granted_at: '2026-10-17T12:30:00Z' },
  { notebook: '/Users/ana/Documents/Análisis de clientes.ipynb', target: '/Users/ana/Proyectos/ventas/datos/pedidos_2026.parquet', pattern: false, granted_at: '2026-10-11T09:02:00Z' },
  { notebook: '/Users/ana/Documents/Análisis de clientes.ipynb', target: '/Volumes/Compartido/Finanzas/**/*.xlsx', pattern: true, granted_at: '2026-09-28T15:47:00Z' },
];

export function installNbPermMock() {
  let grants = [...GRANTS];
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a: { args: { notebook?: string | null; target?: string } }) => {
      switch (cmd) {
        case 'notebook_file_grants': return grants.filter((g) => !a.args.notebook || g.notebook === a.args.notebook);
        case 'notebook_file_grant_remove': grants = grants.filter((g) => !(g.notebook === a.args.notebook && g.target === a.args.target)); return true;
        case 'notebook_file_grants_clear': grants = grants.filter((g) => a.args.notebook && g.notebook !== a.args.notebook); return 0;
        default: return null;
      }
    },
  };
}

export function nbPermPreview(params: URLSearchParams) {
  const set = params.get('set') ?? 'one';
  if (set === 'settings') {
    return h('div', { style: 'width: 760px; padding: 16px 24px; background: var(--el-bg-color)' }, [h(NotebookSettings)]);
  }
  if (set === 'notebook') {
    return h('div', { style: 'width: 600px; padding: 8px 20px; background: var(--el-bg-color)' }, [h(NotebookFileGrants, { notebook: GRANTS[0].notebook })]);
  }
  return h('div', { style: 'width: 100vw; height: 100vh; background: var(--ide-editor)' }, [
    h(NotebookFileApproval, { request: REQUESTS[set] ?? REQUESTS.one, onDecide: () => {} }),
  ]);
}
