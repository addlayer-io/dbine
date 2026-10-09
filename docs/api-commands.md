# Tauri commands: design, scripts, import and databases

Contract between the backend (`src-tauri/src/commands/`) and the UI. All
commands take a single `args` object with snake_case fields, like the rest of
the API (`web/src/api/client.ts`). The Rust types are in
`crates/dbine-driver/src/schema.rs`.

## Driver data (`list_drivers`)

Each driver includes, besides what it already reported:

- `capabilities`: `{ create_database, drop_database, foreign_keys, monitor }`
- `designer`: `DesignerSpec | null`. What the designer creates, its data types
  and its own options.
- `create_templates`: `[{ kind, label, template }]`. In the text, `{schema}` and
  `{name}` are replaced by the UI.
- `script_separator`: text that goes between objects in a script (`GO`, `/` or `""`).

## Structure and DDL

| Command | args | Returns |
|---|---|---|
| `database_schema` | `{ connection_id, database }` | `TableSchema[]` |
| `table_ddl` | `{ connection_id, table: TableSchema, parts: DdlParts }` | `string` |
| `insert_script` | `{ connection_id, target: ObjectRef, columns: string[], rows: Cell[][] }` | `string` |

| `update_script` | `{ connection_id, target: ObjectRef, changes: [{ key: [[col, value]], set: [[col, value]] }] }` | `string` |

`DdlParts` is `{ drop, if_exists, create, indexes, foreign_keys }`, all booleans.

`update_script` turns the cells edited in the grid into engine code:
`UPDATE … WHERE <key>` in SQL, `updateOne` in MongoDB, etc. How it works:

- `key` identifies the row: its primary key or, if the table has none, all the
  columns with their original value.
- `set` carries the new values.
- DBine only generates the code: it appends it to the query or opens it in a
  new one, and the user decides whether to run it.
- An engine that can't update rows returns `bad_request` with the reason.

## Table data with filters (`filtered_browse_query`)

| Command | args | Returns |
|---|---|---|
| `filtered_browse_query` | `{ connection_id, database, object: ObjectRef, limit, filters: ColumnFilter[] }` | `{ query, server_side, reason }` |

It is the filter row that appears under the grid headers, in a table's data
view. `ColumnFilter` is `{ column, op, values, sql }`:

- `op` is one of these:
  - comparisons: `eq`, `ne`, `gt`, `ge`, `lt`, `le`;
  - text: `contains`, `not_contains`, `starts_with`, `ends_with`;
  - nulls and empties: `is_null`, `not_null`, `is_empty`, `not_empty`;
  - lists: `in`, `not_in`;
  - booleans: `is_true`, `is_false`, `true_or_null`, `false_or_null`;
  - hand-written SQL: `sql` (the whole condition) and `sql_right` (what follows
    the column).
- `values` carries the values with their type: numbers travel as numbers.
- Filters are combined with AND.

The driver adds the filters to its own browse query
(`Driver::filtered_browse`): a `WHERE` in SQL and CQL, the filter document in
Mongo, etc. If it can't, it returns `server_side: false` together with the
unfiltered query and the reason. In that case the UI filters the rows already
loaded and says so in the filter bar.

What is typed in each column's filter box:

| Typed | Means |
|---|---|
| `text` | contains (text); equals (numbers and booleans) |
| `=x`, `<>x`, `!=x`, `>x`, `>=x`, `<x`, `<=x` | comparison |
| `1,2,3` or `=a,b` | any of those values |
| `!x`, `^x`, `x$` | does not contain, starts with, ends with |
| `NULL`, `NOT NULL`, `EMPTY`, `NOT EMPTY` | nulls and empties |
| `2024-05-01` on a date | that whole day |
| `true` / `false`, `1` / `0`, `sí` / `no` on a boolean | true or false |

Each column's ⋮ menu offers the same operations according to its type, plus
"Filter multiple values…" and the two SQL conditions.

## Import connections from other tools

