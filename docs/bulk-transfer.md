# Bulk data transfer (design)

> Status: **implemented**. The transfer engine (`crates/dbine-transfer`), each
> driver's bulk load and direct copy, the faithful cloning of SQL Server and
> PostgreSQL and the row-level sync are done. The Migrate tab uses the engine
> to copy the data ([migration.md](migration.md)); cloning and sync exist in
> the engine and in the drivers, and do not have their own control in that tab
> yet. Which path each engine has, and its limits:
> [engine-support.md](engine-support.md).

Migration works across the 44 engines. It used to copy the data by generating
text `INSERT`s, 500 rows at a time and one table at a time. Now the copy is
done by a transfer engine built to move large volumes: each engine's native
bulk load, tables in parallel, bounded memory, resumption and, between
databases of the same engine, direct copy without decoding the rows.

**Performance goal:** SQL Server → local SQL Server, 3 million mixed rows,
**700,000 rows/s or more** with the app built in release.
**It could not be measured:** the only server available is an emulated `amd64`
image on an `arm64` Mac, and there the limit is the server (direct copy of
200,000 to 270,000 rows/s; reading and loading with decoding, 140,000 to
240,000). The bar is still open for a native `x86` server.

---

## 1. What there was before and what changed

| Before | Problem |
|---|---|
| Each row travels from the driver to the app in its own message (one `write` per row) | Fixed cost per row, the read path does not scale |
| Cells are `serde_json::Value` | Loses information: binaries cut at 1 KiB, `money` as float, decimals and dates as text |
| Written with `insert_script` (SQL text, 500 rows) | The target parses text; no engine uses its fast load path |
| One table at a time | Does not take advantage of the server or the network |
| Unbounded reader→writer channel | If the target is slow, memory grows without limit |
| No per-table state | A cut forces starting from zero |

Errors of the previous copy, fixed in phase 0:

1. **Deadlock** in the built app when source and target use the same driver
   (SQL Server → SQL Server): the row arrives on the driver's reader thread
   and, on that same thread, the driver is asked for the `INSERT`, whose
   answer only that thread can deliver. The same in "Generate script" with
   data.
2. **Binaries larger than 1 KiB** arrive cut and the `INSERT` fails.
3. Between databases of the same engine, **computed columns and `rowversion`**
   are attempted to be written, and the whole table fails.
4. **`money`** loses precision on large values.

---

## 2. Architecture

```
                 app (src-tauri)
   ┌──────────────────────────────────────────────┐
   │ crates/dbine-transfer: generic orchestrator  │
   │  plan · queue · N tables in parallel · state │
   │  resumption · retries · cancellation         │
   └───────────────┬──────────────────────────────┘
          typed batches (bounded rows + bytes)
   ┌───────────────┴───────────────┐
   │ source driver                 │ target driver
   │  read_batches()  ───────────► │  bulk_writer()
   └───────────────────────────────┘
        same driver at both ends:
        copy_native(source, target) inside the driver's process,
        without the rows passing through the app
```

### 2.1 Data model for transferring

A typed batch (`RowBatch`) in `crates/dbine-driver`, independent of the grid's
`serde_json::Value`:

- Lossless cells: null, boolean, 64-bit integers (signed and unsigned),
  floats, exact decimal (128-bit integer + scale), text, full binary, date,
  time, date-time with and without zone, UUID and JSON.
- A batch is closed at **1,000 rows or 2 MiB**, whichever comes first.
- Between the driver and the app, batches travel **as a single message per
  batch**, not one per row.
- Between the reader and the writer of a table there is a **window of 16
  batches** (`CHANNEL_BATCHES`): if the target is slow, the reader waits and
  memory does not grow.

### 2.2 Driver contract (`crates/dbine-driver`)

It enters through the contract, with a default implementation so that it works
on **every** engine:

- `Session::read_batches(table, columns, filter)`: reading in typed batches.
  By default it adapts the usual `execute`.
- `Session::bulk_load(table, columns, options)`: bulk load, with a commit
  every N rows or N bytes (`commit_rows`, `commit_bytes`), table lock
  (`table_lock`) and identity preservation (`keep_identity`) when the engine
  has them. Without it, the migration uses `insert_script`, so no engine is
  left without migration.
- `Driver::supports_bulk_load()` and `Driver::supports_native_copy(engine)`:
  capabilities so the UI shows which path is used.
