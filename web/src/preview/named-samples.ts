import type { QueryOutcome, ScriptUnit, StatementResult } from '../api/types';
import { useConnectionsStore } from '../stores/connections';

// view=query&sample=named: named results and the run lens over a fake
// PostgreSQL that runs statement by statement. The editor holds commented
// statements; `split_script` and `execute_query` answer as the backend would
// (a statement index, offset and line for each result). &run=1 runs the
// whole script once it loads; &again=1 then runs "Pedidos del mes" from its
// lens (its tab refreshes in place); &rename=1 opens the rename box on the
// first result tab. &mode=whole: an engine that gets the text whole and
// numbers its statements itself (as MongoDB's driver does); &mode=opaque:
// one that doesn't say which statement each result came from (Snowflake).
// &mode=mongo: MongoDB, whose splitter takes `//` for code (a statement
// starts on its comment lines); &mode=redis: Redis, one command per line,
// which its splitter sees as one statement (the engine numbers each line);
// &mode=tsql: SQL Server, where a script without GO is one batch.
// window.__namedCalls lists the backend calls (what the grid's editing
// looked up, say).

const SQL_SAMPLE = `-- Clientes activos
SELECT id, nombre, alta FROM clientes WHERE activo;

-- Pedidos del mes
SELECT p.id, c.nombre, p.total
FROM pedidos p
JOIN clientes c ON c.id = p.cliente_id
WHERE p.fecha >= date_trunc('month', now());

UPDATE pedidos SET revisado = true WHERE total > 1000;

-- Totales por día
SELECT fecha, SUM(total) AS total FROM pedidos GROUP BY fecha ORDER BY fecha;

SELECT id, nombre FROM proveedores WHERE activo;
`;

const MONGO_SAMPLE = `// Usuarios activos
db.users.find({ active: true });

// Pedidos del mes
db.orders.find({ month: 11 });

db.orders.updateMany({ total: { $gt: 1000 } }, { $set: { checked: true } });

// Totales por día
db.orders.aggregate([{ $group: { _id: "$day", total: { $sum: "$total" } } }]);
`;

const REDIS_SAMPLE = `# Usuario 1
HGETALL user:1

# Pedidos recientes
LRANGE orders 0 -1
INCR visits
# Visitas
GET visits
`;

const mode = new URLSearchParams(location.search).get('mode');
const language = mode === 'mongo' ? 'json' : mode === 'redis' ? 'redis' : 'sql';
export const NAMED_SAMPLE_SQL = mode === 'mongo' ? MONGO_SAMPLE : mode === 'redis' ? REDIS_SAMPLE : SQL_SAMPLE;

/** The script cut at `;` outside comments and quotes; a unit starts at its
 *  first word (comments before it aren't part of it), as the backend's.
 *  MongoDB's and Redis's `//` and `#` aren't comments to it: a unit starts
 *  on them, and a Redis script (no `;`) is one unit. */
function split(sql: string): ScriptUnit[] {
  const out: ScriptUnit[] = [];
  let start = -1;
  let i = 0;
  const lineAt = (at: number) => sql.slice(0, at).split('\n').length;
  const finish = (end: number) => {
    if (start >= 0) {
      const text = sql.slice(start, end).trimEnd();
      if (text) out.push({ text, start, end: start + text.length, line: lineAt(start), kind: 'sql', repeat: 1 });
    }
    start = -1;
  };
  while (i < sql.length) {
    const c = sql[i];
    if (c === '-' && sql[i + 1] === '-' && language === 'sql') {
      const nl = sql.indexOf('\n', i);
      i = nl < 0 ? sql.length : nl;
      continue;
    }
    if (c === "'") {
      if (start < 0) start = i;
      const close = sql.indexOf("'", i + 1);
      i = close < 0 ? sql.length : close + 1;
      continue;
    }
    if (c === ';') {
      finish(i);
      i++;
      continue;
    }
    if (start < 0 && !/\s/.test(c)) start = i;
    i++;
  }
  finish(sql.length);
  return out;
}

const col = (name: string, type_name: string) => ({ name, type_name });
let runs = 0;

/** The statements the engine itself sees: a Redis command per line that
 *  isn't a comment, a MongoDB statement from its code (past its `//` lines). */
function engineStatements(sql: string): ScriptUnit[] {
  if (language === 'redis') {
    const out: ScriptUnit[] = [];
    let at = 0;
    sql.split('\n').forEach((line, n) => {
      const t = line.trim();
      if (t && !t.startsWith('#')) {
        const start = at + line.indexOf(t);
        out.push({ text: t, start, end: start + t.length, line: n + 1, kind: 'sql', repeat: 1 });
      }
      at += line.length + 1;
    });
    return out;
  }
  return split(sql).map((u) => {
    const code = u.text.split('\n').findIndex((l) => !l.trim().startsWith('//'));
    const skip = u.text.split('\n').slice(0, Math.max(0, code)).reduce((n, l) => n + l.length + 1, 0);
    return { ...u, text: u.text.slice(skip), start: u.start + skip, line: u.line + Math.max(0, code) };
  });
}

