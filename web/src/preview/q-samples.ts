import type { DbObject } from '../api/types';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// view=query&sample=q: the query editor on a KDB+-like connection (language
// redis, dialect q), to see q's highlighting and completion without a kdb+.

export const Q_SAMPLE = `/ trades of the day
select avg price, sum size by sym from trade where date=last date, sym in \`AAPL\`MSFT
t:([] time:12:00:00.000 12:00:01.500; sym:\`a\`b; px:1.5 2.5e; n:1 2j)
.u.upd[\`trade;(.z.p;\`IBM;101.5;100i)]
f:{[x] :x+1}; f each til 10
+/ 1 2 3 / over
"a \\"quoted\\" string", 0N 0w 0Nd 0x2a 101b 2026.11.03D12:00:00.000000000
\`:data/hdb set t
\\t select from quote
/
a comment block
\\
`;

const DB = 'default';
const OBJECTS: DbObject[] = [
  { kind: 'table', schema: null, name: 'trade', parent: null },
  { kind: 'table', schema: null, name: 'quote', parent: null },
  { kind: 'function', schema: null, name: '.u.upd', parent: null },
  { kind: 'variable', schema: null, name: 'cfg', parent: null },
];
const COLUMNS: Record<string, string[]> = { trade: ['date', 'time', 'sym', 'price', 'size'], quote: ['time', 'sym', 'bid', 'ask'] };
const columnInfo = (name: string) => ({ name, data_type: 'symbol', nullable: true, primary_key: false, auto_increment: false, default_value: null });

export function installQSample() {
  const conns = useConnectionsStore();
  conns.drivers = conns.drivers.map((d) => ({
    ...d,
    language: 'redis',
    dialect: 'q',
    object_kinds: [
      { id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true },
      { id: 'function', label: 'Funciones', has_columns: false, browsable: false, has_definition: true },
      { id: 'variable', label: 'Variables', has_columns: false, browsable: true, has_definition: false },
    ],
  }));
  conns.objects[dbKey('c1', DB)] = { status: 'ready', items: OBJECTS, error: null };
  for (const [table, cols] of Object.entries(COLUMNS)) {
    conns.columns[objKey('c1', DB, null, table)] = { status: 'ready', error: null, items: cols.map(columnInfo) };
  }
  useTabsStore().tabs.push({ id: 'q', kind: 'query', connectionId: 'c1', database: DB, queryId: 'q1', preview: false });
}

installQSample.invoke = (cmd: string, args?: Record<string, unknown>): unknown => {
  if (cmd === 'lint_script') return [];
  if (cmd === 'list_database_objects') return { objects: OBJECTS, schemas: [] };
  if (cmd === 'get_columns') return (COLUMNS[(args?.object as { name: string }).name] ?? []).map(columnInfo);
  if (cmd === 'plugin:event|listen') return 1;
  return null;
};
