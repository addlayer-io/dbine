// Development only: sample drivers for the connection dialog preview
// (view=connection) and a moving server for the monitor (view=monitor).
import type { DriverInfo, Family, Field, MonitorSnapshot, ProfiledStatement, ProfilerStarted } from '../api/types';

const f = (key: string, label: string, type: string, extra: Partial<Field> = {}): Field => ({
  key, label, kind: { type } as Field['kind'], required: false, secret: false, placeholder: '', default: '', help: '', ...extra,
});

const SERVER_FIELDS: Field[] = [
  f('host', 'Servidor', 'text', { required: true, placeholder: 'localhost' }),
  f('port', 'Puerto', 'text'),
  f('database', 'Base de datos', 'text', { placeholder: 'postgres' }),
  f('username', 'Usuario', 'text', { required: true }),
  f('password', 'Contraseña', 'password', { secret: true }),
  f('sslmode', 'SSL', 'select', { default: 'prefer' }),
  f('encrypt', 'Cifrar la conexión (TLS)', 'bool'),
  f('read_only', 'Solo lectura', 'bool'),
];
(SERVER_FIELDS[5].kind as unknown as { options: [string, string][] }).options = [
  ['disable', 'Desactivado'], ['prefer', 'Preferir'], ['require', 'Requerido'],
];

const ENGINES: [string, string, Family, number][] = [
  ['postgres', 'PostgreSQL', 'relational', 5432], ['mysql', 'MySQL', 'relational', 3306], ['mariadb', 'MariaDB', 'relational', 3306],
  ['sqlserver', 'SQL Server', 'relational', 1433], ['azuresql', 'Azure SQL Database', 'relational', 1433],
  ['oracle', 'Oracle', 'relational', 1521], ['sqlite', 'SQLite', 'relational', 0], ['firebird', 'Firebird', 'relational', 3050],
  ['cockroachdb', 'CockroachDB', 'relational', 26257], ['yugabytedb', 'YugabyteDB', 'relational', 5433],
  ['timescaledb', 'TimescaleDB', 'relational', 5432], ['tidb', 'TiDB', 'relational', 4000], ['db2', 'IBM Db2 (LUW)', 'relational', 50000],
  ['informix', 'IBM Informix', 'relational', 9088], ['sybase', 'SAP ASE (Sybase)', 'relational', 5000], ['cubrid', 'CUBRID', 'relational', 33000],
  ['dameng', 'Dameng (DM)', 'relational', 5236], ['altibase', 'Altibase', 'relational', 20300],
  ['clickhouse', 'ClickHouse', 'analytical', 8123], ['duckdb', 'DuckDB', 'analytical', 0], ['snowflake', 'Snowflake', 'analytical', 443],
  ['bigquery', 'Google BigQuery', 'analytical', 0], ['redshift', 'Amazon Redshift', 'analytical', 5439], ['databricks', 'Databricks SQL', 'analytical', 443],
  ['trino', 'Trino', 'analytical', 8080], ['hana', 'SAP HANA', 'analytical', 30015], ['teradata', 'Teradata', 'analytical', 1025],
  ['vertica', 'Vertica', 'analytical', 5433], ['exasol', 'Exasol', 'analytical', 8563], ['athena', 'Amazon Athena', 'analytical', 0],
  ['mongodb', 'MongoDB', 'document', 27017], ['couchdb', 'CouchDB', 'document', 5984], ['cosmosdb', 'Azure Cosmos DB', 'document', 443],
  ['redis', 'Redis', 'key_value', 6379], ['dynamodb', 'Amazon DynamoDB', 'key_value', 0],
  ['cassandra', 'Apache Cassandra', 'wide_column', 9042], ['scylladb', 'ScyllaDB', 'wide_column', 9042],
  ['elasticsearch', 'Elasticsearch', 'search', 9200], ['opensearch', 'OpenSearch', 'search', 9200], ['solr', 'Apache Solr', 'search', 8983],
  ['influxdb', 'InfluxDB 2 (Flux)', 'time_series', 8086], ['iotdb', 'Apache IoTDB', 'time_series', 6667],
  ['ksqldb', 'ksqlDB', 'streaming', 8088], ['neo4j', 'Neo4j', 'graph', 7687],
];

