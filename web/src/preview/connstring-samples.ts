import { h } from 'vue';
import ConnStringDialog from '../components/ConnStringDialog.vue';
import { useConnectionsStore } from '../stores/connections';
import { openConnString } from '../composables/connString';
import type { ConnStringBuildArgs } from '../api/connString';
import { sampleDrivers } from './monitor-samples';

// view=connstring[&include=1][&reveal=1][&engine=sqlserver]:
// "Copiar cadena de conexión…" over a fake backend (a PostgreSQL connection
// behind an SSH tunnel, or SQL Server with Entra ID). The texts are what
// src-tauri/src/connstring builds for these connections.

const MASK = '••••••••';
const PW = 'S3cr3t;p@ss';

const PG = {
  formats: [
    ['uri', 'URI (postgresql://)'], ['libpq', 'libpq (psql, psycopg)'], ['adonet', 'ADO.NET (Npgsql)'], ['jdbc', 'JDBC'],
    ['sqlalchemy', 'Python (SQLAlchemy)'], ['odbc', 'ODBC'], ['node', 'Node.js (pg)'], ['go', 'Go (pgx)'],
  ],
  text(format: string, pw: string, pct: string): string {
    switch (format) {
      case 'libpq': return `host=db-prod-01.internal port=5432 dbname=ventas user=app password=${pw.includes(';') ? `'${pw}'` : pw} sslmode=verify-full`;
      case 'adonet': return `Host=db-prod-01.internal;Port=5432;Database=ventas;Username=app;Password=${pw.includes(';') ? `"${pw}"` : pw};SSL Mode=VerifyFull;`;
      case 'jdbc': return `jdbc:postgresql://db-prod-01.internal:5432/ventas?user=app&password=${pct}&sslmode=verify-full`;
      case 'sqlalchemy': return `postgresql+psycopg://app:${pct}@db-prod-01.internal:5432/ventas?sslmode=verify-full`;
      case 'odbc': return `DRIVER={PostgreSQL Unicode};SERVER=db-prod-01.internal;PORT=5432;DATABASE=ventas;UID=app;PWD=${pw.includes(';') ? `{${pw}}` : pw};SSLmode=verify-full;`;
      case 'go': return `postgres://app:${pct}@db-prod-01.internal:5432/ventas?sslmode=verify-full`;
      default: return `postgresql://app:${pct}@db-prod-01.internal:5432/ventas?sslmode=verify-full`;
    }
  },
  notes: (format: string) => (format === 'odbc' ? [{ code: 'odbc_driver_name', args: { driver: 'PostgreSQL Unicode' } }] : []),
  info: { notes: [{ code: 'tunnel', args: { ssh: 'bastion.example.com' } }], has_secrets: true, blocked: null },
};

const MS = {
  formats: [
    ['adonet', 'ADO.NET (Microsoft.Data.SqlClient)'], ['jdbc', 'JDBC (Microsoft JDBC Driver)'], ['odbc', 'ODBC'],
    ['node', 'Node.js (mssql, tedious) · JSON'], ['go', 'Go (go-mssqldb)'],
  ],
  text(format: string, pw: string, pct: string): string {
    switch (format) {
      case 'jdbc': return `jdbc:sqlserver://crm.database.windows.net:1433;databaseName=crm;encrypt=true;authentication=ActiveDirectoryServicePrincipal;user=4f1c2d4e-77aa-4c1b-9d2e-5a0b6c7d8e9f;password=${pw.includes(';') ? `{${pw}}` : pw}`;
      case 'odbc': return `DRIVER={ODBC Driver 18 for SQL Server};SERVER=tcp:crm.database.windows.net,1433;DATABASE=crm;Authentication=ActiveDirectoryServicePrincipal;UID=4f1c2d4e-77aa-4c1b-9d2e-5a0b6c7d8e9f;PWD=${pw.includes(';') ? `{${pw}}` : pw};Encrypt=yes;`;
      case 'node': return JSON.stringify({ server: 'crm.database.windows.net', port: 1433, database: 'crm', authentication: { type: 'azure-active-directory-service-principal-secret', options: { clientId: '4f1c2d4e-77aa-4c1b-9d2e-5a0b6c7d8e9f', clientSecret: pw, tenantId: 'contoso.onmicrosoft.com' } }, options: { encrypt: true, trustServerCertificate: false } }, null, 2);
      case 'go': return `sqlserver://4f1c2d4e-77aa-4c1b-9d2e-5a0b6c7d8e9f:${pct}@crm.database.windows.net:1433?database=crm&encrypt=true&fedauth=ActiveDirectoryServicePrincipal`;
      default: return `Server=tcp:crm.database.windows.net,1433;Database=crm;Authentication=Active Directory Service Principal;User ID=4f1c2d4e-77aa-4c1b-9d2e-5a0b6c7d8e9f;Password=${pw.includes(';') ? `"${pw}"` : pw};Encrypt=True;`;
    }
  },
  notes: (format: string) => (format === 'go' ? [{ code: 'go_azuread', args: {} }] : format === 'odbc' ? [{ code: 'odbc_driver_name', args: { driver: 'ODBC Driver 18 for SQL Server' } }] : []),
  info: { notes: [{ code: 'secret_manager', args: {} }], has_secrets: true, blocked: null },
};

export function installConnStringPreview(params: URLSearchParams) {
  const ms = params.get('engine') === 'sqlserver';
  const s = ms ? MS : PG;
  const conns = useConnectionsStore();
  conns.drivers = sampleDrivers;
  conns.list = [{
    id: 'c1', name: ms ? 'CRM (Azure SQL)' : 'Producción · ventas', color: '#3794ff', folder_id: null, tags: ['prod'], save_password: true, updated_at: '',
    config: { driver: ms ? 'azuresql' : 'postgres', host: ms ? 'crm.database.windows.net' : 'db-prod-01.internal', port: ms ? 1433 : 5432, database: ms ? 'crm' : 'ventas', username: 'app', password: null,
      encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.live.c1 = { status: 'connected', serverVersion: '', databases: ms ? ['crm', 'crm_qa'] : ['ventas', 'ventas_historico', 'postgres'], defaultDatabase: ms ? 'crm' : 'ventas', error: null };
  const placeholder = ms ? '${DB_CLIENT_SECRET}' : '${DB_PASSWORD}';
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      if (cmd === 'connection_string_info') {
        return { formats: s.formats.map(([id, label]) => ({ id, label })), database: ms ? 'crm' : 'ventas', ...s.info };
      }
      if (cmd === 'connection_string_build') {
        const b = a?.args as unknown as ConnStringBuildArgs;
        const [pw, pct] = !b.include_secrets ? [placeholder, placeholder] : b.reveal ? [PW, encodeURIComponent(PW)] : [MASK, MASK];
        const text = s.text(b.format, pw, pct);
        return { text, notes: s.notes(b.format), secret: b.include_secrets && b.reveal };
      }
      throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
    },
  };
  openConnString('c1');
}

export function connStringPreview() {
  return h('div', { style: 'height: 100vh; background: var(--ide-editor)' }, [h(ConnStringDialog)]);
}

/** After the dialog loaded: &include=1, &reveal=1 (as clicks would). */
export function driveConnStringPreview(params: URLSearchParams) {
  const step = (sel: string) => document.querySelector<HTMLElement>(sel);
  setTimeout(() => {
    if (params.get('include')) step('.cs-include .el-switch')?.click();
    if (params.get('reveal')) setTimeout(() => step('.cs-preview-head .el-button')?.click(), 300);
  }, 400);
}
