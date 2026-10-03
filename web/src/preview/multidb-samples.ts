// view=multidb: "Ejecutar en varias bases…" with sample tenants (no backend).
import type { MultiDbResponse } from '../api/types';
import type { MultiDbLive } from '../composables/multiDb';

export const sampleTenantDatabases = [
  'master', 'model', 'msdb', 'tempdb',
  ...['acme', 'globex', 'initech', 'umbrella', 'hooli', 'soylent', 'wonka', 'tyrell'].map((n) => `tenant-${n}`),
  'tenant-norte', 'tenant-sur', 'tenant-centro',
  'reporting', 'staging',
];

const people = (db: string, n: number) =>
  Array.from({ length: n }, (_, i) => [db, 1000 + i, `Cliente ${i + 1} (${db.replace('tenant-', '')})`, `cliente${i + 1}@${db.replace('tenant-', '')}.com`, i % 3 === 0]);

export const sampleMultiDbResponse: MultiDbResponse = {
  needs_confirmation: null,
  merged: {
    columns: [{ name: 'base', type_name: '' }, { name: 'id', type_name: 'int' }, { name: 'nombre', type_name: 'nvarchar' }, { name: 'email', type_name: 'nvarchar' }, { name: 'activo', type_name: 'bit' }],
    rows: [...people('tenant-norte', 4), ...people('tenant-sur', 3), ...people('tenant-acme', 3)],
    total_rows: 10,
    truncated: false,
    rows_affected: null,
  },
  databases: [
    { database: 'tenant-norte', status: 'ok', error: null, rows: 4, rows_affected: null, elapsed_ms: 42, results: [], messages: [] },
    { database: 'tenant-sur', status: 'ok', error: null, rows: 3, rows_affected: null, elapsed_ms: 38, results: [], messages: [] },
    { database: 'tenant-acme', status: 'ok', error: null, rows: 3, rows_affected: null, elapsed_ms: 51, results: [], messages: [] },
    { database: 'tenant-centro', status: 'error', error: "Invalid object name 'ventas.clientes'.", rows: 0, rows_affected: null, elapsed_ms: 12, results: [], messages: [] },
  ],
  cancelled: false,
  elapsed_ms: 97,
};

/** A run halfway: two done, one failed, the rest waiting or running. */
export function sampleLive(): MultiDbLive {
  const dbs = ['tenant-norte', 'tenant-sur', 'tenant-centro', 'tenant-acme', 'tenant-globex', 'tenant-initech'];
  const states: MultiDbLive['states'] = {};
  for (const d of dbs) states[d] = { status: 'pending', rows: 0, elapsed_ms: 0, error: null };
  states['tenant-norte'] = { status: 'ok', rows: 4, elapsed_ms: 42, error: null };
  states['tenant-sur'] = { status: 'ok', rows: 3, elapsed_ms: 38, error: null };
  states['tenant-centro'] = { status: 'error', rows: 0, elapsed_ms: 12, error: "Invalid object name 'ventas.clientes'." };
  return { runId: 'r', taskId: 't', databases: dbs, states, done: 3, running: true };
}