- `Driver::copy_native(source, target, table)`: copy inside the driver's
  process when both ends are the same driver (or a compatible family). The
  source is only read.
- `Driver::supports_clone()` and `Driver::clone_script(source, target, tables)`:
  the faithful cloning (2.7).
- `Driver::supports_delta()`, `Driver::delta_filter(...)`,
  `Session::key_range`, `Session::delta_summary` and `Session::delta_apply`:
  the row-level sync (2.8).

Each new method carries its variant in the plugin protocol, its forwarding in
`ReadOnlySession` and its capability field with `#[serde(default)]`.

### 2.3 Each engine's fast path

Each engine uses its own load path: `INSERT BULK` in SQL Server, binary `COPY`
in PostgreSQL, `LOAD DATA LOCAL` in MySQL and MariaDB, DuckDB's Appender,
`RowBinary` in ClickHouse, array DML in Oracle, `insertMany`, `_bulk_docs`
and `_bulk` in the document and search engines, load jobs in BigQuery, and so
on. Those without their own path write multi-row or batched `INSERT` with
their driver's settings.

The full table (mechanism, direct copy, cloning, sync) and what each engine
cannot do are in
[engine-support.md](engine-support.md#bulk-transfer-migrating-data).
Cloud loads do not use files in a stage (`PUT` + `COPY`) where the client of
their API does not allow it (Snowflake, Databricks): there the load is
`INSERT`, with the limitation noted.

**Cancelling and cutting:** no load commits anything after returning control.
If it fails or is cancelled, the requests that were already on their way are
awaited before returning; where the engine has no transactions, what was
already committed stays and the error says so.

### 2.4 SQL Server: the own TDS client

The SQL Server driver now uses a copy of tiberius 0.13 inside the repo
(`vendor/tiberius/`), with these changes. Each one is marked in the code with
`PATCH(dbine)` and documented in `vendor/tiberius/PATCHES.md`:

| # | Change | Why |
|---|---|---|
| 1 | Row passing as bytes: reading raw rows and sending them straight to a bulk load, checking that the wire types match | ~58 % less CPU and ~30 % faster than decoding and re-encoding |
| 2 | `INSERT BULK` with hints (`TABLOCK`), exact column list and own metadata `SELECT` | Table lock like bcp; avoids the column error with `IDENTITY_INSERT`; allows declaring `xml`, `text`, `image`, spatial and `hierarchyid` as encodable types |
| 3 | Configurable packet size (32,767) | ~8 times fewer packets and TLS records than with 4,096 |
| 4 | Exact `money` and `smallmoney` in bulk load | Today they cannot be loaded without losing precision |
| 5 | Length byte of `date` in the metadata | It shifted the following columns |
| 6 | Packet length in its own header | Latent corruption when accumulating packets |
| 7 | Scale of `time(n)` and `datetimeoffset(n)` | Scales other than 7 failed |
| 8 | No length check for `(max)` columns | Values over 65,535 bytes failed |
| 9 | Column names in brackets | Names with spaces or reserved words broke the load |
| 10 | `nvarchar(n)` and `nchar(n)` in characters | They were declared with double the length |

Each change was reviewed before porting it against what 0.13 already solves
(it brings `packet_size` and its own bulk load options); what was already
there was not duplicated.

Other connection settings for transferring: `TCP_NODELAY`, dedicated
connections per table (on cancel, the server rolls back the batch in
progress) and reading with a single `SELECT` without `ORDER BY`.

### 2.5 The orchestrator (`crates/dbine-transfer`)

Generic: it knows nothing about SQL Server or any other engine.

- **Tables in parallel:** 8 by default, from 1 to 32, adjustable **live**
  without cutting the ones that are running. The largest first.
- **Memory bounded by bytes, not just by rows:** at most 16 batches of 2 MiB
  in flight per table (~32 MiB), also with wide rows. Drivers that build
  requests on their own (BigQuery, Snowflake, Cosmos DB…) also adjust to that
  cap.
- **Commit every 100,000 rows or 512 MiB.** Batches of 10,000 rows turned
  out ~12 % slower.
- **Indexes:** the table is created with columns and primary key, and the
  rest of the indexes are created when its copy finishes, while other tables
  keep copying. In a table that already existed, non-clustered indexes are
  disabled during the load and rebuilt afterwards, **even if the copy
  fails**.
- **Foreign keys at the end**, and the constraints that were trusted in the
  source stay trusted in the target.
- **Per-table state in SQLite** (pending, copying, copied, with error,
  cancelled), with committed rows.
- **Resumption after a cut**, even a `kill -9`: each table is all or
  nothing. A half-done table is emptied and copied again; a table that is
  **already copied is never emptied**, only its indexes are finished.
- **Retries** only on transient errors: 3, starting at 1 s and doubling up
  to 30 s. "Retry failed ones" button.
- **Cancel** one table or the whole run.
- **Live queue:** tables can be added to a run in progress, or one can be
  run right now outside the limit.
- **Diagnostics:** per table it measures how long the reader waited for the
  target and the writer for the source, and reports which is the bottleneck
  and the rows/s.
- **Progress every 5 s per table** and a run log with grouped writes. The
  notes of the row-level sync (`DeltaResult.notes`, for example a foreign key
  that lost trust) go to that log.
- **Verification before copying:** the target's columns and types against the
  source; if they differ that table is not copied and it says which column
  differs.
- An unexpected error (panic) in a table is left as an error of that table
  and does not take down the run.

### 2.6 Non-negotiable rules

1. **The source is read-only, always.** No DML, DDL, temporary tables, `DBCC`
   or loads in the source. `ReadOnlySession` guarantees it on the app side;
   the direct copy inside the driver has to respect the same.
2. An already copied table is never emptied, not when resuming, not when
   cancelling, not when retrying.
3. At most one process per table, verified **before** emptying it or changing
   its state.
4. A table is emptied only by explicit request ("empty and copy") or when
   resuming a half-done copy.
5. Never load into a different structure: columns and types are verified
   before each copy.
6. Nothing destructive by default: "create and copy" never writes over
   existing rows.
7. A change in the copy, the resumption or the sync is not verified until it
   is run against a real server, and the resumption, with a real process
   cut.

### 2.7 Faithful cloning

Between two databases of the same engine, a "clone" mode that leaves the
target equal to the source. Each driver writes it as a `CloneScript`
(`before`, per table `create` / `after_data`, and `after`), with statements
that can be run again (`IF NOT EXISTS`, `CREATE OR REPLACE`), so a resumed run
executes the whole script again. The source is only read; the target is only
asked what it supports (edition, version, extensions, *filegroups*).

- **SQL Server and Azure SQL:** schemas, *filegroups*, partition functions
  and schemes, XML schema collections, user types, sequences with their
  current value, synonyms; per table, columns, key, storage,
  *memory-optimized* with all its indexes, temporal tables (system
  versioning), *columnstore*, indexes with `INCLUDE`, filter and options,
  statistics, disabled indexes, identity; and at the end views, functions,
  procedures and triggers in dependency order, `CHECK`, foreign keys, system
  versioning and *extended properties*. Fabric and Babelfish do not clone
  (they lack a good part of those objects); for them the generic migration
  (columns and key) is the faithful one.
- **PostgreSQL and derivatives that keep its catalog** (Timescale, Kingbase,
  AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu): schemas, extensions the target
  offers, collations, enum / composite / domain / range types, sequences,
  functions and procedures; per table, columns (identity, generated,
  collations), key, declarative partitioning, `INHERITS`, `UNLOGGED`, access
  method and storage parameters, indexes, constraints; and at the end
  foreign keys, `CHECK` and exclusion, views and materialized views,
  triggers, row-level security, `TOAST`, replica identity, comments and the
  current value of each sequence. It does not name owners, *tablespaces* or
  permissions: the target's objects belong to whoever runs the script.
  YugabyteDB, openGauss, Greenplum and derivatives, CockroachDB, Redshift and
  the rest do not clone (their catalog or their DDL is not PostgreSQL's).

Between different engines, the `dbine-schema` conversion is still used.

### 2.8 Syncing only what changed

To equalize the target right before going to production, without emptying it:

- **Key groups (buckets):** by ranges if the first column of the key is an
  integer (about `ROWS_PER_BUCKET` rows per bucket between the source's
  minimum and maximum; rows that only the target has fall in the edge
  buckets), or by a hash modulo a prime number (17 to 65,537, so that the
  hash does not ignore the first columns).
- **Summary:** each side counts the rows and sums the row hashes per bucket,
  in parallel. The source is only read.
- **Compare:** the buckets that differ are the ones that changed.
- **Apply:** with few differing buckets (and no more than half), only their
  rows are read from the source; otherwise, the whole table is read. In the
  target, the rows go to a work table and **a single transaction** deletes the
  extra ones, updates the ones that differ and inserts the missing ones. An
  **empty** bucket list **means all**: it is applied to the whole table,
  without filtering by bucket. A sync that fails leaves the target as it was
  and is run again; the table is never emptied.
- **Three depths:** the whole content, large columns by length only, or keys
  only. The depth decides which buckets look changed; the apply always
  compares byte by byte.
- **Notes:** what the user has to know about that table (`DeltaResult.notes`,
  in Spanish) goes to the run log.

By engine:

- **SQL Server and Azure SQL:** row hash with `HASHBYTES('MD5')` and buckets
  with `CHECKSUM`. The key must be `NOT NULL` on both sides. It applies with
  a work table cloned from the target, triggers off and a `MERGE` over the
  rows of the buckets that changed. If the key's *collations* differ, all the
  buckets are applied.
- **Foreign key trust rule.** The sync does not copy the source's trust (that
  is schema: clone and compare). It guarantees that a foreign key that the
  target *had trusted* before the apply has it afterwards, or says why not.
  Incoming keys with `ON DELETE` over exactly the key's columns stay active
  during the `MERGE`, so that the cascading delete happens as in the source;
  the rest are disabled (`NOCHECK`) during the `MERGE`, so tables can be
  synced in any order, and the ones that were trusted are checked again after
  the `COMMIT`. One that fails (typical: the other table has not been synced
  yet) is left untrusted, is marked with an extended property and a note in
  the log names the key, the two tables and what to do; each apply on either
  of the two tables checks the marked ones again and clears them when they
  pass. A key that was already untrusted and unmarked is left as is.
- **PostgreSQL and derivatives** (with YugabyteDB, from PostgreSQL 11): row
  hash with `md5`, buckets with `hashtextextended`, large columns by length
  without reading the out-of-line pages, temporary work table loaded with
  binary `COPY` and, in a transaction, delete, update and insert (in
  PostgreSQL 17 and later, a single `MERGE … RETURNING`). User triggers and
  foreign keys are turned off if the role can; otherwise, they fire and the
  log warns.

It is coordinated with "compare data": the bulk sync is the path for large
tables. The reference measurements (20,000 new rows over 3 million in 0.4 s;
~12 times faster comparison of large columns by length) are from SQL Server.

---

## 3. Phases

| Phase | What | Status |
|---|---|---|
| 0 | Typed batch, batches per message between driver and app, bounded channel, fix of the 4 errors | Done |
| 1 | `dbine-transfer`: live parallelism, commits per window, state, resumption, retries, cancellation, queue; run UI | Done |
| 2 | SQL Server: own tiberius with its changes, `INSERT BULK`, large packets, direct copy with raw rows | Done; the 700,000 rows/s bar could not be measured (see above) |
| 3 | Native bulk load in the rest of the engines, and direct copy where the engine allows it | Done; tested against `dbine-test-*` containers where there are any, the rest noted in `engine-support.md` |
| 4 | Faithful cloning of SQL Server and PostgreSQL | Done in the drivers; the control in the Migrate tab is missing |
| 5 | Sync only what changed (SQL Server and PostgreSQL) | Done in the engine and the drivers; the control in the Migrate tab is missing |

Each driver that changes bumps its version and is republished.

## 4. Implementation notes

- **Build profile:** release uses `opt-level = 3` (the copy loop is pure CPU)
  and the whole project's `serde_json` reads decimals with
  `float_roundtrip`, so as not to change the last digit of a `double`.
- **SQLite within the same file:** copying tables in batches between two
  connections to the same file leaves a read open while writing. In the
  default journal mode, that read prevents the load from committing and it
  fails saying so; in WAL mode (`PRAGMA journal_mode = WAL`) it works. The
  direct SQLite copy copies by `rowid` ranges, in windows that do not leave
  the file locked longer than a bounded time; with a filter it also works if
  source and target are the same file. A view or a `WITHOUT ROWID` table is
  copied with a single statement and, if it does not fit in that time, falls
  back to batches. As the ranges are read in separate transactions, rows
  that another process writes to the source in the meantime may or may not be
  copied (batched reading sees a single state).
