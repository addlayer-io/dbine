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
  /** Its group in a long form (a tab of "Nueva base de datos"); '' : the first. */
  group?: string;
}

/** One read-only fact of a database's properties. */
export interface PropertyInfo {
  /** Its tab ('' : General). */
  group: string;
  label: string;
  value: string;
}

/** "Propiedades" of a database: editable fields and their current values,
 *  facts, the server's suggestions and warnings for disruptive changes. */
export interface DatabaseProperties {
  fields: Field[];
  values: Record<string, string>;
  info: PropertyInfo[];
  choices: FieldChoices[];
  /** Field key → what changing it does (kicks sessions, takes it offline…). */
  warnings: Record<string, string>;
}

/** A server's suggestions for a field (its collations, default paths…). */
export interface FieldChoices {
  key: string;
  /** What the server uses when the field is left empty. */
  default: string | null;
  values: string[];
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
  /** Users and permissions (docs/users-and-permissions.md); null: not offered. */
  security?: SecuritySpec | null;
  /** The engine's own backups (docs/backups.md); null: only DBine's copies. */
  backup?: BackupSpec | null;
  /** "Nuevo esquema…" / "Borrar esquema…"; null: not offered. */
  schema_spec?: SchemaSpec | null;
  /** "Nueva base de datos"'s advanced options; empty: just the name. */
  create_database_fields?: Field[];
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
  /** How the editor runs a script (`Driver::script_mode`); absent: 'whole'. */
  script_mode?: ScriptMode;
  /** The engine's own tool defaults for a script (`Driver::script_defaults`). */
  script_defaults?: ScriptDefaults;
  /** The editor offers the Auto/Manual transactions toggle. */
  supports_manual_transactions?: boolean;
  /** A table's indexes with their usage (`Session::index_usage`). */
  supports_index_usage?: boolean;
  /** "Ver dependencias…" (`Session::dependents`). */
  supports_dependencies?: boolean;
  /** "Deshabilitar / Habilitar índice" (`Driver::index_toggle_script`). */
  supports_index_toggle?: boolean;
  /** "Renombrar…" (`Driver::rename_spec`); absent or null: not offered. */
  rename?: RenameSpec | null;
  /** "Asignar login…" in Users and permissions (`Driver::supports_map_login`). */
  supports_map_login?: boolean;
}

/** One index and how it's used (`dbine_driver::IndexUsage`). The last four
 *  fields are derived by the backend (`IndexUsageReport::derive`). */
export interface IndexUsage {
  name: string;
  /** `CLUSTERED`, `NONCLUSTERED`, `CLUSTERED COLUMNSTORE`… */
  kind: string;
  unique: boolean;
  primary_key: boolean;
  /** In key order; descending ones end in ` DESC`. */
  key_columns: string[];
  included_columns: string[];
  filter: string | null;
  size_kb: number | null;
  seeks: number;
  scans: number;
  lookups: number;
  updates: number;
  last_read: string | null;
  last_write: string | null;
  /** The optimizer doesn't use it: disabled, inactive, invisible, ignored or hidden, depending on the engine. */
  disabled?: boolean;
  /** seeks + scans + lookups. */
  reads: number;
  /** This index's reads over the table's (0–1); null when the table has none. */
  read_share: number | null;
  /** Written but never read. */
  unused: boolean;
  /** updates / reads; null without reads. */
  writes_per_read: number | null;
  /** seeks / (seeks + scans), 0–1; null when both are 0. */
  seek_ratio?: number | null;
  /** good ≥ 0.8, warn ≥ 0.5, bad below (columnstore: never bad); null without seeks or scans. */
  seek_health?: 'good' | 'warn' | 'bad' | null;
}

/** A table's indexes, their usage and its foreign keys. */
export interface IndexUsageReport {
  /** When the counters started (server time), when the engine says. */
  since: string | null;
  /** false: the login can't see the counters (all 0; see `note`). */
  stats_available: boolean;
  note: string | null;
  indexes: IndexUsage[];
  foreign_keys: import('./schema-types').ForeignKeyDef[];
  /** false: one "used N times" counter (no seeks/scans split): no seek health. */
  seek_scan_split?: boolean;
  /** false: the engine doesn't count index writes (updates/last write unknown, never "unused"). */
  writes_counted?: boolean;
}

