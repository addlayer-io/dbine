import type { CodeObject } from '../api/compare';
import type { RenameImpact, RenameSpec } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers } from './monitor-samples';

// view=rename[&prod=0][&name=<new name>][&fail=1]: "Renombrar…" of a SQL Server
// table dbo.Clientes, over a fake backend: the impact (rewritten, engine,
// manual), the script that follows the checkboxes, and a run that succeeds
// (or fails with &fail=1 and is rolled back). &prod=0: a non-production
// connection (no typed confirmation).

const params = new URLSearchParams(location.search);

const spec: RenameSpec = {
  kinds: ['table', 'view', 'procedure', 'function', 'trigger'], columns: true, indexes: true, constraints: true, schemas: false,
  tracked: [], replace: 'create_or_alter', references: 'sql', fold: 'none', transactional: true,
  note: 'sp_rename no cambia el texto de los módulos: DBine los vuelve a crear con CREATE OR ALTER.',
  replace_kinds: {}, holds_rows: [], epilogue: null,
};

const view = (name: string, definition: string): CodeObject => ({ kind: 'view', schema: 'dbo', name, definition });

function impact(to: string): RenameImpact {
  const q = /^[A-Za-z_][A-Za-z0-9_]*$/.test(to) ? to : `[${to.replace(/]/g, ']]')}]`;
  return {
    scanned: 7,
    unreadable: ['dbo.pCifrado'],
    note: null,
    spec_note: spec.note,
    collides: to.toLowerCase() === 'pedidos',
    quoted_name: q,
    atomic: true,
    definition: null,
    table: null,
    items: [
      {
        dependent: { kind: 'table', schema: 'dbo', name: 'Pedidos', parent: null, relation: 'foreign_key', confidence: 'confirmed', detail: 'FK_Pedidos_Clientes (cliente_id) → dbo.Clientes (id)', mentions: [] },
        action: { kind: 'engine' }, original: null,
      },
      {
        dependent: { kind: 'view', schema: 'dbo', name: 'vClientesActivos', parent: null, relation: 'code', confidence: 'confirmed', detail: null, mentions: [] },
        action: {
          kind: 'rewrite', schemabound: false, default_selected: true, unresolved: [],
          object: view('vClientesActivos', `CREATE VIEW dbo.vClientesActivos AS\nSELECT c.id, c.nombre\nFROM dbo.${q} c\nWHERE c.activo = 1`),
          edits: [{ line: 3, before: 'FROM dbo.Clientes c', after: `FROM dbo.${q} c` }],
        },
        original: 'CREATE VIEW dbo.vClientesActivos AS\nSELECT c.id, c.nombre\nFROM dbo.Clientes c\nWHERE c.activo = 1',
      },
      {
        dependent: { kind: 'view', schema: 'dbo', name: 'vSaldos', parent: null, relation: 'code', confidence: 'confirmed', detail: null, mentions: [] },
        action: {
          kind: 'rewrite', schemabound: true, default_selected: true, unresolved: [],
          object: view('vSaldos', `CREATE VIEW dbo.vSaldos WITH SCHEMABINDING AS\nSELECT c.id, c.saldo FROM dbo.${q} c`),
          edits: [{ line: 2, before: 'SELECT c.id, c.saldo FROM dbo.Clientes c', after: `SELECT c.id, c.saldo FROM dbo.${q} c` }],
        },
        original: 'CREATE VIEW dbo.vSaldos WITH SCHEMABINDING AS\nSELECT c.id, c.saldo FROM dbo.Clientes c',
      },
      {
        dependent: { kind: 'procedure', schema: 'dbo', name: 'pAltaCliente', parent: null, relation: 'code', confidence: 'confirmed', detail: null, mentions: [] },
        action: {
          kind: 'rewrite', schemabound: false, default_selected: false,
          object: { kind: 'procedure', schema: 'dbo', name: 'pAltaCliente', definition: `CREATE PROCEDURE dbo.pAltaCliente @n nvarchar(80) AS\nINSERT INTO dbo.${q} (nombre) VALUES (@n);\nEXEC dbo.pLog N'alta en Clientes';` },
          edits: [{ line: 2, before: 'INSERT INTO dbo.Clientes (nombre) VALUES (@n);', after: `INSERT INTO dbo.${q} (nombre) VALUES (@n);` }],
          unresolved: [{ line: 3, text: "EXEC dbo.pLog N'alta en Clientes';", reason: 'in_string' }],
        },
        original: "CREATE PROCEDURE dbo.pAltaCliente @n nvarchar(80) AS\nINSERT INTO dbo.Clientes (nombre) VALUES (@n);\nEXEC dbo.pLog N'alta en Clientes';",
      },
      {
        dependent: {
          kind: 'procedure', schema: 'dbo', name: 'pDinamico', parent: null, relation: 'code', confidence: 'review', detail: null,
          mentions: [{ line: 1, text: "CREATE PROCEDURE dbo.pDinamico AS EXEC sp_executesql N'SELECT * FROM dbo.Clientes'", dynamic: true }],
        },
        action: { kind: 'manual', reason: 'dynamic' }, original: null,
      },
      {
        dependent: { kind: 'function', schema: 'ventas', name: 'fTotal', parent: null, relation: 'code', confidence: 'probable', detail: null, mentions: [{ line: 4, text: 'FROM Clientes', dynamic: false }] },
        action: { kind: 'manual', reason: 'no_match', unresolved: [{ line: 4, text: 'FROM Clientes', reason: 'other_schema' }] }, original: null,
      },
    ],
  };
}

function script(args: { request: { new_name: string }; rewrites: { object: CodeObject; schemabound: boolean }[] }) {
  const to = args.request.new_name;
  const bound = args.rewrites.filter((r) => r.schemabound);
  return {
    statements: [
      ...bound.map((r) => `DROP VIEW IF EXISTS [dbo].[${r.object.name}];`),
      `EXEC sp_rename N'dbo.Clientes', N'${to.replace(/'/g, "''")}', N'OBJECT';`,
      ...args.rewrites.map((r) => r.object.definition.replace(/^CREATE /, 'CREATE OR ALTER ')),
    ],
    warnings: bound.length ? [`Se borran y se vuelven a crear ${bound.map((r) => `«${r.object.name}»`).join(', ')}: se pierden los permisos otorgados sobre ellos.`] : [],
  };
}

export function installRenamePreview() {
  const conns = useConnectionsStore();
  conns.drivers = sampleDrivers.map((d) => ({ ...d, rename: spec }));
  conns.list = [{
    id: 'c1', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: params.get('prod') === '0' ? ['dev'] : ['prod'], save_password: true, updated_at: '',
    config: { driver: sampleDrivers[0].id, host: 'sql-prod-01', port: 1433, database: 'ventas', username: 'app', password: null,
      encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.live.c1 = { status: 'connected', serverVersion: 'SQL Server 2022', databases: ['ventas'], defaultDatabase: 'ventas', error: null };
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'rename_impact':
          await new Promise((r) => setTimeout(r, 300));
          return impact(String(a?.args?.new_name ?? ''));
        case 'rename_script': return script(a?.args as never);
        case 'schema_sync_run':
          await new Promise((r) => setTimeout(r, 600));
          return params.get('fail')
            ? { done: 2, failed: [2, "Cannot alter 'dbo.vSaldos' because it is being referenced by object 'vResumen'."], rolled_back: true }
            : { done: (a?.args?.statements as string[]).length, failed: null, rolled_back: false };
        default: throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      }
    },
  };
}
