// Mirrors of crates/dbine-driver/src/schema.rs (snake_case, like serde).
// Tables with their keys, foreign keys and indexes (ER diagram, scripts)
// and what the table designer offers per engine.
import type { Field } from './types';

/** What an engine reports and what the designer edits. */
export interface TableSchema {
  /** Object kind (`table`, `collection`, `index`…). Serde default: `table`. */
  kind: string;
  schema: string | null;
  name: string;
  columns: ColumnDef[];
  primary_key: KeyDef | null;
  foreign_keys: ForeignKeyDef[];
  /** Indexes and unique constraints (not the primary key). */
  indexes: IndexDef[];
  /** `CHECK` constraints (serde default: none). */
  checks?: CheckDef[];
  comment: string | null;
  /** Engine-specific table options, keyed like `DesignerSpec.table_options`. */
  options: Record<string, string>;
  /** What the reader couldn't read and left out, and why (Spanish backend
   *  messages, `tb()`). Omitted when empty. */
  warnings?: string[];
}

export interface ColumnDef {
  name: string;
  /** Type as the engine spells it (`nvarchar(50)`, `jsonb`, `text`…). */
  data_type: string;
  nullable: boolean;
  /** SQL expression / literal as the engine takes it. */
  default_value: string | null;
  auto_increment: boolean;
  comment: string | null;
  /** Engine-specific column options, keyed like `DesignerSpec.column_options`. */
  options: Record<string, string>;
}

export interface KeyDef {
  name: string | null;
  columns: string[];
}

export interface ForeignKeyDef {
  name: string | null;
  columns: string[];
  ref_schema: string | null;
  ref_table: string;
  ref_columns: string[];
  /** `CASCADE`, `SET NULL`… (`null` = the engine's default). */
  on_delete: string | null;
  on_update: string | null;
}

export interface IndexDef {
  name: string;
  columns: string[];
  unique: boolean;
  /** Access method / engine kind (`btree`, `gin`, `CLUSTERED`, `text`…). */
  kind: string | null;
  /** Partial index predicate. */
  filter: string | null;
  /** Non-key columns stored in the index (`INCLUDE`). */
  include?: string[];
  /** Engine settings that make two indexes different (fill factor, a full-text index's catalog…). */
  options?: Record<string, string>;
  /** Set for a vector (similarity search) index. */
  vector?: VectorIndex;
}

/** How a vector index searches (`dbine_driver::VectorIndex`). */
export interface VectorIndex {
  /** The algorithm as the engine names it (`hnsw`, `ivfflat`, `diskann`…); absent: the engine's. */
  algorithm?: string;
  /** The distance: a standard name (`cosine`, `l2`, `ip`, `l1`, `hamming`, `jaccard`) or the engine's own. */
  metric?: string;
  /** Build parameters as the engine names them (`m`, `ef_construction`, `lists`…). */
  params?: Record<string, string>;
}

export interface CheckDef {
  name: string | null;
  /** The condition, without `CHECK`. */
  expression: string;
}

/** Which parts of a table's DDL to produce (`table_ddl`). */
export interface DdlParts {
  drop: boolean;
  if_exists: boolean;
  create: boolean;
  indexes: boolean;
  foreign_keys: boolean;
}

/** What the table designer offers for an engine (`DriverInfo.designer`). */
export interface DesignerSpec {
  /** Kind of object it creates (`table`, `collection`, `index`, `key`…). */
  kind: string;
  /** Menu / title text: "Nueva tabla", "Nueva colección"… */
  label: string;
  /** Suggestions for the type column (free text is allowed). */
  data_types: string[];
  /** Schema selector (engines with schemas). */
  schemas: boolean;
  primary_key: boolean;
  auto_increment: boolean;
  defaults: boolean;
  nullability: boolean;
  comments: boolean;
  indexes: boolean;
  foreign_keys: boolean;
  /** Extra per-column fields (stored in `ColumnDef.options`). */
  column_options: Field[];
  /** Extra per-table fields (stored in `TableSchema.options`). */
  table_options: Field[];
  /** Whether the designer needs columns at all. */
  columns_required: boolean;
  /** The vector indexes it creates (absent / null: none). */
  vector_indexes?: VectorIndexSpec | null;
}

/** What a vector index can be in the designer (`dbine_driver::VectorIndexSpec`). */
export interface VectorIndexSpec {
  /** Algorithms offered (the first is the default); empty: the engine has one. */
  algorithms: string[];
  /** Distances offered, standard names where they fit (the first is the default); empty: not chosen. */
  metrics: string[];
  /** Build parameters with their default (shown as the placeholder). */
  params: { key: string; algorithms: string[]; default: string }[];
  /** The index takes filter columns after the vector column. */
  filter_columns: boolean;
}

/** Starting script for an object without designer; `{schema}`/`{name}` are replaced by the UI. */
export interface CreateTemplate {
  kind: string;
  /** Menu text: "Nueva vista", "Nuevo procedimiento"… */
  label: string;
  template: string;
}

/** Database-level operations a driver offers. */
export interface Capabilities {
  create_database: boolean;
  drop_database: boolean;
  /** Reports foreign keys (relations for the ER diagram). */
  foreign_keys: boolean;
  /** Its sessions report a monitor snapshot (the "Monitor" dashboard). */
  monitor: boolean;
  /** Its sessions report who blocks whom (the Monitor's locks panel). */
  blocking?: boolean;
  /** Its sessions can end another server session. */
  kill_session?: boolean;
  /** Its sessions list the server's processes (the Monitor's "Procesos"). */
  processes?: boolean;
  /** Its sessions stop another session's statement. */
  cancel_query?: boolean;
  /** "Propiedades" on its databases: view and change their settings. */
  database_properties?: boolean;
  /** The server's jobs ("Programadas › En los servidores"). */
  jobs?: boolean;
  /** Its scheduler's objects are created, edited and deleted from DBine (the designer). */
  scheduler?: boolean;
  /** It handles vectors (embeddings): vector columns, their dimension, literals and vector indexes. */
  vectors?: boolean;
}
