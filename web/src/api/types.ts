import { labelMap } from '../composables/i18nLabels';

// Mirrors of the Rust types (crates/dbine-driver/src/{info,config,model}.rs,
// crates/dbine-core/src/state.rs). Serde keeps Rust's snake_case names.

export type Family =
  | 'relational' | 'analytical' | 'document' | 'key_value'
  | 'wide_column' | 'search' | 'time_series' | 'streaming' | 'graph';

export type Language = 'sql' | 'cql' | 'json' | 'redis' | 'flux' | 'cypher';

export type FieldKind =
  | { type: 'text' } | { type: 'number' } | { type: 'password' } | { type: 'bool' }
  | { type: 'file' } | { type: 'textarea' }
  | { type: 'select'; options: [string, string][] };

export interface Field {
  key: string;
  label: string;
  kind: FieldKind;
  required: boolean;
  secret: boolean;
  placeholder: string;
  default: string;
  help: string;
  /** The connection form's tab (absent: general). */
  section?: 'general' | 'ssl' | 'advanced';
  /** Shown (and saved) only when field `key` has one of `values`. */
  when?: { key: string; values: string[] } | null;
}

export interface ObjectKindInfo {
  id: string;
  label: string;
  has_columns: boolean;
  browsable: boolean;
  has_definition: boolean;
}

export interface DriverInfo {
  id: string;
  name: string;
  family: Family;
  language: Language;
  dialect: string;
  default_port: number;
  fields: Field[];
  databases_label: string;
  has_schemas: boolean;
  object_kinds: ObjectKindInfo[];
  /** Help on the query syntax (plain text), empty when there's none. */
  query_help: string;
  /** Offers estimated / actual execution plans. */
  supports_explain: boolean;
  /** "Comparar esquemas" can apply changes to it. */
  supports_schema_sync: boolean;
  /** Users and permissions (docs/usuarios-y-permisos.md); null: not offered. */
  security?: SecuritySpec | null;
  /** The engine's own backups (docs/backups.md); null: only DBine's copies. */
  backup?: BackupSpec | null;
  /** Its sessions implement the profiler ("Profiler" on its databases). */
  supports_profiler: boolean;
  /** Databases of keys, searched on the server a page at a time (Redis, etcd). */
  key_search: KeySearch | null;
  capabilities: import('./schema-types').Capabilities;
  /** The table designer ("Nueva tabla", "Nueva colección"…), if any. */
  designer: import('./schema-types').DesignerSpec | null;
  /** Starting scripts for the other kinds of objects. */
  create_templates: import('./schema-types').CreateTemplate[];
  /** Between objects of a generated script (GO, /…). */
  script_separator: string;
}

export interface ConnectionConfig {
  driver: string;
  host: string;
  port: number;
  database: string;
  username: string | null;
  password: string | null;
  encrypt: boolean;
  trust_server_certificate: boolean;
  read_only: boolean;
  options: Record<string, string>;
}

/** Fields of the form that map to typed config fields (the rest are options). */
export const TYPED_FIELDS = [
  'host', 'port', 'database', 'username', 'password', 'encrypt', 'trust_server_certificate', 'read_only',
] as const;

export interface SavedConnection {
  id: string;
  name: string;
  color: string | null;
  config: ConnectionConfig;
  save_password: boolean;
  /** Explorer folder; null = top level. */
  folder_id: string | null;
  /** Free labels (prod, dev, qa…) shown in the explorer and used to filter it. */
  tags: string[];
  /** MCP access (docs/mcp.md); null/absent = the global default. */
  mcp_level?: 'disabled' | 'schema' | 'read' | 'write' | null;
  updated_at: string;
}

export interface ConnectionFolder {
  id: string;
  name: string;
  parent_id: string | null;
  color: string | null;
}

export interface SavedQuery {
  id: string;
  connection_id: string;
  database: string;
  name: string;
  sql: string;
  updated_at: string;
  last_run_at: string | null;
}

/** How an engine's key search works (`Driver::key_search`). */
export interface KeySearch {
  /** glob: `*`, `?`, `[…]`, and plain text finds keys containing it; prefix: keys starting with the text. */
  syntax: 'glob' | 'prefix';
  /** The explorer nests keys by it (`user:1:cart` → user › user:1). */
  separator: string;
  /** Types it filters by on the server; empty when it has none. */
  types: string[];
  case_sensitive: boolean;
}

export interface KeyScan {
  pattern: string;
  key_type: string | null;
  cursor: string | null;
  count: number;
}

export interface KeyEntry {
  name: string;
  key_type: string | null;
  /** Milliseconds to expiry; null when it doesn't expire. */
  ttl_ms: number | null;
}

export interface KeyPage {
  keys: KeyEntry[];
  /** Where to go on; null when the search is over. */
  cursor: string | null;
  /** Keys in the database (or range); first page only. */
  total: number | null;
  /** Keys the server looked at for this page. */
  scanned: number;
}