export const sampleDrivers: DriverInfo[] = ENGINES.map(([id, name, family, port]) => ({
  id, name, family, language: 'sql', dialect: 'postgres', default_port: port, fields: SERVER_FIELDS,
  databases_label: 'Bases de datos', has_schemas: true, object_kinds: [], query_help: '', supports_explain: true, supports_profiler: true, key_search: null,
    supports_schema_sync: true,
  capabilities: { create_database: true, drop_database: true, foreign_keys: true, monitor: true },
  designer: null, create_templates: [], script_separator: '',
}));

// -- monitor ------------------------------------------------------------------------------

let tick = 0;
let queries = 1_830_000;
let commits = 402_000;
let readBytes = 8.1e9;

export function sampleSnapshot(): MonitorSnapshot {
  tick++;
  const wave = (a: number, p: number) => a * (0.5 + 0.5 * Math.sin(tick / p));
  queries += 900 + Math.round(wave(1400, 3));
  commits += 180 + Math.round(wave(260, 4));
  readBytes += 2.2e6 + wave(9e6, 5);
  const cpu = 18 + wave(62, 6) + Math.random() * 6;
  return {
    info: [
      ['Versión', 'PostgreSQL 16.4 on aarch64-unknown-linux-gnu'], ['Servidor', 'db-prod-01 (10.0.4.12)'],
      ['Rol', 'Primario'], ['Zona horaria', 'America/Argentina/Buenos_Aires'],
      ['max_connections', '200'], ['shared_buffers', '4 GB'],
    ],
    metrics: [
      { key: 'cpu', label: 'CPU del servidor', group: 'CPU', unit: 'percent', value: cpu, max: null, counter: false },
      { key: 'mem_used', label: 'Memoria usada', group: 'Memoria', unit: 'bytes', value: 11.2e9 + wave(2e9, 9), max: 16 * 2 ** 30, counter: false },
      { key: 'mem_cache', label: 'Shared buffers', group: 'Memoria', unit: 'bytes', value: 4 * 2 ** 30, max: null, counter: false },
      { key: 'connections', label: 'Conexiones', group: 'Conexiones', unit: 'count', value: 120 + Math.round(wave(62, 5)), max: 200, counter: false },
      { key: 'active_sessions', label: 'Sesiones activas', group: 'Conexiones', unit: 'count', value: 3 + Math.round(wave(14, 3)), max: null, counter: false },
      { key: 'queries', label: 'Consultas', group: 'Actividad', unit: 'count', value: queries, max: null, counter: true },
      { key: 'transactions', label: 'Transacciones', group: 'Actividad', unit: 'count', value: commits, max: null, counter: true },
      { key: 'disk_read', label: 'Lectura en disco', group: 'Disco', unit: 'bytes', value: readBytes, max: null, counter: true },
      { key: 'cache_hit', label: 'Aciertos de caché', group: 'Caché', unit: 'percent', value: 99.1 - wave(2, 7), max: null, counter: false },
      { key: 'storage_used', label: 'Espacio usado', group: 'Almacenamiento', unit: 'bytes', value: 182.4e9, max: null, counter: false },
      { key: 'locks_waiting', label: 'Bloqueos en espera', group: 'Bloqueos', unit: 'count', value: Math.round(wave(3, 2)), max: null, counter: false },
      { key: 'replication_lag', label: 'Retraso de réplica', group: 'Replicación', unit: 'seconds', value: 0.2 + wave(1.4, 4), max: null, counter: false },
      { key: 'uptime', label: 'Tiempo activo', group: 'Servidor', unit: 'seconds', value: 1_234_567 + tick * 5, max: null, counter: false },
    ],
    tables: [
      {
        key: 'sessions', title: 'Sesiones', columns: ['PID', 'Usuario', 'Base', 'Cliente', 'Estado', 'Duración', 'Consulta'],
        rows: Array.from({ length: 14 }, (_, i) => [
          4100 + i, ['app', 'etl', 'reporting', 'postgres'][i % 4], 'ventas', `10.0.4.${20 + i}`,
          i % 3 ? 'idle' : 'active', `${(i * 1.7).toFixed(1)} s`,
          i % 3 ? '' : 'SELECT c.id, c.nombre, sum(p.total) FROM clientes c JOIN pedidos p ON p.cliente_id = c.id GROUP BY 1, 2',
        ]),
      },
      { key: 'locks', title: 'Bloqueos', columns: ['PID', 'Tipo', 'Objeto', 'Modo', 'Concedido'], rows: [[4102, 'relation', 'pedidos', 'RowExclusiveLock', true]] },
      {
        key: 'databases', title: 'Bases y tamaños', columns: ['Base', 'Tamaño', 'Conexiones', 'Commits', 'Rollbacks'],
        rows: [['ventas', '142 GB', 98, 381_220, 412], ['logística', '38 GB', 21, 20_411, 12], ['postgres', '8 MB', 1, 90, 0]],
      },
    ],
    notes: ['PostgreSQL no expone el uso de CPU por SQL: el valor viene de la extensión pg_proctab.'],
  };
}