| Command | args | Returns |
|---|---|---|
| `import_connections_detect` | — | `{ [source]: path \| null }`: where each tool is on this machine |
| `import_connections_scan` | `{ source, path?, text? }` | `{ path, items, warnings }` |
| `import_connections_apply` | `{ source, path?, text?, keys, passwords }` | `{ imported, folders, failed: [name, reason][] }` |

`source` is one of `dbeaver`, `dbgate`, `datagrip`, `azure_data_studio`,
`ssms` or `url`. With `url`, what is read is `text`, with one connection per
line. The paths below are the macOS ones; on Windows they are under `%APPDATA%`
and on Linux under `~/.config`.

| Tool | What is read | Passwords |
|---|---|---|
| DBeaver | The `.dbeaver/data-sources*.json` of each project in the workspace (`~/Library/DBeaverData/workspace6`). Projects other than `General` become a folder. | Yes: `credentials-config.json`, encrypted with DBeaver's fixed key |
| DbGate | `~/.dbgate/connections.jsonl`, without the unsaved connections | Yes: `crypt:…`, with the key from `~/.dbgate/.key` |
| DataGrip / JetBrains | The global ones (`JetBrains/<IDE><version>/options/dataSources.xml`) and those of each IDE's recent projects (`.idea/dataSources.xml`), without duplicates. The user comes from `dataSources.local.xml`. | Yes, from the system keychain (`IntelliJ Platform DB — <uuid>`), only on import; the system may ask for permission |
| Azure Data Studio | `azuredatastudio/User/settings.json` (JSON with comments): `datasource.connections` and the groups as folders | No: they stay in its own store and are asked for on connect |
| SSMS | The registered servers (`Microsoft/SQL Server Management Studio/<version>/RegSrvr.xml`) or an exported `.regsrvr`, with their groups and colors | No: they are protected by the Windows account |
| URL | `postgres://…`, `mysql://…`, `mongodb+srv://…`, `redis(s)://…`, `sqlserver://…`, `jdbc:…`, `Server=…;Database=…` or the path of a SQLite or DuckDB file | The ones the URL carries |

How it imports:

- **Passwords:** they never reach the UI. `scan` only reports:
  - `has_secret`, whether the connection carries one;
  - `keychain`, whether it is in the other tool's keychain.

  `apply` rereads the source and passes them straight to DBine's keychain.
  With `passwords: false`, or if there are none, they are asked for on connect.
- **Folders:** existing ones with the same name are reused.
- **Duplicate connections:** `existing` warns if there is already one with the
  same engine, host, port, database and user. The UI doesn't mark them by
  default.
- **What isn't imported:**
  - the SSH tunnel;
  - Windows integrated authentication, IAM or Entra ID with MFA;
  - engines without a driver in DBine or configured with cloud credentials:
    BigQuery, Spanner, Athena, Databricks, DynamoDB, Cosmos DB and Azure Data
    Explorer.

  What is imported without any of that data is reported in `notes`; what can't
  be imported comes back in `unsupported`.
- **First start:** if any tool has connections to import, the app offers it
  once. The answer is stored in the preference `import.suggested`, which
  travels with sync.

## Compare schemas

How it works from the UI: `docs/schema-compare.md`.

| Command | args | Returns |
|---|---|---|
| `schema_compare_load` | `{ connection_id, database, schemas? }` | `{ driver, tables: TableSchema[], objects: CodeObject[], warnings }` |
| `schema_compare` | `{ left: DbModel, right: DbModel, options: { ignore_case, ignore_schema, ignore_comments } }` | `{ tables: TableDiff[], objects: ObjectDiff[] }` |
| `schema_compare_convert` | `{ from_driver, to_driver, tables, target_schema }` | `{ tables, warnings }` |
| `schema_sync_script` | `{ connection_id, tables: TableChange[], objects: ObjectChange[], views: CodeObject[] }` | `{ statements, warnings }` |
| `schema_sync_run` | `{ connection_id, database, statements, run_id, atomic? }` | `{ done, failed: [index, error] \| null, rolled_back }` |

The types:

- `CodeObject` is `{ kind, schema, name, definition }`: views, materialized
  views, procedures, functions and triggers, with their code.