/** `Driver::script_mode`: statement by statement, batch by batch (T-SQL `GO`), or one call. */
export type ScriptMode = 'per_statement' | 'batches' | 'whole';

export interface ScriptDefaults {
  /** Go on after a failed statement (the tab's toggle starts here). */
  continue_on_error: boolean;
  /** Ask before an UPDATE / DELETE without WHERE. */
  confirm_unsafe_dml: boolean;
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

/** Where a secret field takes its value when it isn't kept in the keychain
 *  (src-tauri: dbine_core::secret_sources::SecretSource; docs/secret-sources.md).
 *  Stored as JSON in the `secret_sources` option, by field key. */
export type SecretSource =
  | { kind: 'command'; command: string; json_key?: string }
  | { kind: 'onepassword'; reference: string; account?: string; json_key?: string; fill_username?: boolean; user_field?: string }
  | {
    kind: 'vault'; address: string; path: string; key: string; namespace?: string; kv_version?: '1' | '2'; ca_file?: string;
    auth?: 'approle'; approle_mount?: string; role_id?: string;
  }
  | { kind: 'aws'; secret_id: string; region?: string; profile?: string; json_key?: string; fill_username?: boolean }
  | { kind: 'azure'; vault_url: string; name: string; version?: string; json_key?: string }
  | { kind: 'gcp'; project: string; secret: string; version?: string; json_key?: string }
  | {
    kind: 'keepass'; file: string; entry: string; attribute?: string; key_file?: string;
    master?: 'ask' | 'command' | 'none'; master_command?: string; fill_username?: boolean; user_field?: string;
  }
  | {
    kind: 'zoho'; data_center?: string; secret_name: string; attribute?: string; client_id?: string;
    master?: 'ask' | 'command'; master_command?: string; fill_username?: boolean; user_field?: string;
    /** A Zoho account saved on this computer (`SecretAccount`); then no inline sign-in. */
    account?: string;
  }
  | {
    kind: 'vault_db'; address: string; mount?: string; role: string; static_role?: boolean; namespace?: string; ca_file?: string;
    auth?: 'approle'; approle_mount?: string; role_id?: string;
  }
  | {
    kind: 'conjur'; url: string; conjur_account: string; login: string; variable: string; username_variable?: string; json_key?: string; ca_file?: string;
  }
  | {
    kind: 'ccp'; url: string; app_id: string; safe?: string; folder?: string; object?: string; account_user?: string; account_address?: string;
    client_cert?: string; client_key?: string; ca_file?: string; fill_username?: boolean;
  }
  | {
    kind: 'delinea'; url: string; platform_url?: string; username: string; domain?: string; secret_name: string; attribute?: string;
    master?: 'ask' | 'command'; master_command?: string; fill_username?: boolean; user_field?: string;
  }
  | {
    kind: 'bitwarden'; server?: string; item: string; attribute?: string; json_key?: string; client_id: string;
    master?: 'ask' | 'command'; master_command?: string; fill_username?: boolean; user_field?: string;
  }
  | { kind: 'bitwarden_sm'; server?: string; secret: string; json_key?: string }
  | {
    kind: 'ssm'; parameter: string; version?: string; region?: string; profile?: string; list_index?: string; json_key?: string;
    fill_username?: boolean;
  };
export type SecretSourceKind = SecretSource['kind'];

/** A `secret_source_unlock` error's `unlock`: a master password to ask for
 *  (KeePass, Zoho Vault, Bitwarden set to "ask"). */
export interface SecretSourceUnlock {
  connection_id: string | null;
  connection_name: string;
  field: string;
  /** The field's label (a backend message: `tb()` it). */
  field_label: string;
  kind: 'keepass' | 'zoho' | 'delinea' | 'bitwarden';
  /** The one typed before didn't open it. */
  wrong: boolean;
}

/** A `secret_source_approval` error's `approval`: the definition to confirm. */
export interface SecretSourceApproval {
  connection_id: string | null;
  connection_name: string;
  field: string;
  /** The field's label (a backend message: `tb()` it). */
  field_label: string;
  source: SecretSource;
}

/** "Probar" of a secret field's source: only the value's length. */
export interface SecretSourceTest {
  chars: number;
  /** The secret also had a `username` (AWS, KeePass, Zoho Vault: "fill the user"). */
  username: boolean;
}

/** One thing a source's "Examinar" lists (src-tauri: secret_sources::browse::Entry). */
export interface SecretBrowseEntry {
  /** What goes in the form's input (a group: the `path` to list next). */
  id: string;
  name: string;
  /** Where it is, `/`-separated (absent: at the top). */
  path?: string;
  kind: 'group' | 'item';
  /** Secondary text (an ID, a UUID, a type). */
  detail?: string;
}

/** What `browse_secret_source` returns: names only, never a value. */
export interface SecretBrowseListing {
  entries: SecretBrowseEntry[];
  /** The chosen item's field names (or its JSON keys). */
  fields?: string[];
  /** Everything below the path came at once (the UI walks the groups). */
  complete: boolean;
  truncated: boolean;
  /** The provider searches by text (`query`). */
  search: boolean;
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
  /** A file opened from the OS and not saved yet (src-tauri/src/open_files.rs):
   *  gone at the next start unless "Guardar conexión". */
  temporary?: boolean;
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

/** A schema of a database, listed even when empty (`SchemaInfo` in Rust). */
export interface SchemaInfo {
  name: string;
  /** Built into the engine (sys, INFORMATION_SCHEMA…): hidden while it has no objects. */
  system: boolean;
}

/** What the explorer loads for a database (`DatabaseObjects` in Rust). */
export interface DatabaseObjects {
  objects: DbObject[];
  /** null: the driver doesn't list schemas (they're derived from the objects). */
  schemas: SchemaInfo[] | null;
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

/** One key of the data grid's column sort (`dbine_driver::SortKey`); the
 *  first is the primary one. */
export interface SortKey {
  column: string;
  descending: boolean;
}

/** `filtered_browse_query`: an object's browse query with the grid's
 *  filters and sort, and which of them the server applies. */
export interface FilteredBrowse {
  query: string;
  /** The filters are in `query`; false: `reason` says why the engine can't. */
  server_side: boolean;
  reason: string | null;
  /** The sort is in `query` (also when there's none); false: `sort_reason`
   *  says why the engine can't, and the grid sorts the loaded rows. */
  sort_server_side: boolean;
  sort_reason: string | null;
}

export interface StatementResult {
  columns: ResultColumn[];
  rows: Cell[][];
  total_rows: number;
  truncated: boolean;
  rows_affected: number | null;
  /** Statement of the script that produced it (index into `split_script`'s
   *  units); null when the driver got the whole script at once. */
  statement?: number | null;
  /** Where that statement starts in the `sql` sent (JS string index) and its line (1-based). */
  offset?: number | null;
  line?: number | null;
  /** The engine's completion tag ("INSERT 0 3", "Table created"…). */
  tag?: string | null;
  elapsed_ms?: number | null;
}

export type MessageLevel = 'info' | 'warning' | 'error';

/** A message of a run, in arrival order (`QueryOutcome.log`). */
export interface Message {
  level: MessageLevel;
  text: string;
  statement: number | null;
  code: string | null;
  /** 1-based line of the `sql` sent. */
  line: number | null;
}

/** A failed statement. `offset` is a JS string index into the `sql` sent. */
export interface ScriptError {
  message: string;
  code: string | null;
  sqlstate: string | null;
  statement: number | null;
  offset: number | null;
  line: number | null;
  /** The script stopped here even if it continues on errors. */
  fatal: boolean;
}

export type TxState = 'idle' | 'open' | 'failed';

export interface QueryOutcome {
  results: StatementResult[];
  messages: string[];
  /** The first failure's text (what older screens show). */
  error: string | null;
  elapsed_ms: number;
  plans: Plan[];
  /** Messages and errors in order; empty from drivers built before it (use `messages`/`error`). */
  log?: Message[];
  errors?: ScriptError[];
  /** The tab's transaction after the run (editor runs, drivers that track it). */
  transaction?: TxState | null;
  /** The session's database after the run, when a statement switched it (`USE`). */
  database?: string | null;
}

/** How `execute_query` runs a script: 'whole' (default, every screen but the
 *  editor), 'auto' (the editor: as the driver says), or forced. */
export type RunMode = 'whole' | 'auto' | 'per_statement' | 'batches';

/** An UPDATE/DELETE without WHERE; offsets are JS string indices into the `sql` sent. */
export interface UnsafeDml {
  keyword: 'UPDATE' | 'DELETE';
  start: number;
  end: number;
  line: number;
}

/** `execute_query`'s answer. With `needs_confirmation`, nothing ran: ask,
 *  then run again with `confirmedUnsafe`. */
export interface ExecuteResponse extends QueryOutcome {
  needs_confirmation?: UnsafeDml[];
  /** A cancel closed the tab's session: its open transaction was rolled back. */
  session_closed?: boolean;
}

export type StatementKind = 'sql' | 'block' | 'batch' | 'client_command';

/** A unit of a script (`split_script`); offsets are JS string indices. */
export interface ScriptUnit {
  text: string;
  start: number;
  end: number;
  line: number;
  kind: StatementKind;
  /** `GO 5`: 5. */
  repeat: number;
  /** A client-side error found while splitting (`GO 99999999999`). */
  error?: string;
}

/** Event `query-progress`: a statement of an editor run ended. */
export interface QueryProgress {
  session_id: string;
  statement: number;
  total: number;
  start: number;
  end: number;
  line: number;
  iteration: number;
  repeat: number;
  elapsed_ms: number;
  results: StatementResult[];
  log: Message[];
  errors: ScriptError[];
}

/** Event `query-message`: a message while a statement runs. */
export interface QueryMessage {
  session_id: string;
  message: Message;
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

/** Why a connect failed (dbine_driver::ConnectFailureKind). */
export type ConnectFailureKind = 'firewall' | 'timeout' | 'refused' | 'dns' | 'tls' | 'auth' | 'unknown';

/** "Agregar mi IP al firewall de Azure": the rule that would be created
 *  (src-tauri commands/azure_firewall.rs FirewallPlan). */
export interface AzureFirewallPlan {
  server: string;
  host: string;
  rule_name: string;
  /** ready: az installed and logged in. */
  az: 'ready' | 'missing' | 'logged_out' | 'sandboxed';
}

export interface AzureFirewallRuleAdded {
  server: string;
  resource_group: string;
  subscription: string | null;
  rule_name: string;
  ip: string;
}

/** A "connect" / "auth_failed" error's classification (CommandError's
 *  `detail`, src-tauri/src/error.rs ConnectDetail). */
export interface ConnectDetail {
  kind: ConnectFailureKind;
  /** The server's error number or SQLSTATE ("40615"). */
  code: string | null;
  /** This client's IP as the server saw it (Azure SQL's 40615 says it). */
  client_ip: string | null;
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

/** Which principals can own a schema (`SchemaOwnerKinds` in Rust). */
export type SchemaOwnerKinds = 'both' | 'users' | 'roles';

/** What "Nuevo esquema…" / "Borrar esquema…" offer for an engine (`SchemaSpec` in Rust). */
export interface SchemaSpec {
  /** A schema has an owner, set when creating it. */
  owner: boolean;
  /** Which principals can own a schema (absent: both). */
  owner_kinds?: SchemaOwnerKinds;
  /** Dropping can take its objects with it (CASCADE). */
  cascade: boolean;
  /** Schema-level privileges offered when granting on the new schema. */
  privileges: string[];
  /** Those grants can carry "con opción de otorgar" (absent: true). */
  grant_option?: boolean;
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
  create_schema: Access;
}

/** `check_for_update`'s answer (`UpdateInfo` in Rust). */
export interface UpdateInfo {
  /** The running app's version. */
  current: string;
  /** The latest release's version, without the leading `v`. */
  latest: string;
  /** `latest` is newer than `current`. */
  available: boolean;
  /** The release page (opened with `openReleasePage`). */
  url: string;
  /** The release notes as written (Markdown), already cut short. */
  notes: string;
  published_at: string | null;
  /** This install can download and install `latest` by itself. */
  installable: boolean;
  /** Why it can't: a build without the updater key, macOS not run from an
   *  installed app, a deb/rpm/MSI install, no updater manifest, or the
   *  Flathub build (Flatpak updates it: nothing was checked). */
  reason: 'unsigned' | 'location' | 'package' | 'no_manifest' | 'flatpak' | null;
  /** The update's state in this run (`ready`: downloaded, waiting for the restart). */
  phase: 'idle' | 'downloading' | 'ready';
  /** The window driving the update (its label). */
  owner: string | null;
}

/** `update-progress` (to the update's window only). */
export interface UpdateProgress {
  phase: 'downloading' | 'verifying';
  downloaded: number;
  total: number | null;
}

/** What to look for (`dbine_driver::DependencyTarget`): an object, or one of its columns. */
export interface DependencyTarget {
  object: ObjectRef;
  column?: string | null;
}

export type Relation = 'foreign_key' | 'index' | 'check' | 'code';
export type Confidence = 'confirmed' | 'probable' | 'review';

export interface Mention {
  line: number;
  text: string;
  /** Inside a string: dynamic SQL. */
  dynamic: boolean;
}

/** Something that depends on the target (`dbine_driver::Dependent`). */
export interface Dependent {
  kind: string;
  schema: string | null;
  name: string;
  parent: string | null;
  relation: Relation;
  confidence: Confidence;
  detail: string | null;
  mentions: Mention[];
}

export interface DependencyReport {
  items: Dependent[];
  /** Definitions read. */
  scanned: number;
  /** Objects whose definition couldn't be read. */
  unreadable: string[];
  note: string | null;
}

/** What "Renombrar…" renames (`dbine_driver::RenameTarget`). */
export type RenameTarget =
  | { what: 'object'; object: ObjectRef; parent?: string | null }
  | { what: 'column'; table: ObjectRef; column: string }
  | { what: 'index'; table: ObjectRef; index: string }
  | { what: 'constraint'; table: ObjectRef; constraint: string }
  | { what: 'schema'; database?: string | null; schema: string }
  | { what: 'database'; database: string };

/** What a driver renames and how (`dbine_driver::RenameSpec`). */
export interface RenameSpec {
  kinds: string[];
  columns: boolean;
  indexes: boolean;
  constraints: boolean;
  schemas: boolean;
  /** Dependent kinds the engine updates by itself. */
  tracked: string[];
  replace: 'drop_create' | 'create_or_replace' | 'create_or_alter';
  references: 'sql' | 'pipeline' | 'none';
  fold: 'lower' | 'upper' | 'none';
  transactional: boolean;
  note: string | null;
  /** `replace` for some kinds of dependent only. */
  replace_kinds: Record<string, 'drop_create' | 'create_or_replace' | 'create_or_alter'>;
  /** Kinds that may keep rows of their own (start unselected unless `TO`). */
  holds_rows: string[];
  /** Run after the whole script (`{schema}` = the target's schema). */
  epilogue: string | null;
  /** `tracked` for some targets only (the object's kind, `column`, `index`…). */
  tracked_for?: Record<string, string[]>;
  /** Dependents dropped and created again lose their grants (false: none to lose). */
  grants_on_objects?: boolean;
  /** The app reads an object rename's table (`RenameRequest::table`). */
  wants_table?: boolean;
  /** It renames databases (`RenameTarget::Database`). */
  databases?: boolean;
  /** The database a database rename runs in (null: none). */
  database_from?: string | null;
  /** What the dialog says first about a database rename. */
  database_note?: string | null;
  /** A database rename moves its contents one by one. */
  database_moves?: boolean;
}

/** One object of a database being renamed (`rename::DatabaseObject`). */
export interface DatabaseObject { kind: string; schema: string | null; name: string; definition: string | null }

/** A rename to script (`dbine_driver::RenameRequest`). */
export interface RenameRequest {
  target: RenameTarget;
  new_name: string;
  table?: import('./schema-types').TableSchema | null;
  definition?: string | null;
}

export interface RenameEdit { line: number; before: string; after: string }
export type UnresolvedReason = 'in_string' | 'qualified' | 'other_schema' | 'case' | 'maybe_function' | 'ambiguous_column' | 'alias_named_like_schema';
export interface RenameUnresolved { line: number; text: string; reason: UnresolvedReason }
export type ManualReason = 'dynamic' | 'unreadable' | 'not_rewritten' | 'no_match' | 'shared_routine';

/** What happens to a dependent (`commands::rename::Action`). */
export type RenameAction =
  | { kind: 'engine' }
  | { kind: 'tracked' }
  | { kind: 'manual'; reason: ManualReason; unresolved?: RenameUnresolved[] }
  | {
    kind: 'rewrite'; object: import('./compare').CodeObject; edits: RenameEdit[]; unresolved: RenameUnresolved[];
    schemabound: boolean; default_selected: boolean;
    /** What reads it, dropped with it and created again unchanged. */
    carried: import('./compare').CodeObject[];
  };

export interface RenameImpactItem { dependent: Dependent; action: RenameAction; original: string | null }

/** `rename_impact` (`commands::rename::RenameImpact`). */
export interface RenameImpact {
  items: RenameImpactItem[];
  scanned: number;
  unreadable: string[];
  note: string | null;
  spec_note: string | null;
  /** Another object already has the new name. */
  collides: boolean;
  /** The new name as the script writes it (quoted when needed). */
  quoted_name: string;
  /** The script runs in one transaction. */
  atomic: boolean;
  definition: string | null;
  table: import('./schema-types').TableSchema | null;
  /** A database rename: the other sessions on it, which the rename ends. */
  sessions?: import('./locks').ServerProcess[];
  /** A database rename that moves its contents: back to `rename_script`. */
  objects?: DatabaseObject[];
  /** A database rename: the database the script runs in ('' : none). */
  run_on?: string | null;
}

/** The calling window (`windows::WindowRole`). */
export interface WindowRole {
  label: string;
  /** Restores and saves the tabs and the AI conversation. */
  primary: boolean;
  window_count: number;
}

/** A running background task, as a window reports it (`windows::TaskSummary`). */
export interface TaskSummary {
  id: string;
  title: string;
}

/** A running task and the window it belongs to (`windows::RunningTask`). */
export interface RunningTask {
  label: string;
  id: string;
  title: string;
}

export type StateChangeKind =
  | 'connection' | 'folder' | 'explorer' | 'query' | 'migration'
  | 'setting' | 'library' | 'history' | 'backup' | 'restore' | 'project';

/** What a write to the local state changed (`dbine_core::StateChange`),
 *  the payload of the "state-changed" event every window gets. */
export interface StateChange {
  kind: StateChangeKind;
  id: string | null;
  connection_id: string | null;
  database: string | null;
}

// -- "Ejecutar en varias bases…" (src-tauri/src/commands/multi_db.rs) --------

/** How a database of a multi-database run ended ('skipped': never started, the run was cancelled first). */
export type MultiDbStatus = 'ok' | 'error' | 'cancelled' | 'skipped';

/** One database's outcome (`DbRun`). */
export interface MultiDbRun {
  database: string;
  status: MultiDbStatus;
  error: string | null;
  /** Rows of its result sets (before the max-rows cut). */
  rows: number;
  rows_affected: number | null;
  elapsed_ms: number;
  /** Its result sets, minus the one merged into the shared grid. */
  results: StatementResult[];
  messages: string[];
}

/** What `run_multi_db` returns (`MultiDbResponse`). */
export interface MultiDbResponse {
  /** Nothing ran: the script isn't read-only (first writing keyword; '' when
   *  the engine's language can't be checked). Run again with `confirmedWrite`. */
  needs_confirmation: string | null;
  /** Every database's first result set in one grid, with `base` first. */
  merged: StatementResult | null;
  databases: MultiDbRun[];
  cancelled: boolean;
  elapsed_ms: number;
}

/** `multi-db-progress`: a database ended. */
export interface MultiDbProgress {
  run_id: string;
  database: string;
  status: MultiDbStatus;
  rows: number;
  elapsed_ms: number;
  error: string | null;
  done: number;
  total: number;
}

// -- Proyectos: git repos of scripts linked to this machine --------------------
// (src-tauri/src/commands/projects.rs, projects_git.rs; docs/projects.md)

export interface ProjectTarget { connection_id: string; database: string }
export interface ProjectBinding {
  /** Used when the repo has no environments, or none is active. */
  direct: ProjectTarget | null;
  /** Alias (from .dbine.json) → the user's connection + database. Local only. */
  environments: Record<string, ProjectTarget>;
  /** Alias in use; null = `direct`. */
  active_environment: string | null;
}
export interface Project { id: string; name: string; path: string; binding: ProjectBinding; sort_order: number; created_at: string; updated_at: string }
export interface ProjectEnvironment { name: string; engine: string | null; confirm_run: boolean; description: string }
export interface ProjectManifest { version: number; name: string | null; engine: string | null; environments: ProjectEnvironment[]; default_environment: string | null }
export interface ProjectInfo extends Project { exists: boolean; is_repo: boolean; manifest: ProjectManifest | null; manifest_error: string | null; manifest_warnings: string[] }
/** U = untracked, C = conflict. */
export type ChangeMark = 'M' | 'A' | 'U' | 'D' | 'R' | 'C';
export interface FileChange { path: string; orig_path: string | null; index: string; worktree: string; mark: ChangeMark }
export interface ProjectStatus {
  git: boolean; exists: boolean; is_repo: boolean; branch: string | null; detached: boolean; head: string | null;
  upstream: string | null; remote: string | null; has_remote: boolean; ahead: number; behind: number; changes: FileChange[]; truncated: boolean;
  operation: 'merge' | 'rebase' | 'cherry-pick' | 'revert' | null; last_commit: string | null; fetch_error: string | null; identity_missing: boolean;
}
export interface FsEntry { name: string; path: string; is_dir: boolean; symlink: boolean; size: number; ignored: boolean }
export interface FileContent { path: string; text: string; eol: 'lf' | 'crlf'; bom: boolean; mtime_ms: number; size: number; hash: string }
export interface FileStat { path: string; exists: boolean; mtime_ms: number; size: number; hash: string }
export interface WriteOut { written: boolean; conflict: boolean; stat: FileStat }
export interface FileDiff { path: string; orig_path: string | null; mark: ChangeMark; before: string | null; after: string | null; binary: boolean; too_large: boolean }
export interface ProjectPullOut { up_to_date: boolean; updated: string[]; conflicts: string[]; operation: string | null; note?: string | null }
export interface ProjectSyncOut extends ProjectPullOut { pushed: boolean }
export interface FolderInspect { path: string; exists: boolean; is_repo: boolean; repo_root: string | null; already_linked: string | null; suggested_name: string; has_manifest: boolean; empty: boolean }
export interface GitProgress { op_id: string; phase: string; percent: number | null }
export interface ProjectFilesChanged { project_id: string; paths: string[]; reason: 'write' | 'create' | 'rename' | 'delete' | 'pull' | 'discard' | 'operation' | 'checkout' }

// Feedback (src-tauri/src/commands/feedback.rs): what the optional
// diagnostics of "Enviar comentarios" can say about the machine.
export interface FeedbackSystemInfo { app_version: string; os: string; arch: string }

// Notebooks (src-tauri/src/notebook.rs, docs/notebooks.md): SQL and text
// cells saved as Jupyter .ipynb, with result previews. Targets are names
// only (never ids or credentials).
export interface NotebookTarget {
  /** A connection's NAME. */
  connection: string | null;
  database: string | null;
  /** A project environment's name (.dbine.json). */
  environment: string | null;
}
/** `foreign`: a Python / R… cell from elsewhere, read-only, kept unchanged. */
export type CellKind = 'markdown' | 'sql' | 'foreign';
export interface StoredColumn { name: string; type_name: string }
export interface StoredStatement {
  columns: StoredColumn[];
  rows: Cell[][];
  /** Rows the query returned (before any cap). */
  rows_total: number;
  /** Rows kept in the file. */
  rows_kept: number;
  /** The preview was cut (row cap, byte cap, or the run itself was capped). */
  truncated: boolean;
  rows_affected: number | null;
  statement: string | null;
}
export interface CellResult {
  results: StoredStatement[];
  messages: string[];
  error: string | null;
  /** RFC 3339 UTC. */
  ran_at: string;
  duration_ms: number;
  /** Driver id. */
  engine: string | null;
  /** "Ventas prod › ventas". */
  target_label: string | null;
  /** The stored messages or error were cut to fit the cell's byte cap. */
  truncated?: boolean;
}
export interface NotebookCell {
  /** Stable id (nbformat 4.5 cell id). */
  id: string;
  kind: CellKind;
  source: string;
  /** Foreign: its language; SQL: "sql" or the engine's language. */
  language: string | null;
  /** Overrides the notebook's target. */
  target: NotebookTarget | null;
  /** false = "No guardar el resultado" (the summary stays, the rows don't). */
  keep_result: boolean;
  /** Chart settings and aggregated points, opaque to the backend. */
  chart: unknown | null;
  execution_count: number | null;
  result: CellResult | null;
  /** Foreign cells only: the original nbformat cell, written back unchanged. */
  raw: unknown | null;
}
export interface Notebook {
  target: NotebookTarget | null;
  /** false = "Guardar sin resultados". */
  save_results: boolean;
  /** The file said `save_results` itself (not written; a foreign notebook
   *  in a git project then gets the project's default, without results). */
  save_results_set?: boolean;
  cells: NotebookCell[];
  /** How it was read: "dbine" | "ads" | "jupyter". */
  origin: string;
  /** Notebook-level metadata DBine doesn't own, kept on save. */
  extra_metadata: unknown;
  /** Things ignored on read (Spanish, translated with tb()). */
  warnings: string[];
}
export interface NotebookRead { notebook: Notebook; text_hash: string }

// -- Charts over the whole result (`dbine_driver::aggregate`, chart_aggregate) --

/** A measure's aggregate (the chart's "Agregación" ids). */
export type AggFn = 'count' | 'sum' | 'avg' | 'min' | 'max';
/** `func` over `column`; `column: null` is COUNT(*) (only with 'count').
 *  COUNT(column) counts its non-null cells. */
export interface Measure {
  column: string | null;
  func: AggFn;
}
/** A grouped chart: one category per `x` value, one series per measure
 *  (per measure and `split` value with a split). `x` null: the measures over
 *  the whole result (a KPI), one category (null) and no split. Columns as
 *  the query's result names them. */
export interface AggregateSpec {
  x: string | null;
  split: string | null;
  measures: Measure[];
  /** Most groups (x, or x × split pairs) in the answer (1–10000; default 1000). */
  max_groups?: number;
}
/** What `chart_aggregate` computes. Histograms and box plots always run in DBine. */
export type ChartKind =
  | ({ kind: 'group' } & AggregateSpec)
  /** Equal-width bins (1–200, default 20) between the column's min and max. */
  | { kind: 'histogram'; column: string; bins?: number }
  /** Min, quartiles (linear interpolation), max and Tukey's whiskers per
   *  `by` value (one box with `by` null); at most `max_groups` boxes
   *  (default 50). */
  | { kind: 'box'; column: string; by: string | null; max_groups?: number };
/** One series: measure `measure` (index in `measures`) for split value
 *  `split` (null without a split, or the NULL split value); one value per
 *  category, null where the group has none. */
export interface ChartSeries {
  measure: number;
  split: string | null;
  values: (number | null)[];
}
/** `counts[i]` values in [edges[i], edges[i + 1]) (the last bin closed). */
export interface ChartHistogram {
  edges: number[];
  counts: number[];
  nulls: number;
  non_numeric: number;
}
export interface BoxStats {
  /** The `by` value; null: NULL, or no `by`. */
  category: string | null;
  count: number;
  min: number;
  q1: number;
  median: number;
  q3: number;
  max: number;
  /** Tukey's whiskers: the lowest and highest values within 1.5 IQR of the box. */
  whisker_low: number;
  whisker_high: number;
  /** Values past the whiskers. */
  outlier_count: number;
  /** The most extreme of them (at most 100), ascending. */
  outliers: number[];
}
/** A chart's data over the whole result of its query. Category and split
 *  labels are the cells as `String(v)` gives them; null is NULL (show the
 *  chart's own "(nulo)" label). */
export interface ChartAggregate {
  categories: (string | null)[];
  series: ChartSeries[];
  boxes: BoxStats[] | null;
  histogram: ChartHistogram | null;
  /** 'server': the engine grouped the rows; 'client': DBine read them. */
  method: 'server' | 'client';
  rows_scanned: number | null;
  /** The client path stopped at 1,000,000 rows: the chart covers those. */
  rows_capped: boolean;
  /** More groups than `max_groups`: the last ones were left out. */
  groups_truncated: boolean;
}


/** A secret manager's account saved on this computer (dbine_core::secret_accounts):
 *  never exported nor synced; its secrets are in the keychain. */
export interface SecretAccount {
  id: string;
  kind: 'zoho';
  label: string;
  /** The account's e-mail or user name ('' when the manager didn't say). */
  identity: string;
  data_center: string;
  client_id: string;
  /** Where the master passphrase comes from; absent: the keychain. */
  master?: 'ask' | 'command';
  master_command?: string;
  created_at: string;
  updated_at: string;
}

/** "Conectar Zoho Vault", step 3: the code exchanged (src-tauri: accounts::ZohoExchanged). */
export interface ZohoExchanged {
  pending: string;
  data_center: string;
  identity: string;
}