export interface DbObject {
  kind: string;
  schema: string | null;
  name: string;
  parent: string | null;
}

export interface ObjectRef {
  kind: string;
  schema: string | null;
  name: string;
}

export interface ColumnInfo {
  name: string;
  data_type: string;
  nullable: boolean;
  primary_key: boolean;
  auto_increment: boolean;
  default_value: string | null;
}

export type Cell = null | boolean | number | string;

export interface ResultColumn {
  name: string;
  type_name: string;
}

export interface StatementResult {
  columns: ResultColumn[];
  rows: Cell[][];
  total_rows: number;
  truncated: boolean;
  rows_affected: number | null;
}

export interface QueryOutcome {
  results: StatementResult[];
  messages: string[];
  error: string | null;
  elapsed_ms: number;
  plans: Plan[];
}

export interface PlanNode {
  op: string;
  detail: string;
  object: string | null;
  total_cost: number | null;
  self_cost: number | null;
  est_rows: number | null;
  actual_rows: number | null;
  executions: number | null;
  actual_ms: number | null;
  warnings: string[];
  props: [string, string][];
  children: PlanNode[];
}

export interface Plan {
  statement: string;
  root: PlanNode;
  actual: boolean;
  raw_format: string;
  raw: string;
}

export interface ConnectResult {
  server_version: string;
  databases: string[];
  default_database: string;
}

export interface TestResult {
  ok: boolean;
  message: string;
}

/** Getters: each read returns the current language (same keys, same order). */
export const FAMILY_LABELS: Record<Family, string> = labelMap({
  relational: 'core:families.relational',
  analytical: 'core:families.analytical',
  document: 'core:families.document',
  key_value: 'core:families.keyValue',
  wide_column: 'core:families.wideColumn',
  search: 'core:families.search',
  time_series: 'core:families.timeSeries',
  streaming: 'core:families.streaming',
  graph: 'core:families.graph',
});

// -- profiler (dbine_driver::profiler) --------------------------------------------------------

export interface ProfilerStarted {
  /** complete: every statement; sampled: what's running at each poll. */
  mode: 'complete' | 'sampled';
  source: string;
  /** Server settings switched on to profile; put back at stop. */
  changes: string[];
  note: string | null;
  /** What `reads` / `writes` count on this engine ("páginas", "filas"…). */
  reads_unit?: string | null;
  writes_unit?: string | null;
}

export interface ProfiledStatement {
  /** `YYYY-MM-DD HH:MM:SS.mmm`, UTC. */
  time: string;
  duration_ms: number | null;
  text: string;
  database: string | null;
  user: string | null;
  /** Host or address of the client. */
  client: string | null;
  /** The client's application name (SQL Server's program name…). */
  application: string | null;
  rows: number | null;
  error: string | null;
  detail: string | null;
  /** CPU time, milliseconds (engines that report it). */
  cpu_ms?: number | null;
  /** Reads / writes in the engine's unit (`ProfilerStarted.reads_unit`). */
  reads?: number | null;
  writes?: number | null;
}

// -- server monitor (dbine_driver::monitor) --------------------------------------------------

export type MetricUnit = 'percent' | 'bytes' | 'count' | 'millis' | 'seconds';

export interface Metric {
  key: string;
  label: string;
  group: string;
  unit: MetricUnit;
  value: number | null;
  max: number | null;
  /** Running total since server start: shown as a rate per second. */
  counter: boolean;
}

export interface MonitorTable {
  key: string;
  title: string;
  columns: string[];
  rows: unknown[][];
}

export interface MonitorSnapshot {
  metrics: Metric[];
  tables: MonitorTable[];
  info: [string, string][];
  notes: string[];
}

/** What the Backups tab offers for an engine's own backups. */
export interface BackupSpec {
  backup_options: Field[];
  restore: boolean;
  restore_options: Field[];
  delete: boolean;
  history: boolean;
  /** A backup covers the server, not one database: offered on the connection. */
  server_wide: boolean;
  /** Where the scripts run ("" = the tab's database). */
  script_database: string;
  note: string;
}

/** What the "Usuarios y permisos" tab offers for an engine. */
export interface SecuritySpec {
  privileges: string[];
  object_kinds: string[];
  create_user: boolean;
  create_role: boolean;
  passwords: boolean;
  membership: boolean;
  per_database: boolean;
}

/** Whether the login may do one action (`Access` in Rust). */
export type Access = { state: 'unknown' } | { state: 'allowed' } | { state: 'denied'; missing: string };

/** What the login may do on the server (`Permissions` in Rust). */
export interface Permissions {
  backup: Access;
  restore: Access;
  profiler: Access;
  kill_session: Access;
  create_database: Access;
  drop_database: Access;
  manage_security: Access;
}
