// Sample data for the development preview (dev-preview.html).
import type { Plan, PlanNode, ResultColumn, Cell } from '../api/types';
import type { ColumnDef, ForeignKeyDef, IndexDef } from '../api/schema-types';
import type { Field, FieldKind } from '../api/types';
import type { DesignerSpec, TableSchema } from '../api/schema-types';

function n(op: string, o: Partial<PlanNode> = {}, children: PlanNode[] = []): PlanNode {
  return {
    op, detail: '', object: null, total_cost: null, self_cost: null, est_rows: null, actual_rows: null,
    executions: 1, actual_ms: null, warnings: [], props: [], children, ...o,
  };
}

// Shaped like the plan SQL Server returned for the report query in the
// tests (orders × customers), extended with a lookup branch.
const root = n('SELECT', {
  total_cost: 2.41, self_cost: 0, est_rows: 4, actual_rows: 4, actual_ms: 41,
  warnings: ['Índice faltante (impacto 83.4%): CREATE NONCLUSTERED INDEX [IX_sugerido] ON [dbo].[pedidos] ([cliente_id], [estado], [fecha]) INCLUDE ([total])'],
  props: [['Grado de paralelismo', '1'], ['Memoria concedida (KB)', '1024']],
}, [
  n('Sort', { detail: 'Order By', total_cost: 2.41, self_cost: 0.0114, est_rows: 4, actual_rows: 4, actual_ms: 41, props: [['Columnas de orden', 'total DESC']] }, [
    n('Compute Scalar', { total_cost: 2.40, self_cost: 0, est_rows: 4 }, [
      n('Hash Match', { detail: 'Aggregate', total_cost: 2.40, self_cost: 0.2805, est_rows: 4, actual_rows: 4, actual_ms: 40 }, [
        n('Hash Match', { detail: 'Inner Join', total_cost: 2.12, self_cost: 0.738, est_rows: 39317, actual_rows: 23206, actual_ms: 27,
          props: [['Claves hash', '[c].[id] = [p].[cliente_id]'], ['Residual', '[p].[cliente_id]=[c].[id]']] }, [
          n('Clustered Index Scan', { object: 'dbo.clientes.PK_clientes', total_cost: 0.0406, self_cost: 0.0406, est_rows: 5000, actual_rows: 5000, actual_ms: 1 }),
          n('Nested Loops', { detail: 'Inner Join', total_cost: 1.34, self_cost: 0.21, est_rows: 39317, actual_rows: 23206, actual_ms: 19 }, [
            n('Index Seek', { object: 'dbo.pedidos.ix_pedidos_fecha', detail: 'Rango: fecha >= 2026-01-01', total_cost: 0.18, self_cost: 0.18, est_rows: 118000, actual_rows: 69615, actual_ms: 6 }),
            n('Key Lookup', { detail: 'Clustered', object: 'dbo.pedidos.PK_pedidos', total_cost: 0.95, self_cost: 0.95, est_rows: 1, actual_rows: 23206, executions: 69615, actual_ms: 12,
              warnings: ['Conversión implícita que afecta el plan: CONVERT_IMPLICIT(nvarchar(20),[p].[estado],0)'],
              props: [['Predicado', "[p].[estado]='pagado'"], ['Lecturas lógicas', '209145']] }),
          ]),
        ]),
      ]),
    ]),
  ]),
]);

export const samplePlans: Plan[] = [
  {
    statement: "select c.ciudad, count(*) as pedidos, sum(p.total) as total\nfrom pedidos p join clientes c on c.id = p.cliente_id\nwhere p.estado = 'pagado' and p.fecha >= '2026-01-01'\ngroup by c.ciudad order by total desc;",
    root, actual: true, raw_format: 'showplan_xml', raw: '<ShowPlanXML …/>',
  },
  {
    statement: "update pedidos set estado = 'anulado' where fecha < '2025-01-01';",
    root: n('UPDATE', { total_cost: 1.2, self_cost: 0, est_rows: 5000 }, [
      n('Clustered Index Update', { object: 'dbo.pedidos.PK_pedidos', total_cost: 1.2, self_cost: 0.9, est_rows: 5000 }, [
        n('Index Seek', { object: 'dbo.pedidos.ix_pedidos_fecha', total_cost: 0.3, self_cost: 0.3, est_rows: 5000 }),
      ]),
    ]),
    actual: false, raw_format: 'showplan_xml', raw: '',
  },
];

