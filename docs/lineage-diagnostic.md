# Lineage diagnostic log

A slow lineage (the Dependencies panel of a table, the lineage of a database, the Power BI
cross search) is measured, not guessed at, with one plain-text file: `lineage-diagnostic.log`.
One line per stage of a read, with how long it took, how many objects it handled and how it
ended.

## When it is on

- **Test builds** (the version has `-test` in it): on by default.
- **Released builds**: off.
- **Settings > General > Registro de diagnóstico del linaje** forces it either way. The change
  applies at once, no restart.
- **Ocultar nombres en el registro del linaje** replaces catalog, table and model names with a
  code (`h:1a2b3c4d`), the same code for the same name during one run.

When it is off the stages cost one flag check and write nothing.

## Where the file is

It is in the `logs` folder, next to `app.log`:

| System | Folder |
|---|---|
| Windows | `%APPDATA%\io.addlayer.dbine\logs\` |
| macOS | `~/Library/Application Support/io.addlayer.dbine/logs/` |
| Linux | `~/.config/io.addlayer.dbine/logs/` |

Settings > General has **Abrir carpeta del registro** (opens the folder with the file selected)
and **Copiar ruta**. The file is capped at 5 MB: a full one moves to
`lineage-diagnostic.log.1` (one old file is kept), so the log never grows past 10 MB.

## How to send it

Reproduce the slow lineage once, then send `lineage-diagnostic.log` (and
`lineage-diagnostic.log.1` if it exists). It has no passwords, tokens, host names, URLs'
hosts or row data. Object names are in it unless **Ocultar nombres** is on.

## What a line looks like

```
2026-11-23T10:11:12.345Z op=a1b2c3 stage=databricks.lineage_sql ms=812345 res=ok rows=100000
```

- `op`: the id that ties the lines of one command together (`-`: a stage that doesn't know
  its command, such as the generic scans of `get_dependents`; those carry `target=`).
- `stage`: where it was.
- `ms`: how long the stage took.
- `res`: `ok`, `error` (with `error_kind` and a trimmed `msg`), `timeout`, or `cancelled`:
  the stage never ended because the app's time limit dropped it. A stage that is
  `cancelled` after a long `ms` is the one that used the time.
- The rest are counts (`objects`, `rows`, `items`…) and the limit in force (`limit_secs`).

The first line of a run (`stage=header`) has the app version, the system and the settings that
matter (timeouts, parallelism).

## Stages

App commands:

| Stage | What it tells |
|---|---|
| `get_lineage_graph.total` | A whole-database lineage read: total time, schemas, limit, and objects, edges and external sources it ended with. |
| `lineage_graph_cached.result` | The cache's answer when the panel opens: `hit`, its age in seconds, size. |
| `get_lineage_chunk.total` / `chunk` | A lineage read in chunks: each chunk with its `stage`, `offset`, `len` and `of`. |
| `get_dependents.total`, `dependents`, `job_dependents` | The "used by" side of the Dependencies panel: total, the driver's read, and the server's jobs. |
| `get_references.total` | The "uses" side of the panel. |
| `lineage_warm.read` / `lineage_warm.database` / `lineage_warm.model` | The background read after connecting, per database or Power BI model (including `fresh in cache` skips and timeouts). |
| `cross.model_sources`, `cross.tenant_scan`, `get_cross_users.models` | The Power BI cross search: per model (`source=memory`, `disk` or `server`), the tenant scanner call, and how many models are read at once. |

Generic scans of the driver contract (every engine):

| Stage | What it tells |
|---|---|
| `graph_scan.list_objects`, `graph_scan.foreign_keys`, `graph_scan.definitions`, `graph_scan.jobs` | The whole-database scan: objects listed, foreign keys read, definitions read (`read`, `unreadable`, `slowest_ms`, `limit`). |
| `dependents_scan.database_schema`, `dependents_scan.list_objects`, `dependents_scan.definitions` | The default "used by" scan: it reads the definition of every object one by one. |
| `references_scan.foreign_keys` | The default "uses" read of a table: `via=index_usage` (one table) or `via=database_schema` (the whole database). |

Databricks:

| Stage | What it tells |
|---|---|
| `databricks.lineage_of` | The whole catalog's lineage read, with its totals. |
| `databricks.lineage_sql` | The query over `system.access.table_lineage`: `ms`, `rows`. |
| `databricks.entity_names` | Jobs, notebooks and pipelines named: how many and how many were resolved. |
| `statement` | Every SQL statement on the warehouse: `submit_ms` (time to accept it), `first_state` (`PENDING` right after submitting means the warehouse was starting), `rows`, `total_rows`. |
| `statement.wait` | The wait for that statement: `polls` made. |
| `http` | Every REST call: `method`, `path`, `status`, `ms`. The calls to `/api/2.1/jobs/get` or `/api/2.0/pipelines/...` show how long naming what ran took, one by one. |

## Driver hosts

A driver that runs as its own process (an on-demand driver) writes to the same file: the app
passes it the file's path in the environment variable `DBINE_LINEAGE_DIAG` when it starts it.
A host never rotates the file; the app does.