- `schema_compare` is pure, with no connection. The UI calls it again on its
  copies after every change passed from one side to the other.
- `TableDiff` has:
  - `key`;
  - `left` and `right`, indexes into each model or `null`;
  - `status`: `equal`, `changed`, `only_left` or `only_right`;
  - `columns`, `indexes` and `foreign_keys`, as `ItemDiff[]`;
  - `primary_key`, a status;
  - `fields`: the table properties that differ.
- `ItemDiff` has `name`, `left`, `right`, `status` and `fields`. `fields` lists
  what differs: `type`, `nullable`, `default`, `auto_increment`, `comment`,
  `columns`, `unique`, `kind`, `filter`, `on_delete`, `on_update`.
- `TableChange` is one of:
  - `{ op: "create", table }`;
  - `{ op: "drop", table }`;
  - `{ op: "alter", old, new }`.
- `ObjectChange` is `{ op: "create" | "drop" | "replace", object }`.
- `views` are the target's views as they will end up. Those that use tables
  whose columns change type or are dropped are added to the script
  automatically: they are dropped before and recreated after.

The script is built by the target's driver (`Driver::sync_script`). SQL engines
use the common planner `dbine_driver::alter::sync_script` with their
`AlterStyle`. Engines that can't apply changes have `supports_schema_sync: false`
in `list_drivers`, and the UI disables "Sync" with the reason.

`schema_sync_run`:

- Rejects read-only connections.
- Uses its own session (`sync:<run_id>`), which `cancel_query` can cut.
- Runs each statement separately and stops at the first one that fails.
- With `atomic: true`, if the driver has manual transactions and its
  `RenameSpec` says `transactional`, it runs everything in one transaction: it
  commits at the end and rolls back on an error or a cancellation
  (`rolled_back: true`). In all other cases `atomic` changes nothing.

## Rename with impact

How it works from the UI: [`rename.md`](rename.md). What each driver offers
arrives in `list_drivers` as `rename` (a `RenameSpec` or `null`).

| Command | args | Returns |
|---|---|---|
| `rename_impact` | `{ connection_id, database, target: RenameTarget, new_name, keep_view_columns? }` | `RenameImpact` |
| `rename_script` | `{ connection_id, database, request: RenameRequest, rewrites: { object: CodeObject, schemabound }[] }` | `{ statements, warnings }` |

The script runs with `schema_sync_run` and `atomic: true`.

- `RenameTarget` is one of `{ what: "object", object, parent? }`,
  `{ what: "column", table, column }`, `{ what: "index", table, index }`,
  `{ what: "constraint", table, constraint }` or
  `{ what: "schema", database?, schema }`.
- `RenameRequest` is `{ target, new_name, table?, definition? }`; `table` and
  `definition` come from `RenameImpact`.
- `RenameImpact` has `items` (`{ dependent, action, original }`),
  `scanned`, `unreadable`, `note`, `spec_note`, `collides`, `quoted_name`,
  `atomic`, `definition` and `table`.
- `action` is one of:
  - `{ kind: "engine" }`: a key, an index or a check;
  - `{ kind: "tracked" }`: an object the engine follows by itself;
  - `{ kind: "manual", reason, unresolved? }`, with `reason` `dynamic`,
    `unreadable`, `not_rewritten` or `no_match`;
  - `{ kind: "rewrite", object, edits, unresolved, schemabound, default_selected }`.
- `edits` are `{ line, before, after }`; `unresolved` are
  `{ line, text, reason }`, with `reason` `in_string`, `qualified`,
  `other_schema`, `case`, `maybe_function`, `ambiguous_column` or
  `alias_named_like_schema`.
- `rename_impact` rejects an empty name or one equal to the current one, and
  the engines or objects the driver doesn't rename.
- `rename_script` is pure: it doesn't touch the server. It puts the driver's
  rename in the middle, with the dependents that get dropped (`drop_create` and
  the SCHEMABINDING ones) before and the rest after, ordered so that each one
  is created after what it uses.

## Databases