const cities = ['Buenos Aires', 'Córdoba', 'Rosario', 'Mendoza', 'La Plata', 'Tucumán'];
const months = ['2026-01', '2026-02', '2026-03', '2026-04', '2026-05', '2026-06', '2026-07', '2026-08'];
export const sampleResult: { columns: ResultColumn[]; rows: Cell[][] } = {
  columns: [
    { name: 'mes', type_name: 'varchar' },
    { name: 'ciudad', type_name: 'varchar' },
    { name: 'pedidos', type_name: 'int' },
    { name: 'total', type_name: 'decimal' },
  ],
  rows: months.flatMap((m, i) =>
    cities.map((c, j) => [m, c, 120 + ((i * 37 + j * 53) % 90), String((15000 + ((i * 3100 + j * 2700) % 9000)).toFixed(2))] as Cell[])),
};

// -- table designer (view=designer) --------------------------------------------------------
function field(key: string, label: string, kind: FieldKind, o: Partial<Field> = {}): Field {
  return { key, label, kind, required: false, secret: false, placeholder: '', default: '', help: '', ...o };
}

/** Shaped like SQL Server's DesignerSpec: schemas, comments (extended properties), options. */
export const sampleDesignerMssql: DesignerSpec = {
  kind: 'table',
  label: 'Nueva tabla',
  data_types: [
    'int', 'bigint', 'smallint', 'tinyint', 'bit', 'decimal(18, 2)', 'numeric(18, 0)', 'money', 'float', 'real',
    'date', 'time', 'datetime2', 'datetimeoffset', 'char(10)', 'varchar(50)', 'varchar(max)', 'nvarchar(50)',
    'nvarchar(255)', 'nvarchar(max)', 'uniqueidentifier', 'varbinary(max)', 'xml',
  ],
  schemas: true, primary_key: true, auto_increment: true, defaults: true, nullability: true,
  comments: true, indexes: true, foreign_keys: true,
  column_options: [
    field('collation', 'Intercalación', { type: 'text' }, { placeholder: 'predeterminada', help: 'COLLATE de la columna (solo texto).' }),
    field('sparse', 'Sparse', { type: 'bool' }, { default: 'false' }),
  ],
  table_options: [
    field('filegroup', 'Grupo de archivos', { type: 'text' }, { placeholder: 'PRIMARY' }),
    field('data_compression', 'Compresión de datos', { type: 'select', options: [['NONE', 'Ninguna'], ['ROW', 'Por fila'], ['PAGE', 'Por página']] }),
    field('system_versioning', 'Tabla temporal (SYSTEM_VERSIONING)', { type: 'bool' }, { default: 'false', help: 'Agrega las columnas de período y la tabla de historial.' }),
  ],
  columns_required: true,
};

/** Shaped like MongoDB's: a collection whose "columns" are $jsonSchema validator fields. */
export const sampleDesignerMongo: DesignerSpec = {
  kind: 'collection',
  label: 'Nueva colección',
  data_types: ['string', 'int', 'long', 'double', 'decimal', 'bool', 'date', 'objectId', 'object', 'array', 'binData', 'null'],
  schemas: false, primary_key: false, auto_increment: false, defaults: false, nullability: false,
  comments: false, indexes: true, foreign_keys: false,
  column_options: [
    field('required', 'Requerido', { type: 'bool' }, { default: 'false' }),
    field('description', 'Descripción', { type: 'text' }),
    field('pattern', 'Patrón', { type: 'text' }, { placeholder: 'regex' }),
  ],
  table_options: [
    field('validation_level', 'Nivel de validación', { type: 'select', options: [['strict', 'strict'], ['moderate', 'moderate'], ['off', 'off']] }, { default: 'strict' }),
    field('validation_action', 'Acción de validación', { type: 'select', options: [['error', 'error'], ['warn', 'warn']] }, { default: 'error' }),
    field('capped', 'Colección limitada (capped)', { type: 'bool' }, { default: 'false' }),
    field('size', 'Tamaño máximo (bytes)', { type: 'number' }, { help: 'Obligatorio si es capped.' }),
    field('max', 'Documentos máximos', { type: 'number' }),
    field('timeseries', 'Serie de tiempo', { type: 'textarea' }, { placeholder: '{ "timeField": "ts", "metaField": "meta", "granularity": "minutes" }' }),
  ],
  columns_required: false,
};

