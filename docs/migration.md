# Migrate a database to another engine

**Migrate…**, in a database's menu, opens a tab to move its tables and data
to another database, even on another engine (for example, from SQL Server to
PostgreSQL). The conversion is done by `crates/dbine-schema`; the per-engine
details are in [schema-conversion.md](schema-conversion.md).

To get only the script, without running it, there is **Generate script…**. By
default it uses the database's engine, but another can be chosen (see below).

## Three modes

After choosing the target engine, the mode is chosen. Only the ones that work
for that pair of engines are enabled; the others show why.

- **Migrate (convert):** between any pair of engines. It converts the
  structure to the target engine, creates the tables and copies the data. It
  is what the rest of this page describes.
- **Clone (make the target identical):** between two databases of the same
  engine (today SQL Server and Azure SQL, and PostgreSQL with the derivatives
  that keep its catalog). Instead of the conversion, the driver itself writes
  the script that leaves the target equal to the source: schemas, types,
  tables with their storage and partitions, indexes, constraints, sequences,
  views, routines, triggers and other objects, adapted to what the target
  server supports. The preview shows that script and what could not be left
  identical. The run creates the tables, copies the data with the transfer
  engine (each table with its indexes as soon as it finishes) and at the end
  runs the rest of the script; a statement that depends on another is retried
  in passes and the ones that keep failing are reported.
- **Sync only what changed:** between two databases of the same engine that
  allows it (SQL Server and PostgreSQL). The tables must exist in the target
  with the same columns: nothing is created or emptied. Each table is
  compared by its primary key, or by a unique key chosen in the preview; the
  ones that have neither are listed with the reason. You choose what to
  compare:
  - **Full:** the whole content of each row.
  - **Large columns by size only:** large columns are compared by their
    length; much faster, but it does not see a change that keeps the same
    size.
  - **Keys only:** finds the new and deleted rows, not the modified ones.

  Also the server cores the summary can use (0: the server decides). The
  result shows, per table, the inserted, updated and deleted rows, and the
  notes of each sync (for example, a foreign key that lost trust).

## The Migrate tab

1. **Target:**
   - The target engine. Those that only work as a source (Drill, InfluxDB,
     NetSuite…) appear disabled, with the reason.
   - The **target connection**, required to migrate, and its database.
     Choosing the engine connects nothing: the connection is opened when you
     choose it. A read-only connection is not accepted as a target.
   - **Schemas**, in the engines that have them:
     - By default, each table stays in its source schema and the schemas are
       created in the target.
     - The source's default schema maps to the target's: SQL Server's `dbo`
       goes to PostgreSQL's `public`, and vice versa.
     - If "Single schema" is filled in, all the tables go to that schema. If
       there are repeated names, they are disambiguated with a short suffix
       and the report warns about it.
2. **Tables:** all checked at the start, with a filter.
3. **Options:**
   - copy the data;
   - indexes and foreign keys;
   - adapt the case of names to the target;
   - drop first the tables that exist in the target (`DROP`);
   - `IF EXISTS` / `IF NOT EXISTS`;
   - **advanced options** of the copy: tables at once (8 by default, from 1
     to 32), order (largest first, smallest first or alphabetical) and how
     often to commit (every 100,000 rows by default).
4. **Preview:** the report (Dropped, Loss, Warning, Info), the conversion of
   each column and the structure script. It touches nothing.
5. **Migrate:** asks for confirmation and runs on the target connection, in
   three stages:
   1. **Structure.** The missing schemas; the `DROP`, if requested, in passes
      (a table that others depend on is dropped when those are gone; if it
      still cannot be dropped, the migration stops before creating anything);
      and the `CREATE` of each table **with its columns and primary key**. If
      a table fails, it is reported and the others continue, without copying
      its data.
   2. **Data**, with the bulk transfer engine
      ([bulk-transfer.md](bulk-transfer.md)):
      - several tables at once, the largest first (according to the source's
        catalog in SQL Server, PostgreSQL and MySQL);
      - for each table, the fastest path available: direct copy inside the
        driver when source and target are the same driver and it allows it,
        the target's bulk load, or batched `INSERT` with its driver's load
        settings (`IDENTITY_INSERT`, sequences…);
      - each connection is its own table's, and the source's is read-only;
      - before copying, the target's columns are verified; a table that
        already existed and has rows is not touched;
      - **each table's indexes are created as soon as its copy finishes**,
        while the others keep copying;
      - the target's computed columns and `rowversion` are not loaded.
   3. **Constraints.** The foreign keys of the copied tables and, from SQL
      Server to SQL Server, the identity of each table that has one is left
      at the same value as in the source (`DBCC CHECKIDENT … RESEED`). If
      anything fails, it is reported.