| Command | args | Returns |
|---|---|---|
| `create_database` | `{ connection_id, name, options? }` | `void` |
| `create_database_script` | `{ connection_id, name, options? }` | `string` |
| `create_database_choices` | `{ connection_id }` | `FieldChoices[]` |
| `drop_database` | `{ connection_id, name }` | `void` |
| `drop_objects` | `{ connection_id, database, objects: ObjectRef[] }` | `{ dropped: ObjectRef[], errors: [ObjectRef, string][] }` |

`options` is a `key` → value map with the engine's advanced options
(`DriverInfo.create_database_fields`); if it is missing or empty, the database
is created with just the name. `create_database_script` returns what
`create_database` would run (SQL, or the API call in BigQuery, Couchbase and
other HTTP engines) and fails if the name is empty. `create_database_choices`
returns the server's suggestions (`{ key, default, values }`) and an empty list
if it has none. If a step after creation fails, the error says the database was
created and which step it was. Details: [`create-databases.md`](create-databases.md).

`drop_objects` drops tables and collections with the driver's DDL (`table_ddl`
with `drop`). On SQL engines it also drops views, routines and triggers. It
works in passes, so an object that others depend on is dropped once those are
gone. It doesn't accept read-only connections. In the explorer it is used from
"Delete…" in an object's menu, or from "Delete N objects…" with several
selected (Cmd/Ctrl+click adds or removes, Shift+click takes a range); with
several, it asks to type «delete» to confirm.

## Server monitor

| Command | args | Returns |
|---|---|---|
| `monitor_snapshot` | `{ connection_id }` | `MonitorSnapshot` |
| `monitor_processes` | `{ connection_id }` | `ServerProcess[]` |
| `monitor_cancel_query` | `{ connection_id, id }` | nothing |
| `monitor_kill_session` | `{ connection_id, id }` | nothing |

`monitor_processes` is the [Processes](processes.md) list, only for drivers
with `capabilities.processes`; it polls through its own session
(`processes:<id>`) and, if the connection drops, the next call reconnects.
`monitor_cancel_query` (`capabilities.cancel_query`) stops another session's
statement and leaves it open; `monitor_kill_session` terminates it. The `id` is
the one `monitor_processes` reports. Neither has events or can be cancelled:
they are short calls. The interface hides them on read-only connections.

Only for drivers with `capabilities.monitor`. It uses the connection's own
session (`monitor:<id>`), so polling doesn't wait behind the explorer or a
running query; if the connection drops, the next call reconnects.

`MonitorSnapshot` = `{ metrics, tables, info, notes }`:

- `metrics`: `{ key, label, group, unit, value, max, counter }`. `unit` is
  `percent`, `bytes`, `count`, `millis` or `seconds`. If `counter` is `true`,
  the value is a total accumulated since the server started, and the UI shows
  the per-second rate between two readings.
- `tables`: `{ key, title, columns, rows }` (sessions, running queries, locks,
  databases and sizes, nodes…).
- `info`: `[label, value]` pairs (version, role, parameters).
- `notes`: what the engine can't report and why.

## Database script (generator and export)

`generate_script`:

- **args:**
  - `script_id`
  - `connection_id`
  - `database`
  - `objects: ObjectRef[]`
  - `options: { drop, if_exists, create, indexes, foreign_keys, definitions, data, data_limit: number | null }`
  - `path: string | null`
- **Destination:**
  - with `path`, it writes the file and reports progress as it goes;
  - without `path`, it returns the text to open in the editor (with a size cap).
- **Returns:** `{ script: string | null, objects: number, rows: number }`
- **Script order:**
  1. the DROPs, if requested;
  2. all the tables: CREATE and indexes;
  3. the definitions: views, routines and other objects, in the order they arrive;
  4. the data, if requested;
  5. the foreign keys, after the data, so the load doesn't fail because of a
     parent row that isn't there yet (or that was left out by the row cap);
  6. the triggers, at the end, so they don't fire while restoring the data.