export const sampleExistingTables: { schema: string | null; name: string; columns: string[] }[] = [
  { schema: 'dbo', name: 'clientes', columns: ['id', 'nombre', 'ciudad', 'email'] },
  { schema: 'dbo', name: 'productos', columns: ['id', 'sku', 'descripcion', 'precio'] },
  { schema: 'ventas', name: 'sucursales', columns: ['id', 'nombre'] },
];

const col = (name: string, data_type: string, o: Partial<TableSchema['columns'][number]> = {}) =>
  ({ name, data_type, nullable: true, default_value: null, auto_increment: false, comment: null, options: {}, ...o });

/** A half-designed `pedidos` table (SQL Server). */
export const sampleDesignerInitialMssql: TableSchema = {
  kind: 'table', schema: 'dbo', name: 'pedidos',
  columns: [
    col('id', 'int', { nullable: false, auto_increment: true }),
    col('cliente_id', 'int', { nullable: false }),
    col('fecha', 'datetime2', { nullable: false, default_value: 'sysutcdatetime()' }),
    col('estado', 'nvarchar(20)', { nullable: false, default_value: "N'pendiente'", comment: 'pendiente | pagado | anulado' }),
    col('total', 'decimal(18, 2)'),
    col('notas', 'nvarchar(max)', { options: { collation: 'Latin1_General_CI_AI' } }),
  ],
  primary_key: { name: 'PK_pedidos', columns: ['id'] },
  foreign_keys: [{ name: 'FK_pedidos_clientes', columns: ['cliente_id'], ref_schema: 'dbo', ref_table: 'clientes', ref_columns: ['id'], on_delete: 'NO ACTION', on_update: null }],
  indexes: [{ name: 'ix_pedidos_fecha', columns: ['fecha', 'estado'], unique: false, kind: 'NONCLUSTERED', filter: null }],
  comment: 'Pedidos de clientes', options: { data_compression: 'PAGE' },
};

/** A `eventos` collection with validator fields (MongoDB). */
export const sampleDesignerInitialMongo: TableSchema = {
  kind: 'collection', schema: null, name: 'eventos',
  columns: [
    col('tipo', 'string', { options: { required: 'true', description: 'login | compra | error', pattern: '^[a-z_]+$' } }),
    col('usuario_id', 'objectId', { options: { required: 'true' } }),
    col('ts', 'date', { options: { required: 'true' } }),
    col('payload', 'object'),
  ],
  primary_key: null, foreign_keys: [],
  indexes: [{ name: 'idx_eventos_ts', columns: ['ts'], unique: false, kind: null, filter: null }],
  comment: null, options: { validation_level: 'moderate', validation_action: 'warn' },
};

// ---- script generator / import / run-script dialogs (view=script|import|run) ----
export const sampleKindLabels: Record<string, string> = {
  table: 'Tablas', view: 'Vistas', procedure: 'Procedimientos', function: 'Funciones', trigger: 'Triggers',
};
export const sampleScriptObjects: { kind: string; schema: string | null; name: string }[] = [
  ...['clientes', 'pedidos', 'pedido_items', 'productos', 'categorias', 'proveedores', 'stock', 'facturas', 'pagos', 'usuarios', 'auditoria']
    .map((name) => ({ kind: 'table', schema: 'dbo', name })),
  ...['v_ventas_mensuales', 'v_clientes_activos', 'v_stock_bajo'].map((name) => ({ kind: 'view', schema: 'dbo', name })),
  ...['sp_cerrar_mes', 'sp_recalcular_stock'].map((name) => ({ kind: 'procedure', schema: 'dbo', name })),
  { kind: 'function', schema: 'dbo', name: 'fn_total_pedido' },
  { kind: 'trigger', schema: 'dbo', name: 'tr_pedidos_auditoria' },
];
export const sampleImportTables: { schema: string | null; name: string; columns: string[] }[] = [
  { schema: 'dbo', name: 'clientes', columns: ['id', 'nombre', 'email', 'ciudad', 'alta', 'activo', 'limite_credito'] },
  { schema: 'dbo', name: 'pedidos', columns: ['id', 'cliente_id', 'fecha', 'estado', 'total'] },
];
const importPreview = {
  format: 'csv',
  columns: [
    { name: 'ID', inferred_type: 'integer' }, { name: 'Nombre', inferred_type: 'text' },
    { name: 'Email', inferred_type: 'text' }, { name: 'Ciudad', inferred_type: 'text' },
    { name: 'Alta', inferred_type: 'date' }, { name: 'Activo', inferred_type: 'boolean' },
    { name: 'Saldo', inferred_type: 'number' }, { name: 'Observaciones', inferred_type: 'text' },
  ],
  rows: Array.from({ length: 50 }, (_, i) => [
    1000 + i, ['Ana Pérez', 'Luis Gómez', 'Marta Díaz', 'Jorge Ruiz', 'Sofía Luna'][i % 5],
    `cliente${1000 + i}@ejemplo.com`, cities[i % cities.length], `2025-${String((i % 12) + 1).padStart(2, '0')}-1${i % 9}`,
    i % 3 !== 0, Math.round(((i * 7919) % 100000) + 0.5) / 100, i % 4 === 0 ? null : 'Cliente mayorista',
  ] as Cell[]),
  sheets: [],
};