/** A result like the server's for each statement. */
function resultOf(text: string, statement: number, unit: ScriptUnit): StatementResult {
  const at = { statement, offset: unit.start, line: unit.line, elapsed_ms: 4 + statement * 3 };
  const lower = text.toLowerCase();
  if (language !== 'sql') {
    const rows = lower.startsWith('incr') || lower.includes('updatemany') ? [[lower.includes('updatemany') ? '{ "modifiedCount": 12 }' : 8]]
      : lower.includes('aggregate') ? [['2026-11-10', 18250.5], ['2026-11-11', 20410]]
        : [['1', 'Ana Gómez'], ['2', `Luis Pérez ${runs}`]];
    const columns = rows[0].length === 1 ? [col('value', 'text')] : [col('_id', 'string'), col(lower.includes('aggregate') ? 'total' : 'value', 'string')];
    return { columns, rows, total_rows: rows.length, truncated: false, rows_affected: null, tag: null, ...at };
  }
  if (lower.includes('from proveedores')) {
    const rows = [[11, 'Distribuidora Sur'], [14, 'Papelera Norte']];
    return { columns: [col('id', 'int8'), col('nombre', 'varchar')], rows, total_rows: rows.length, truncated: false, rows_affected: null, tag: 'SELECT 2', ...at };
  }
  if (lower.startsWith('update')) return { columns: [], rows: [], total_rows: 0, truncated: false, rows_affected: 12, tag: 'UPDATE 12', ...at };
  if (lower.includes('sum(total)')) {
    const rows = [['2026-11-10', 18250.5], ['2026-11-11', 20410], ['2026-11-12', 15990.25 + runs * 100]];
    return { columns: [col('fecha', 'date'), col('total', 'numeric')], rows, total_rows: rows.length, truncated: false, rows_affected: null, tag: 'SELECT 3', ...at };
  }
  if (lower.includes('from pedidos p')) {
    const rows = [[3101, 'Ana Gómez', 1250], [3102, 'Luis Pérez', 980.5], [3107, 'Ana Gómez', 2310], [3110 + runs, 'Carla Ruiz', 450]];
    return { columns: [col('id', 'int8'), col('nombre', 'varchar'), col('total', 'numeric')], rows, total_rows: rows.length, truncated: false, rows_affected: null, tag: `SELECT ${rows.length}`, ...at };
  }
  const rows = [[1, 'Ana Gómez', '2024-03-02'], [2, 'Luis Pérez', '2024-06-18'], [5, 'Carla Ruiz', '2025-01-09']];
  return { columns: [col('id', 'int8'), col('nombre', 'varchar'), col('alta', 'date')], rows, total_rows: rows.length, truncated: false, rows_affected: null, tag: 'SELECT 3', ...at };
}

/** SQL Server's units without GO: one batch from the first statement on. */
function batches(sql: string): ScriptUnit[] {
  const units = split(sql);
  if (!units.length) return [];
  const start = units[0].start;
  const text = sql.slice(start).trimEnd();
  return [{ text, start, end: start + text.length, line: units[0].line, kind: 'batch', repeat: 1 }];
}

export function installNamedSample() {
  const conns = useConnectionsStore();
  conns.drivers = conns.drivers.map((d) => ({
    ...d, script_mode: mode === 'tsql' ? 'batches' : mode ? 'whole' : 'per_statement', supports_explain: language === 'sql',
    ...(language === 'sql' ? {} : { language, dialect: '' }),
  }));
}

const calls: [string, unknown][] = [];
(window as unknown as { __namedCalls: typeof calls }).__namedCalls = calls;

installNamedSample.invoke = async (cmd: string, args?: Record<string, unknown>): Promise<unknown> => {
  calls.push([cmd, args]);
  if (cmd === 'split_script') {
    const sql = String(args?.sql ?? '');
    const statements = !!args?.statements;
    return mode === 'tsql' && !statements ? batches(sql) : split(sql);
  }
  if (cmd === 'results_runs') return [];
  if (cmd === 'execute_query') {
    runs++;
    const sql = String(args?.sql ?? '');
    const units = mode === 'mongo' || mode === 'redis' ? engineStatements(sql) : split(sql);
    await new Promise((r) => setTimeout(r, 250));
    // SQL Server: the batch is the statement each result is numbered by.
    const batch = mode === 'tsql' ? batches(sql)[0] : null;
    // The comment above counts in a self-numbering driver's start (as some do).
    const results = units.map((u, i) => resultOf(u.text, i, u)).map((r, i) => (
      batch ? { ...r, statement: 0, offset: batch.start, line: batch.line }
        : mode === 'opaque' ? { ...r, statement: null, offset: null, line: null }
        : mode === 'whole' ? { ...r, offset: Math.max(0, sql.lastIndexOf('\n', units[i].start - 2) + 1) }
          : r));
    const o: QueryOutcome = {
      results,
      messages: [],
      error: null,
      elapsed_ms: results.reduce((n, r) => n + (r.elapsed_ms ?? 0), 0),
      plans: [],
      log: units.some((u) => u.text.toLowerCase().startsWith('update'))
        ? [{ level: 'info', text: 'Se marcaron 12 pedidos como revisados', statement: mode === 'opaque' ? null : units.findIndex((u) => u.text.toLowerCase().startsWith('update')), code: null, line: null }]
        : [],
      errors: [],
      transaction: null,
    };
    return o;
  }
  if (cmd === 'plugin:event|listen') return 1;
  return null;
};