- **Another engine:** with `target_driver` (a driver different from the
  database's) the tables are converted with `dbine-schema` and the target
  driver writes the DDL and the INSERTs. Views, routines and triggers are not
  converted: the script says so in a comment, along with the conversion's
  losses.
- **Progress:** event `script-progress` with `{ id, done, total, current }`.
- **Cancel:** `cancel_query` with `session_id = "script:<id>"`.

## File import

`preview_import_file`:

- **args:** `{ path, format, options: { delimiter, header, sheet } }`
- **`format`:** `auto | csv | csv_semicolon | tsv | json | json_lines | xlsx | xml`
- **Returns:** `{ format, columns: [{ name, inferred_type }], rows: Cell[][], sheets: string[] }`
  - `rows` carries the first 50 rows;
  - `inferred_type` is `integer | number | boolean | date | datetime | text`.

`import_file`:

- **args:**
  - `import_id`
  - `connection_id`
  - `database`
  - `path`
  - `format`
  - `options`
  - `target: ObjectRef`
  - `create_table: TableSchema | null`: creates it before importing;
  - `mapping: [{ source, target }]`
  - `batch: number`
- **Returns:** `{ rows, elapsed_ms }`
- **Progress:** event `import-progress` with `{ id, rows }`.
- **Cancel:** `cancel_query` with `session_id = "import:<id>"`.

## Run a script file (restore a dump)

`run_script_file`:

- **args:** `{ run_id, connection_id, database, path, continue_on_error }`
- **Returns:** `{ statements, errors: string[], elapsed_ms }`
- **How it reads it:** in parts, splitting the statements according to the driver (`GO` in SQL Server).
- **Progress:** event `script-run-progress` with `{ id, statements, bytes, total_bytes }`.
- **Cancel:** `cancel_query` with `session_id = "run:<id>"`.

## Backups

See [backups.md](backups.md). `list_drivers` carries `backup` (`BackupSpec` or
`null`) per driver.

- `backup_list` `{ connection_id, database }` → `{ copies, native, native_error }`:
  - `copies`: DBine's copies of that database. With an empty `database`, those
    of the whole connection.
  - `native`: the server's history, when the engine reports it.
  - `native_error`: the error reading the history. The copies are returned
    anyway.
- `backup_default_path` `{ connection_id, database }` → the suggested path for
  a new copy.
- `backup_copy` `{ backup_id, connection_id, database, objects, data, path }`
  → the saved copy (`BackupCopy`):
  - **Progress:** event `script-progress` with `id = backup_id`.
  - **Cancel:** `cancel_query` with `session_id = "script:<backup_id>"`.
- `backup_copy_delete` `{ id, delete_file }`: removes the copy from the list
  and, with `delete_file`, also deletes the file.
- `backup_script` `{ connection_id, action }` → the engine's script:
  - `action`: `{ action: "backup", database, options }`,
    `{ action: "restore", source, database, options }` or
    `{ action: "delete", source }`.
  - The UI runs it with `execute_query` in `script_database` (or in the tab's
    database), without saving it to the history.
- To restore a copy, `run_script_file` is used.

## Preferences

| Command | args | Returns |
|---|---|---|
| `list_settings` | — | `{ [key]: value }` |
| `set_setting` | `{ key, value }` | `void` |

- `value` as `null` deletes the preference and goes back to the default value.
- `local.*` keys belong to the machine and are not accepted.
- Keys in use: `grid.copyFormat`, `query.maxRows`.

## Downloadable drivers

See [`on-demand-drivers.md`](on-demand-drivers.md).

| Command | args | Returns |
|---|---|---|
| `drivers_packages` | — | `{ on_demand, packages: [{ package, label, version, drivers, size, installed, available, previous, status: { kind, … }, min_app_needed }] }` |
| `drivers_install` | `{ package }` | `void` (progress arrives through `component-download`) |
| `drivers_remove` | `{ package }` | `void` |
| `drivers_check_updates` | — | `void` (looks up the index right away; new versions of installed drivers are downloaded in the background and announced through `drivers-changed`) |
| `drivers_rollback` | `{ package }` | `void` (discards the version in use and new connections use the previous one) |

- `on_demand` is `false` in builds that bundle all the drivers (development).
  In that case the list comes back empty.
- `installed` is the bytes on disk, or `null` if it hasn't been downloaded yet.

## Cloud sync

How it works: `docs/sync.md`. The providers are
`google_drive | onedrive | folder`.

| Command | args | Returns |
|---|---|---|
| `sync_status` | — | `{ config, providers, status, dirty, last_sync_at }` |
| `sync_connect` | `{ provider, folder }` | `{ account, remote: { updated_at, device, app_version } \| null }` |
| `sync_cancel_connect` | — | `void` |
| `sync_setup` | `{ passphrase, mode: "upload" \| "restore" }` | `SyncAction` |
| `sync_now` | — | `SyncAction` |
| `sync_upload_now` | — | `SyncAction` |
| `sync_restore_now` | — | `SyncAction` |
| `sync_set_auto` | `{ auto }` | `void` |
| `sync_set_passphrase` | `{ passphrase }` | `void` |
| `sync_change_passphrase` | `{ current, new }` | `SyncAction` |
| `sync_disconnect` | `{ delete_remote }` | `void` |
| `sync_local_backups` | — | `[{ path, updated_at, device, size }]` |
| `sync_restore_local` | `{ path }` | `void` |

What each one does:

- `sync_connect`: signs in in the browser (OAuth) or validates the folder, and
  looks for an existing backup. It doesn't sync anything yet.
- `sync_setup`: does the first upload or restore and then stores the
  passphrase in the keychain.
- `sync_now`: automatic sync (uploads, restores or resolves a conflict).
- `sync_upload_now`: replaces the backup with what is on this machine.
- `sync_restore_now`: replaces what is on this machine with the backup.
- `sync_set_passphrase`: stores on this machine the passphrase changed on
  another one. It first verifies it against the backup.
- `sync_change_passphrase`: reencrypts the backup with a new salt.
- `sync_disconnect`: forgets the account and the passphrase on this machine
  and, if requested, deletes the backup.
- `sync_local_backups`: lists the copies saved before each restore.

`SyncAction` is one of these:

- `{ action: "up_to_date" }`
- `{ action: "uploaded", previous_kept }`
- `{ action: "downloaded", local_backup, device }`

Errors: `wrong_passphrase` (the passphrase doesn't open the backup), `sync_auth`
(the account has to be connected again) and `sync` (provider or folder failure).

Events:

- `sync-status`, with `{ running, last_error, last_error_kind, last_action, last_run_at }`.
- `sync-applied`, when a restore replaced the local state: the UI reloads
  connections, queries and preferences.

## Code quality ([`code-quality.md`](code-quality.md))

| Command | args | Returns |
|---|---|---|
| `lint_script` | `{ connection_id, sql }` | `LintFinding[]` |
| `lint_rules` | — | `Rule[]` |

- `LintFinding`: `{ rule, severity, start, end, line, params }`. `start` and
  `end` are JS string indexes (UTF-16), `line` is the line of `start` (from 1)
  and `params` carries the values the message shows.
- `severity` is `error`, `warning` or `info`.
- `Rule`: `{ id, severity, groups }`. `groups` are the engine groups it applies
  to (`sql`, `tsql`, `postgres`, `mysql`, `oracle`, `influxql`, `cql`,
  `mongodb`, `couchdb`, `search`, `redis`, `etcd`, `cypher`).
- It is text analysis only: it doesn't query the database. Each rule's texts
  are in the interface.

## Document the database ([`database-docs.md`](database-docs.md))

| Command | args | Returns |
|---|---|---|
| `dbdocs_outline` | `{ connection_id, database }` | `{ schemas, kinds, foreign_keys, dependencies }` |
| `dbdocs_generate` | `{ connection_id, database, run_id, path, options }` | `{ path, tables, objects, bytes, notes }` |
| `dbdocs_open` | `{ path, reveal }` | `void` |

- `dbdocs_outline`: what the dialog offers. `kinds` is the number of objects
  per type.
- `options`: `{ format: "html" | "markdown", schemas, tables, views, routines,
  triggers, others, source, indexes, foreign_keys, dependencies, diagram,
  labels }`. `labels` are the document's texts in the interface language;
  without them, Spanish.
- `dbdocs_generate` runs in a read-only session. It adds the format's extension
  if the name doesn't have it.
- `dbdocs_open` opens the file (or shows it in its folder with
  `reveal: true`) and only accepts files written by this run of DBine.

Event: `dbdocs-progress`, with `{ run_id, done, total, phase }` (`phase` is
`objects`, `schema`, `columns`, `source` or `writing`).
Cancel: `cancel_query` with `session_id: "docs:<run_id>"`.

## Query builder ([`query-builder.md`](query-builder.md))

| Command | args | Returns |
|---|---|---|
| `build_query` | `{ connection_id, spec, session_id? }` | `{ sql, warnings, features }` |
| `preview_built_query` | `{ connection_id, spec, session_id }` | `{ sql, columns, rows, truncated, elapsed_ms }` |

- `spec`: `{ database, tables, joins, columns, distinct, limit }`.
- `features` is what the engine offers (joins, `GROUP BY`, `HAVING`,
  aggregates, `DISTINCT`, `ORDER BY`, limit, `OR` groups, operators); the
  interface hides the rest. `warnings` are notices about what the engine
  doesn't do.
- `preview_built_query` brings up to 100 rows in a read-only session.
  Cancel: `cancel_query` with `session_id: "qb-preview:<session_id>"`.

## Data subset ([`data-subset.md`](data-subset.md))

| Command | args | Returns |
|---|---|---|
| `subset_plan` | `SubsetArgs` | `SubsetPlan` |
| `subset_run` | `SubsetArgs` + `{ masks, confirm, seed? }` | `SubsetReport` |

- `SubsetArgs`: `{ run_id, connection_id, database, table, filter, children,
  target_connection_id, target_database }`. `filter` is `{ expression,
  columns, limit }`, with `limit` `{ kind: "all" }`, `{ kind: "rows", count }` or
  `{ kind: "percent", percent }`. `children` is `{ depth, max_rows }` or
  `null`.
- `masks`: per table (`{ schema, name, columns }`), one rule per column:
  `keep`, `fake` (with `kind`), `shift_date` (`days`), `noise` (`percent`),
  `fixed` (`value`), `null` or `hash`.
- `subset_plan` only reads (source and target). It returns the tables in write
  order, the total rows, the cut cycles, the notes and `confirm_label` (the
  text to type if the target is a production one).
- `subset_run` rejects a read-only target and, if it is a production one, a
  `confirm` different from `confirm_label`. Without `seed`, it uses a random
  one per run.
- `SubsetReport`: `{ tables, notes, elapsed_ms, cancelled }`. Each table
  carries `status`: `done`, `error`, `cancelled` or `skipped`.

Event: `subset-progress`, with `{ runId, phase, table, rows, total }`
(`phase`: `read`, `collect`, `create`, `insert`, `cycles`, `constraints`,
`done`). Cancel: `cancel_query` with `session_id: "subset:<run_id>:src"` and
`"subset:<run_id>:tgt"`.

## Optimize query ([`query-optimizer.md`](query-optimizer.md))

| Command | args | Returns |
|---|---|---|
| `optimizer_analyze` | `{ connection_id, database, sql, run_id }` | `Analysis` |
| `optimizer_ai` | `{ connection_id, database, sql, run_id, provider, model?, plans }` | `{ candidates, none, sent }` |
| `optimizer_compare` | `{ connection_id, database, run_id, versions, runs?, max_rows? }` | `Measure[]` |
| `optimizer_cancel` | `{ run_id }` | `void` |

- `Analysis`: `{ language, dialect, engine, writes, supports_explain,
  candidates, notes, hints, warnings, plans, cost, skipped }`.
- `Candidate`: `{ id, source, rule, params, title, explanation, sql, verify }`
  with `source` `rule`, `ai` or `user`.
- `versions` of `optimizer_compare`: `[{ id, sql }]`, the original first.
  `runs` goes from 1 to 20 (3 by default) and `max_rows` is 100000 by default.
- `Measure`: `{ id, executed, error, runs_ms, min_ms, avg_ms, rows, truncated,
  checksum, equivalent, cost, plans, plan_error }`. `equivalent` is `null`
  when it couldn't be verified.
- A query that writes data is not run: only its estimated plan is measured.
- `optimizer_ai` sends the query, the tables' structure and a summary of the
  plan; never rows.

Event: `optimizer-progress`, with `{ run_id, measure }`, one per version.
Cancel: `optimizer_cancel`, or `cancel_query` with
`session_id: "optimize:<run_id>"`.

## Mail for scheduled tasks ([`scheduled-tasks.md`](scheduled-tasks.md#settings--mail))

| Command | args | Returns |
|---|---|---|
| `mail_settings_get` | — | `{ settings, password_saved }` |
| `mail_settings_save` | `{ settings, password? }` | `{ settings, password_saved }` |
| `mail_test` | `{ settings, password?, to }` | `string` |

- `settings`: `{ host, port, security, user, from_address, from_name }` with
  `security` `starttls`, `tls` or `none`. `settings` is `null` if there is no
  saved server yet.
- The password goes to the keychain. If `password` comes empty, the saved one
  is kept; without `user`, it is deleted.
- `mail_test` uses the form as is, saved or not, and returns the send summary.

## Search the database ([`search.md`](search.md))

| Command | args | Returns |
|---|---|---|
| `search_database` | `{ connection_id, database, search_id, query, names?, code? }` | `{ hits, scanned, unreadable, truncated, cancelled, from_catalog }` |

- `query`: `{ text, case_sensitive, whole_word, kinds, max_hits }`. Without
  `kinds`, it searches all types that have code; with `"column"`, in column
  names. Without `max_hits`, it stops at 2000.
- A hit is `{ kind, schema, name, parent, line, text }`. `line` 0 is a match in
  the name; for a column, `parent` is the table and `text` its type.

Event: `code-search-progress`, with `{ search_id, done, total, hits }` (the new
hits). Cancel: `cancel_query` with `session_id: "search:<search_id>"`.

## Health check ([`health-check.md`](health-check.md))

| Command | args | Returns |
|---|---|---|
| `database_health` | `{ connection_id, database, run_id }` | `{ checks, checked_at, skipped }` |

- A check is `{ id, category, title, severity, detail, objects, fix }`, with
  `severity` `ok`, `info`, `warning` or `critical`. They come ordered from most
  to least severe.
- `fix` is a script that the interface opens in a query; it is never run by
  itself.
- Cancel: `cancel_query` with `session_id: "health:<run_id>"`.

## Test data ([`test-data.md`](test-data.md))

| Command | args | Returns |
|---|---|---|
| `datagen_preview` | `DataGenArgs` | `{ table_columns, generators, columns, rows }` |
| `datagen_run` | `DataGenArgs` | `{ rows, elapsed_ms }` |

- `DataGenArgs`: `{ connection_id, database, table, columns, rows, seed?,
  gen_id, batch }`. Each element of `columns` is `{ name, generator, params,
  null_percent }`; columns that aren't named go as `auto`.
- `datagen_preview` returns up to 20 sample rows. `datagen_run` accepts from 1
  to 10,000,000 rows, inserts `batch` at a time (1 to 5000) and rejects
  read-only connections.

Event: `datagen-progress`, with `{ id, rows, total }`. Cancel:
`cancel_query` with `session_id: "datagen:<gen_id>"`.

## Database properties ([`database-properties.md`](database-properties.md))

| Command | args | Returns |
|---|---|---|
| `database_properties` | `{ connection_id, database }` | `DatabaseProperties` |
| `alter_database_script` | `{ connection_id, database, changes }` | `string` |
| `alter_database` | `{ connection_id, database, changes }` | `void` |

- `DatabaseProperties`: `{ fields, values, info, choices, warnings }`.
- `changes` is field → new value, only what changed. With no changes,
  `alter_database` does nothing. `alter_database_script` returns the script
  that is shown before applying.