/** Stands in for Tauri's IPC in the preview: sample answers, and long jobs never finish. */
export function installTauriMock() {
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    convertFileSrc: (p: string) => p,
    invoke: async (cmd: string) => {
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'plugin:dialog|open': return '/Users/demo/Descargas/clientes_2026.csv';
        case 'plugin:dialog|save': return null;
        case 'preview_import_file': return importPreview;
        case 'run_script_file': return {
          statements: 18_342, elapsed_ms: 41_870,
          errors: [
            "Línea 1204: Invalid object name 'dbo.v_legacy_ventas'.",
            "Línea 8931: Violation of PRIMARY KEY constraint 'PK_productos'. Cannot insert duplicate key in object 'dbo.productos'. The duplicate key value is (1042).",
            "Línea 15220: The INSERT statement conflicted with the FOREIGN KEY constraint 'FK_pedidos_clientes'.",
          ],
        };
        case 'generate_script': case 'import_file': return new Promise(() => {});
        default: throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      }
    },
  };
}

// -- ER diagram (view=diagram, diagram-big, diagram-nofk, diagram-empty) ------------------------
/** `name type [!]` — `!` = NOT NULL. */
function cols(spec: string): ColumnDef[] {
  return spec.split(/,(?![^(]*\))/).map((s) => {
    const [name, ...rest] = s.trim().split(/\s+/);
    const notNull = rest[rest.length - 1] === '!';
    if (notNull) rest.pop();
    return { name, data_type: rest.join(' '), nullable: !notNull, default_value: null, auto_increment: false, comment: null, options: {} };
  });
}
function fk(columns: string, refTable: string, refColumns = 'id', o: Partial<ForeignKeyDef> = {}): ForeignKeyDef {
  return { name: `fk_${refTable}_${columns.replace(/,\s*/g, '_')}`, columns: columns.split(/,\s*/), ref_schema: null, ref_table: refTable, ref_columns: refColumns.split(/,\s*/), on_delete: null, on_update: null, ...o };
}
function ix(name: string, columns: string, unique = false): IndexDef {
  return { name, columns: columns.split(/,\s*/), unique, kind: 'btree', filter: null };
}
function tbl(schema: string, name: string, spec: string, o: Partial<TableSchema> = {}): TableSchema {
  const columns = cols(spec);
  columns[0].auto_increment = columns[0].name === 'id';
  return {
    kind: 'table', schema, name, columns, primary_key: { name: `pk_${name}`, columns: ['id'] },
    foreign_keys: [], indexes: [], comment: null, options: {}, ...o,
  };
}

