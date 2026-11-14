import type { DbObject, FunctionInfo } from '../api/types';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// view=query&sample=dax: the query editor on a Power BI-like connection
// (language dax), to see DAX's highlighting and completion (tables after
// `'`, columns after `'Ventas'[`, measures after `[`, functions with dots)
// without a Power BI tenant.

export const DAX_SAMPLE = `// Ventas por año: la medida del modelo contra la suma de la columna
DEFINE
\tMEASURE 'Ventas'[Margen %] = DIVIDE([Margen], [Total ventas])
\tVAR anio = 2024
EVALUATE
\tSUMMARIZECOLUMNS(
\t\t'Fecha'[Año],
\t\t"Modelo", [Total ventas],
\t\t"Suma", SUM('Ventas'[Importe]),
\t\t"Margen", [Margen %]
\t)
ORDER BY 'Fecha'[Año] DESC

-- Las 10 mejores líneas de pedido
EVALUATE
\tTOPN(10, FILTER('Líneas de pedido', [Cantidad] > 1 && 'Líneas de pedido'[Estado] <> "Anulado"), [Importe], DESC)

/* Las tablas del modelo,
   como las ve un lector */
EVALUATE INFO.VIEW.TABLES()
`;

const DB = 'Ventas';
const OBJECTS: DbObject[] = [
  { kind: 'table', schema: null, name: 'Ventas', parent: null },
  { kind: 'table', schema: null, name: 'Fecha', parent: null },
  { kind: 'table', schema: null, name: 'Líneas de pedido', parent: null },
  { kind: 'table', schema: null, name: 'Clientes', parent: null },
  { kind: 'measure', schema: null, name: 'Total ventas', parent: 'Ventas' },
  { kind: 'measure', schema: null, name: 'Margen', parent: 'Ventas' },
  { kind: 'measure', schema: null, name: 'Clientes activos', parent: 'Clientes' },
  { kind: 'hierarchy', schema: null, name: "'Fecha'[Calendario]", parent: 'Fecha' },
  { kind: 'relationship', schema: null, name: "'Ventas'[ClienteKey] → 'Clientes'[ClienteKey]", parent: null },
];
const COLUMNS: Record<string, string[]> = {
  Ventas: ['Importe', 'Costo', 'FechaKey', 'ClienteKey'],
  Fecha: ['Fecha', 'Año', 'Mes', 'FechaKey'],
  'Líneas de pedido': ['Pedido', 'Cantidad', 'Importe', 'Estado'],
  Clientes: ['ClienteKey', 'Nombre', 'País'],
};
const columnInfo = (name: string) => ({ name, data_type: 'String', nullable: true, primary_key: false, auto_increment: false, default_value: null });
const fn = (name: string, kind: FunctionInfo['kind'], args: string, returns = ''): FunctionInfo => ({ name, kind, args, returns, description: '', schema: '' });
const FUNCTIONS: FunctionInfo[] = [
  fn('SUM', 'aggregate', '(ColumnName)'), fn('DIVIDE', 'scalar', '(Numerator, Denominator, AlternateResult)'),
  fn('CALCULATE', 'scalar', '(Expression, Filter1, …)'), fn('FILTER', 'table', '(Table, FilterExpression)'),
  fn('TOPN', 'table', '(N_Value, Table, OrderBy_Expression, Order, …)'), fn('SUMMARIZECOLUMNS', 'table', '(GroupBy_ColumnName, …)'),
  fn('INFO.VIEW.TABLES', 'table', '()'), fn('INFO.VIEW.COLUMNS', 'table', '()'), fn('INFO.VIEW.MEASURES', 'table', '()'),
  fn('STDEVX.S', 'aggregate', '(Table, Expression)'), fn('DATE', 'scalar', '(Year, Month, Day)'), fn('NOT', 'scalar', '(Logical)'),
];

export function installDaxSample() {
  const conns = useConnectionsStore();
  conns.drivers = conns.drivers.map((d) => ({
    ...d,
    language: 'dax',
    dialect: 'powerbi',
    object_kinds: [
      { id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true },
      { id: 'measure', label: 'Medidas', has_columns: false, browsable: true, has_definition: true },
      { id: 'hierarchy', label: 'Jerarquías', has_columns: true, browsable: true, has_definition: true },
      { id: 'relationship', label: 'Relaciones', has_columns: false, browsable: false, has_definition: true },
    ],
  }));
  conns.objects[dbKey('c1', DB)] = { status: 'ready', items: OBJECTS, error: null };
  for (const [table, cols] of Object.entries(COLUMNS)) {
    conns.columns[objKey('c1', DB, null, table)] = { status: 'ready', error: null, items: cols.map(columnInfo) };
  }
  useTabsStore().tabs.push({ id: 'q', kind: 'query', connectionId: 'c1', database: DB, queryId: 'q1', preview: false });
}

installDaxSample.invoke = (cmd: string, args?: Record<string, unknown>): unknown => {
  if (cmd === 'lint_script') return [];
  if (cmd === 'list_database_objects') return { objects: OBJECTS, schemas: [] };
  if (cmd === 'get_columns') return (COLUMNS[(args?.object as { name: string }).name] ?? []).map(columnInfo);
  if (cmd === 'function_catalog') return FUNCTIONS;
  if (cmd === 'plugin:event|listen') return 1;
  return null;
};
