# Schema conversion between engines

`crates/dbine-schema` converts tables from one engine to another: types,
default values, auto-increments, keys, foreign keys, indexes and names. It's
the basis of structure migration between engines. From the interface it's
used with **Migrate…** in a database's menu ([migration.md](migration.md)).

It takes the tables as the source driver reads them (`database_schema`) and
returns the same tables in the target engine's terms, plus a report of
everything that changed. The DDL is then written by the target driver
(`table_ddl`), so the crate doesn't duplicate any engine's syntax.

```rust
let conv = dbine_schema::convert(&tables, "postgres", "sqlserver", &Options::default())?;
// conv.tables:  Vec<TableSchema> in SQL Server types
// conv.columns: source column → target column and type (to copy data)
// conv.issues:  the report
```

## How it converts

1. `parse` splits each native type into its parts: name, arguments,
   `unsigned`, time zone, arrays and wrappers such as `Nullable(...)`.
2. The source dialect classifies it into a neutral logical type
   (`LogicalType`). The logical type keeps what matters so as not to lose
   data: integer size and sign, precision and scale, fractional seconds, time
   zone, whether the text is unicode, the values of an enum.
3. The target dialect writes that logical type with its own names. If the
   value doesn't fit, it uses a bigger type instead of truncating; if
   something is still lost, it says so.
4. Default values are translated: "now", "today's date", "new UUID",
   booleans, sequences. A `nextval(...)` or `IDENTITY` becomes
   auto-increment in the target.
5. Foreign keys, `ON DELETE`/`ON UPDATE` actions, indexes and filtered
   indexes are carried according to what the target accepts.
6. Names are adapted:
   - Oracle's `CLIENTES` becomes `clientes` in PostgreSQL, because there that
     is the unquoted form. A name with mixed upper and lower case is
     respected.
   - If a name exceeds the target's maximum length, it's shortened without
     repeating.
7. The target dialect adds what its engine demands. For example: `ENGINE` and
   `ORDER BY` in ClickHouse, partition key in Cassandra, time index in
   GreptimeDB, distribution in StarRocks.

Between engines of the same family (for example PostgreSQL → CockroachDB)
types, defaults and options are copied as they are; only capability
differences are applied.

## The report

Nothing is lost silently. Each change is recorded with its severity:

| Severity | What it means | Example |
|---|---|---|
| Info | Faithful change worth knowing | `serial` → `IDENTITY`; UUID as `char(36)` |
| Warning | Same values, different behavior | a GIN index ends up with the default type |
| Loss | Values may not fit or lose detail | `datetime2(7)` → `timestamp(6)`; the time zone in MySQL |
| Dropped | The target can't express it | foreign keys in ClickHouse; a filtered unique index |

## What it doesn't do (yet)

- **It doesn't copy data.** `conv.columns` says which source column goes to
  which target column, including those the target adds (time index,
  `ROW_ID`) and those it renames. It's what the data copy will use.
- **Views, procedures and triggers** aren't converted.
- **PostgreSQL enums created with `CREATE TYPE`** arrive as an unknown type
  and the target's DDL fails. They have to be converted by hand.
- **Foreign keys → relationships** in graph engines: they're dropped and
  reported.
- **`STRUCT`, `ROW`, `Tuple`** are converted to JSON, without a note.

## Engines

All DBine drivers have a dialect, except those in the "Not applicable"
table. The `tests/coverage.rs` test fails if a driver is added without a
dialect or a reason.

**Source only.** Conversion to these engines returns an error that explains
why:

| Engine | Reason |
|---|---|
| Apache Drill | Only `CREATE TABLE … AS SELECT`, with no column list. |
| Apache Calcite Avatica | It has no DDL of its own: it depends on the engine behind it. |
| InfluxDB 1, 2 and 3 | Measurements are created when data is written. |
| NetSuite | It's read-only. |
| Archivos CSV / Parquet / JSON | Each file is a read-only view. |

**Not applicable:**

| Engine | Reason |
|---|---|
| Redis, Valkey, Dragonfly | Key-value: there are no tables or columns. |
| etcd | Key tree without a schema. |
| Arrow Flight SQL | Generic protocol: the engine behind it isn't known. |

### Decisions by family

- **MySQL / MariaDB:**
  - `tinyint(1)` is taken as boolean, which is MySQL's convention.
  - `TIMESTAMP` normalizes to UTC and only covers 1970 to 2038, so a
    timestamp with time zone becomes `DATETIME`, with a warning.
  - If the row exceeds 65,535 bytes, the widest `VARCHAR`s become
    `mediumtext`.
- **SQL Server:**
  - `timestamp` is `rowversion`.
  - `tinyint` goes from 0 to 255.
  - A `(max)` column that is part of a key is shrunk, with a warning.
- **Oracle:**
  - `DATE` also stores the time.
  - `AL32UTF8` is assumed: text goes as `VARCHAR2(n CHAR)`.
  - `''` is `NULL`.
  - There is no `ON UPDATE`.
- **SQLite:** types are chosen by affinity. Only a single-column `INTEGER`
  primary key can be auto-increment.
- **ClickHouse:**
  - `ENGINE = MergeTree` and `ORDER BY` come from the primary key.
  - Dates go to `Date32` and `DateTime64`, because `DateTime` only covers
    1970 to 2106.
  - It has no foreign keys.
- **MongoDB:**
  - Each column ends up in a `$jsonSchema` validator, and `NOT NULL` becomes
    `required`.
  - The primary key becomes a unique index. Mongo keeps its own `_id`,
    because renaming the key would break composite keys and references.
- **Cassandra:**
  - The first column of the primary key is the partition key; the others are
    clustering.
  - It has no `NOT NULL` or defaults.
- **Elasticsearch:**
  - Short texts become `keyword` and long ones `text`.
  - Decimals of up to 18 digits become `scaled_float`.
- **Graphs:** each table is a label and each column a property. The key and
  unique indexes become `UNIQUE` constraints.

## What the converter checks

- **Limits:** each type is converted to one that admits all its values. For
  example, MySQL's `INT UNSIGNED` becomes PostgreSQL's `BIGINT`, because an
  `INTEGER` could overflow.
- **Each engine's own types:** JSON, enums and arrays are translated to the
  target's equivalent type, or a warning is given when it doesn't have one.
- **Functions in defaults:** `NOW()`, `GETDATE()` or `NEWID()` are translated
  to the target's equivalent function, or a warning is given when it doesn't
  have one.

## Tests

```sh
cargo test -p dbine-schema          # unit and end-to-end, no servers
```

Tests against real servers are marked `#[ignore]`. They read the same
`DBINE_TEST_<ENGINE>_URL` variables as the drivers' tests. How to run them is
in the header of each file: `live_core.rs`, `analytics.rs`, `enterprise.rs`,
`niche.rs`, `nosql.rs` and `odbc_engines.rs`.

Each live test does the same:

1. Creates in the source a table with all the representative types.
2. Reads it with the source driver and converts it.
3. Creates it in the target with the target driver's DDL and reads it back.
4. Inserts the same row of boundary values on both sides (maximums, decimals,
   dates with time zone, unicode) and compares it.

Where possible, it also does the round trip (A → B → A).