export const sampleSchema: TableSchema[] = [
  tbl('ventas', 'clientes', 'id bigint !, nombre varchar(120) !, email varchar(160) !, telefono varchar(40), cuit char(11), direccion_id bigint, creado_en timestamptz !, activo boolean !',
    { foreign_keys: [fk('direccion_id', 'direcciones', 'id', { on_delete: 'SET NULL' })], indexes: [ix('ux_clientes_email', 'email', true), ix('ix_clientes_nombre', 'nombre')], comment: 'Clientes minoristas y mayoristas' }),
  tbl('ventas', 'direcciones', 'id bigint !, calle varchar(120) !, numero varchar(10), ciudad varchar(80) !, provincia varchar(60) !, codigo_postal varchar(10)'),
  tbl('ventas', 'pedidos', 'id bigint !, cliente_id bigint !, sucursal_id int !, vendedor_id int, fecha date !, estado varchar(20) !, total numeric(14,2) !, observaciones text',
    { foreign_keys: [fk('cliente_id', 'clientes', 'id', { on_delete: 'RESTRICT' }), fk('sucursal_id', 'sucursales', 'id', { ref_schema: 'rrhh' }), fk('vendedor_id', 'empleados', 'id', { ref_schema: 'rrhh', on_delete: 'SET NULL' })],
      indexes: [ix('ix_pedidos_fecha', 'fecha'), ix('ix_pedidos_cliente_estado', 'cliente_id, estado, fecha')] }),
  tbl('ventas', 'pedido_items', 'pedido_id bigint !, linea int !, producto_id bigint !, cantidad int !, precio_unitario numeric(12,2) !, descuento numeric(5,2)',
    { primary_key: { name: 'pk_pedido_items', columns: ['pedido_id', 'linea'] },
      foreign_keys: [fk('pedido_id', 'pedidos', 'id', { on_delete: 'CASCADE' }), fk('producto_id', 'productos')] }),
  tbl('ventas', 'productos', 'id bigint !, sku varchar(32) !, nombre varchar(160) !, categoria_id int !, proveedor_id int, precio numeric(12,2) !, stock int !, activo boolean !',
    { foreign_keys: [fk('categoria_id', 'categorias'), fk('proveedor_id', 'proveedores', 'id', { on_delete: 'SET NULL' })], indexes: [ix('ux_productos_sku', 'sku', true)] }),
  tbl('ventas', 'categorias', 'id int !, nombre varchar(80) !, padre_id int',
    { foreign_keys: [fk('padre_id', 'categorias', 'id', { name: 'fk_categorias_padre' })] }),
  tbl('ventas', 'proveedores', 'id int !, razon_social varchar(160) !, cuit char(11) !, email varchar(160), telefono varchar(40), direccion_id bigint',
    { foreign_keys: [fk('direccion_id', 'direcciones')] }),
  tbl('ventas', 'pagos', 'id bigint !, pedido_id bigint !, medio varchar(20) !, importe numeric(14,2) !, fecha timestamptz !, referencia varchar(64)',
    { foreign_keys: [fk('pedido_id', 'pedidos', 'id', { on_delete: 'CASCADE' })] }),
  tbl('ventas', 'envios', 'id bigint !, pedido_id bigint !, direccion_id bigint !, transportista varchar(60), tracking varchar(64), despachado_en timestamptz, entregado_en timestamptz',
    { foreign_keys: [fk('pedido_id', 'pedidos', 'id', { on_delete: 'CASCADE' }), fk('direccion_id', 'direcciones')] }),
  tbl('rrhh', 'empleados', 'id int !, legajo varchar(12) !, nombre varchar(120) !, apellido varchar(120) !, sucursal_id int !, jefe_id int, ingreso date !, email varchar(160), telefono varchar(40), cuil char(11) !, puesto varchar(60), salario numeric(12,2), activo boolean !, creado_en timestamptz !, actualizado_en timestamptz, notas text',
    { foreign_keys: [fk('sucursal_id', 'sucursales'), fk('jefe_id', 'empleados', 'id', { name: 'fk_empleados_jefe' })], indexes: [ix('ux_empleados_legajo', 'legajo', true)] }),
  tbl('rrhh', 'sucursales', 'id int !, nombre varchar(80) !, direccion_id bigint, gerente_id int',
    { foreign_keys: [fk('direccion_id', 'direcciones', 'id', { ref_schema: 'ventas' })] }),
  tbl('sistema', 'auditoria', 'id bigint !, tabla varchar(64) !, operacion char(1) !, registro_id varchar(64) !, usuario varchar(64) !, fecha timestamptz !, datos jsonb',
    { primary_key: { name: 'pk_auditoria', columns: ['id'] }, indexes: [ix('ix_auditoria_fecha', 'fecha'), ix('ix_auditoria_tabla', 'tabla, registro_id')], comment: 'Bitácora de cambios (sin claves foráneas)' }),
];

