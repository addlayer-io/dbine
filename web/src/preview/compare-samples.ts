// Sample data for dev-preview.html?view=compare: two databases and a fixed
// comparison result (the real one comes from the backend).
import type { ColumnDef, TableSchema } from '../api/schema-types';

const col = (name: string, data_type: string, nullable = true, extra: Partial<ColumnDef> = {}): ColumnDef =>
  ({ name, data_type, nullable, default_value: null, auto_increment: false, comment: null, options: {}, ...extra });
const table = (name: string, columns: ColumnDef[], extra: Partial<TableSchema> = {}): TableSchema =>
  ({ kind: 'table', schema: 'public', name, columns, primary_key: { name: `${name}_pkey`, columns: ['id'] }, foreign_keys: [], indexes: [], comment: null, options: {}, ...extra });

const left = {
  driver: 'postgres',
  tables: [
    table('clientes', [col('id', 'integer', false, { auto_increment: true }), col('nombre', 'varchar(10)'), col('email', 'varchar(100)', false, { default_value: "''" }), col('alta', 'date')],
      { indexes: [{ name: 'ix_clientes_nombre', columns: ['nombre'], unique: false, kind: null, filter: null }] }),
    table('pedidos', [col('id', 'integer', false), col('cliente_id', 'integer', false), col('total', 'numeric(12,2)')],
      { foreign_keys: [{ name: 'pedidos_cliente_id_fkey', columns: ['cliente_id'], ref_schema: 'public', ref_table: 'clientes', ref_columns: ['id'], on_delete: null, on_update: null }] }),
    table('productos', [col('id', 'integer', false), col('nombre', 'text')]),
    table('auditoria', [col('id', 'bigint', false), col('evento', 'text')]),
  ],
  objects: [{ kind: 'view', schema: 'public', name: 'v_clientes', definition: 'CREATE OR REPLACE VIEW public.v_clientes AS\n SELECT id,\n    nombre\n   FROM clientes\n  WHERE alta IS NOT NULL;' }],
  warnings: [],
};
const right = {
  driver: 'postgres',
  tables: [
    table('clientes', [col('id', 'integer', false, { auto_increment: true }), col('nombre', 'varchar(5)'), col('email', 'varchar(100)'), col('telefono', 'text')]),
    table('pedidos', [col('id', 'integer', false), col('cliente_id', 'integer', false), col('total', 'numeric(10,2)')]),
    table('productos', [col('id', 'integer', false), col('nombre', 'text')]),
    table('tmp_import', [col('id', 'integer', false), col('payload', 'jsonb')]),
  ],
  objects: [{ kind: 'view', schema: 'public', name: 'v_clientes', definition: 'CREATE OR REPLACE VIEW public.v_clientes AS\n SELECT id,\n    nombre\n   FROM clientes;' }],
  warnings: [],
};

const result = {
  tables: [
    {
      key: 'public.clientes', left: 0, right: 0, status: 'changed', primary_key: 'equal', fields: [],
      columns: [
        { name: 'id', left: 0, right: 0, status: 'equal', fields: [] },
        { name: 'nombre', left: 1, right: 1, status: 'changed', fields: ['type'] },
        { name: 'email', left: 2, right: 2, status: 'changed', fields: ['nullable', 'default'] },
        { name: 'alta', left: 3, right: null, status: 'only_left', fields: [] },
        { name: 'telefono', left: null, right: 3, status: 'only_right', fields: [] },
      ],
      indexes: [{ name: 'ix_clientes_nombre', left: 0, right: null, status: 'only_left', fields: [] }],
      foreign_keys: [],
    },
    {
      key: 'public.pedidos', left: 1, right: 1, status: 'changed', primary_key: 'equal', fields: [],
      columns: [
        { name: 'id', left: 0, right: 0, status: 'equal', fields: [] },
        { name: 'cliente_id', left: 1, right: 1, status: 'equal', fields: [] },
        { name: 'total', left: 2, right: 2, status: 'changed', fields: ['type'] },
      ],
      indexes: [],
      foreign_keys: [{ name: 'pedidos_cliente_id_fkey', left: 0, right: null, status: 'only_left', fields: [] }],
    },
    { key: 'public.productos', left: 2, right: 2, status: 'equal', primary_key: 'equal', fields: [], columns: [], indexes: [], foreign_keys: [] },
    { key: 'public.auditoria', left: 3, right: null, status: 'only_left', primary_key: 'equal', fields: [], columns: [], indexes: [], foreign_keys: [] },
    { key: 'public.tmp_import', left: null, right: 3, status: 'only_right', primary_key: 'equal', fields: [], columns: [], indexes: [], foreign_keys: [] },
  ],
  objects: [{ kind: 'view', key: 'public.v_clientes', left: 0, right: 0, status: 'changed' }],
};

/** The compare commands, or `undefined` for anything else. */
export function compareMock(cmd: string, a?: { args?: Record<string, unknown> }): unknown {
  switch (cmd) {
    case 'schema_compare_load':
      return JSON.parse(JSON.stringify(a?.args?.database === 'ventas' ? left : right));
    case 'schema_compare':
      return result;
    case 'schema_compare_convert':
      return { tables: a?.args?.tables ?? [], warnings: [] };
    case 'schema_sync_script':
      return {
        statements: [
          'ALTER TABLE "public"."clientes" DROP COLUMN "telefono";',
          'ALTER TABLE "public"."clientes" ADD COLUMN "alta" date NULL;',
          'ALTER TABLE "public"."clientes" ALTER COLUMN "nombre" TYPE varchar(10) USING "nombre"::varchar(10);',
          'CREATE INDEX "ix_clientes_nombre" ON "public"."clientes" ("nombre");',
        ],
        warnings: ['Se borra la columna public.clientes.telefono con sus datos.'],
      };
    default:
      return undefined;
  }
}