### During and after the run

A grid with one row per table: state, rows copied over the estimated ones
with its bar, rows per second, the path used (direct copy, bulk load or
batched `INSERT`), the bottleneck (source or target) and the error, if any.
Per table you can cancel or, if it is queued, run it right now without
waiting for a slot.

At the top, the total progress, the elapsed time, the tables at once (can be
changed live: lowering it does not cut the ones that are copying), **Cancel
all** and, when finished, **Retry failed ones**.

### Resuming

Each run saves its state: each table's in `dbine-transfer.sqlite` and the
migration's in `migrations/<id>.json`, next to the app's state file. If the
app closes during a migration, when you reopen the **Migrate** tab of that
database it appears under **Interrupted**, with **Resume** and **Discard**:

- an already copied table is never emptied: only its indexes are finished;
- a half-copied table is emptied (`TRUNCATE TABLE`, or `DELETE` in engines
  that lack it) and copied again;
- if it was cut while the tables were being created, the migration starts
  over;
- the foreign keys and the identity that were missing are completed at the
  end.

Tested from SQL Server to PostgreSQL (identity, `bit`, `uniqueidentifier`,
`money`, `datetime2`, unicode, quotes, NULL, FK and index): the counts match
and the sequences end up after the last copied id. From SQLite to SQLite, in
the `src-tauri` tests (copy, indexes, retry of a table that failed and
resumption after a cut).

## Saved migrations

Each database has in the explorer a **Migrations** node, next to **Queries**,
with the migrations that were **started from that database** (only under the
source, never under the target). Each one saves everything needed to reopen
it:

- **Drafts:** the Migrate tab's configuration (target engine, mode, target
  connection and database, chosen tables, options, target schema, advanced
  transfer options, sync depth and keys…) is saved by itself while it is put
  together. The entry appears with the first change (choosing an engine,
  unchecking a table…), not by opening the tab; if it is left half-done,
  nothing is lost.
- **Runs:** when migrating, cloning or syncing (and when resuming or
  retrying) the entry is linked to that run. If it is run again, the new one
  becomes the current one and the earlier ones stay in its history.
- **Name:** automatic (`→ <target connection> · <database> · <mode>`) until
  it is changed; it is renamed from the tab title or with **Rename…**.
- **State**, with its icon: draft, running, finished, with errors,
  interrupted or cancelled (that of its last run).

When opened (click or double click) it opens its own Migrate tab, with the
configuration loaded and editable (changes are saved to the same entry) and
the **Run** panel showing the last run: its tables with state, rows and
errors, and its notes. If that run is still going in the app, the panel
follows it live; if it was interrupted or cancelled, it offers **Resume**,
and if tables failed, **Retry failed ones**. Several migrations of the same
database can be open at once, one tab each.

Menu of a migration: **Open**, **Rename…**, **Duplicate** (the same
configuration as a new draft, without runs) and **Delete**, which only
removes the entry from the list: it touches neither the source nor the target
nor the run records. On the **Migrations** node (and in the database menu,
**Migrate…**) there is **New migration…**.

Saved migrations live in the state file next to the queries, are deleted
with their connection and travel in cloud sync like the queries. The runs,
on the other hand, belong to the machine that ran them: on another machine
the entry shows its configuration and warns that it has no runs on that
machine.

## Generate a script for another engine

In **Generate script…**, "Script engine" starts at the database's engine. If
another is chosen:

- the tables are converted and their DDL is written by that engine's driver;
- the data (if requested) comes out as `INSERT` of that engine;
- views, routines and triggers are not converted; the script says so in a
  comment, along with the losses of the conversion;
- the script is opened in a connection of that engine or saved to a file.

In any engine, the script puts the foreign keys **after** the data, so the
load does not fail because of a parent row that is not there yet.