/** 150 tables × 20 columns with ~1.3 foreign keys each (performance check). */
export function bigSchema(tables = 150, columns = 20): TableSchema[] {
  const types = ['int', 'bigint', 'varchar(80)', 'numeric(12,2)', 'timestamptz', 'boolean', 'text', 'date'];
  const out: TableSchema[] = [];
  for (let i = 0; i < tables; i++) {
    const refs = i === 0 ? [] : [Math.floor(i * ((i * 0.618034) % 1)), ...(i % 3 === 0 && i > 4 ? [Math.floor(i * ((i * 0.414214) % 1))] : [])];
    const spec = ['id bigint !', ...refs.map((r, j) => `t${r}_id${j || ''} bigint !`)];
    for (let c = spec.length; c < columns; c++) spec.push(`campo_${c} ${types[(i + c) % types.length]}${c % 4 === 0 ? ' !' : ''}`);
    out.push(tbl('public', `tabla_${String(i).padStart(3, '0')}`, spec.join(', '), {
      foreign_keys: refs.map((r, j) => fk(`t${r}_id${j || ''}`, `tabla_${String(r).padStart(3, '0')}`)),
    }));
  }
  return out;
}

/** Real-database shapes that broke the layout before: several foreign keys
 *  between the same pair of tables (created_by / updated_by → usuarios),
 *  cycles and a few hub tables. `view=diagram-parallel`. */
export function parallelFkSchema(tables = 120): TableSchema[] {
  let seed = 4242;
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
  const names = Array.from({ length: tables }, (_, i) => (i === 0 ? 'usuarios' : i === 1 ? 'empresas' : `tabla_${String(i).padStart(3, '0')}`));
  return names.map((name, i) => {
    const fks: TableSchema['foreign_keys'] = [];
    const cols: TableSchema['columns'] = [
      { name: 'id', data_type: 'int', nullable: false, default_value: null, auto_increment: true, comment: null, options: {} },
    ];
    const fk = (col: string, ref: string) => {
      cols.push({ name: col, data_type: 'int', nullable: true, default_value: null, auto_increment: false, comment: null, options: {} });
      fks.push({ name: `fk_${name}_${col}`, columns: [col], ref_schema: 'dbo', ref_table: ref, ref_columns: ['id'], on_delete: null, on_update: null });
    };
    if (i > 0) { fk('creado_por', 'usuarios'); fk('modificado_por', 'usuarios'); }
    if (i > 1) fk('empresa_id', 'empresas');
    const extra = Math.floor(rnd() * 3);
    for (let k = 0; k < extra; k++) {
      const target = names[Math.floor(rnd() * tables)];
      if (target !== name) fk(`${target}_id_${k}`, target);
    }
    for (let k = 0; k < 4 + Math.floor(rnd() * 8); k++) {
      cols.push({ name: `campo_${k}`, data_type: 'nvarchar(50)', nullable: true, default_value: null, auto_increment: false, comment: null, options: {} });
    }
    return { kind: 'table', schema: 'dbo', name, columns: cols, primary_key: { name: `pk_${name}`, columns: ['id'] }, foreign_keys: fks, indexes: [], comment: null, options: {} };
  });
}

// -- Configuración (view=settings&section=general|sync&state=none|pending|remote|active|error) ---------
export function installSettingsMock(state: string) {
  let cb = 0;
  const providers = [
    { kind: 'google_drive', label: 'Google Drive', available: true },
    { kind: 'onedrive', label: 'OneDrive', available: state !== 'none' },
    { kind: 'folder', label: 'Carpeta', available: true },
  ];
  const provider = state === 'none' ? null : 'google_drive';
  const status = {
    config: { provider, folder: null, account: provider ? 'ana@example.com' : null, enabled: ['active', 'error'].includes(state), auto: true },
    providers,
    status: {
      running: false,
      last_error: state === 'error' ? 'la frase clave no es correcta (o el backup está dañado)' : null,
      last_error_kind: state === 'error' ? 'wrong_passphrase' : null,
      last_action: null,
      last_run_at: null,
    },
    dirty: false,
    last_sync_at: new Date(Date.now() - 4 * 60_000).toISOString(),
  };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    invoke: async (cmd: string) => {
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'sync_status': return status;
        case 'list_settings': return { 'grid.copyFormat': 'csv', 'query.maxRows': 5000 };
        case 'sync_local_backups': return state === 'active' ? [
          { path: '/x/a.json', updated_at: '2026-09-20T14:02:00Z', device: 'MacBook Pro de Martín', size: 48_120 },
          { path: '/x/b.json', updated_at: '2026-09-12T09:40:00Z', device: 'MacBook Pro de Martín', size: 45_300 },
        ] : [];
        default: return null;
      }
    },
  };
}