// -- profiler (view=profiler[&mode=sampled]) ---------------------------------------------------

export function sampleProfilerStart(mode: 'complete' | 'sampled'): ProfilerStarted {
  return mode === 'sampled'
    ? { mode, source: 'pg_stat_activity', changes: [], note: null }
    : {
      mode, source: 'Extended Events',
      changes: ['sesión de Extended Events dbine_profiler_7f3a (sql_batch_completed, rpc_completed)'],
      note: null,
    };
}

const PROFILED: Omit<ProfiledStatement, 'time'>[] = [
  { duration_ms: 3.2, text: 'SELECT id, nombre, email FROM clientes WHERE id = @p1', database: 'ventas', user: 'app', client: '10.0.4.12', application: 'api-ventas', rows: 1, error: null, detail: 'lecturas 3 · CPU 0 ms' },
  { duration_ms: 1840, text: 'SELECT c.region, SUM(p.total) AS total\nFROM pedidos p\nJOIN clientes c ON c.id = p.cliente_id\nWHERE p.fecha >= DATEADD(month, -3, GETDATE())\nGROUP BY c.region\nORDER BY total DESC', database: 'ventas', user: 'reportes', client: '10.0.9.40', application: 'Power BI', rows: 12, error: null, detail: 'lecturas 184.220 · CPU 1.610 ms' },
  { duration_ms: 0.8, text: 'UPDATE sesiones SET ultimo_acceso = SYSUTCDATETIME() WHERE token = @p1', database: 'ventas', user: 'app', client: '10.0.4.12', application: 'api-ventas', rows: 1, error: null, detail: null },
  { duration_ms: 12, text: "INSERT INTO pedidos (cliente_id, fecha, total) VALUES (1042, SYSUTCDATETIME(), 199.90)", database: 'ventas', user: 'app', client: '10.0.4.13', application: 'api-ventas', rows: 1, error: null, detail: null },
  { duration_ms: 4, text: 'SELECT * FROM producto WHERE sku = @p1', database: 'ventas', user: 'app', client: '10.0.4.12', application: 'api-ventas', rows: null, error: "Invalid object name 'producto'.", detail: null },
  { duration_ms: 6.5, text: 'EXEC dbo.sp_stock_disponible @sku = N\'A-1001\'', database: 'ventas', user: 'app', client: '10.0.4.20', application: 'api-stock', rows: 1, error: null, detail: null },
];

/** Each poll brings one to three statements, a few seconds apart. */
export function sampleProfilerPoll(): ProfiledStatement[] {
  const n = 1 + Math.floor(Math.random() * 3);
  return Array.from({ length: n }, (_, i) => {
    const t = new Date(Date.now() - (n - i) * 180);
    const base = PROFILED[Math.floor(Math.random() * PROFILED.length)];
    // Same statements with other values, and durations that vary a bit.
    const id = 1000 + Math.floor(Math.random() * 90);
    const text = base.text.replace('1042', String(id)).replace("N'A-1001'", `N'A-${id}'`);
    const duration_ms = base.duration_ms === null ? null : Math.round(base.duration_ms * (0.6 + Math.random() * 0.8) * 10) / 10;
    return { ...base, text, duration_ms, time: t.toISOString().replace('T', ' ').slice(0, 23) };
  });
}