## What is missing

- Views, procedures and triggers when converting between different engines
  (when cloning, yes).
- In drivers that do not read by columns (the default read does a
  `SELECT *`), a table with columns that the conversion leaves out is not
  copied correctly: the read brings all the columns.

## Commands

Each command receives a single `args`.

| Command | args | Returns |
|---|---|---|
| `migration_targets` | `{ source_connection_id }` (optional) | `[{ id, name, family, supported, reason, clone, sync }]` |
| `migration_plan` | `{ connection_id, database, tables, target_driver, options, mode, sync, target }` | `{ tables, columns, issues, script, available, sync_tables }` |
| `migration_run` | same as `migration_plan`, plus `{ migration_id, target_connection_id, target_database, transfer }` | the run (see below) |
| `migration_set_parallel` | `{ run_id, n }` | the tables at once that resulted |
| `migration_cancel_table` | `{ run_id, table }` | `true` if it found it |
| `migration_run_now` | `{ run_id, table }` | `true` if it was queued |
| `migration_cancel` | `{ run_id }` | — |
| `migration_runs` | `{ limit, ids }` (`ids`: only those runs, no limit) | `[{ id, status, stage, created_at, finished_at, source_connection_id, source_database, target_connection_id, target_database, target_driver, parallel, resumable, tables, foreign_key_errors, after_errors, notes, mode }]` |
| `migration_resume` | `{ run_id }` | the run |
| `migration_retry_failed` | `{ run_id }` | the run |
| `migration_forget` | `{ run_id }` | — |
| `list_saved_migrations` | `{ connection_id, database }` | `[{ id, connection_id, database, name, config, run_ids, created_at, updated_at }]`, newest first |
| `get_saved_migration` | `{ id }` | the saved migration |
| `save_saved_migration` | `{ migration }` (creates or updates; empty `id` = a new one) | the saved migration |
| `rename_saved_migration` | `{ id, name }` | the saved migration |
| `link_saved_migration_run` | `{ id, run_id }` | the saved migration (the run becomes the current one) |
| `duplicate_saved_migration` | `{ id, name }` | the copy, as a draft |
| `delete_saved_migration` | `{ id }` | — |

- `options` is `{ fold_case, target_schema, drop, if_exists, indexes, foreign_keys, data, keep_schemas }`.
- `mode` is `convert` (default), `clone` or `sync`. `clone` and `sync` in
  `migration_targets` say, for the engine of the given source connection,
  `{ available, reason }`.
- `sync` is `{ depth, max_cores, keys }`: `depth` is `Full`, `Sizes` or `Keys`, and
  `keys` a list of `{ schema, name, columns }` with the key chosen per table.
  `sync_tables` brings per table `{ schema, name, primary_key, unique_keys, key, reason }`.
- `target` is `{ connection_id, database }`: the clone preview asks that
  connection what it supports (query only).
- `transfer` is `{ parallel, order, commit_rows }` (`order`: `largest_first`, `smallest_first` or `alphabetical`).
- `config` of a saved migration is the tab's form as is:
  `{ target_driver, mode, target_connection_id, target_database, tables, options, sync, sync_keys, transfer }`
  (`tables`: `null` = all). `run_ids` are its runs, from oldest to newest.
- `tables` is a list of `{ schema, name }`; empty means all the tables.
- The run is `{ run_id, status, tables, foreign_key_errors, after_errors, notes, elapsed_ms, cancelled, mode }`
  (`after_errors`: the clone's final statements that could not be run);
  `status` is `running`, `interrupted`, `done`, `failed` or `cancelled`, and each table is
  `{ name, source, target, status, rows_done, rows_total, path, stats, attempts, error }`.
- Progress arrives in the `migration-progress` event, always with the run's `id`:
  - the engine's events as they are (`run_started`, `table_started`, `table_phase`,
    `table_progress`, `table_done`, `table_failed`, `table_cancelled`, `run_finished`, `log`);
  - `{ event: "plan", tables, parallel }` when the copy starts;
  - `{ event: "step", phase, table, done, total }` in the structure and the constraints
    (`phase`: `schemas`, `drop`, `create`, `foreign_keys`, `identity`, `script`, `before`,
    `check`, `after` or `done`).
