# Notebooks

A notebook is a document of **cells**, one under another: **SQL cells** that
run against a connection and keep their result under them, **text cells**
(Markdown) that explain what is being done and why, and **write cells** that
write a cell's result into a table (see
[Writing a DataFrame to a table](#writing-a-dataframe-to-a-table)). It keeps a whole
investigation, runbook or report in one file: the queries, what they returned
and the conclusions.

## The file: Jupyter's `.ipynb`

Notebooks are saved as standard Jupyter notebooks (nbformat 4.5), not in a
format of DBine's own:

- **GitHub shows them**, with the text, the queries and the result tables;
  so do VS Code, JupyterLab, Colab and Databricks (running them there needs a
  SQL kernel).
- **Notebooks from Azure Data Studio** (SQL kernel) open in DBine as they
  are, which matters now that it is retired.
- DBine keeps what is its own under `metadata.dbine` (the notebook's and each
  cell's); other programs ignore it.

The extension is `.ipynb`. DBine registers it as a file type it can open, but
doesn't claim it as the default app (Jupyter or VS Code may own it). A file is
a DBine notebook when it has `metadata.dbine`; any other `.ipynb` opens too
(see [Notebooks from elsewhere](#notebooks-from-elsewhere)).

### What a cell looks like

```json
{
  "cell_type": "code",
  "source": ["SELECT region, SUM(total) AS total\n", "FROM ventas\n", "GROUP BY region"],
  "metadata": {
    "language": "sql",
    "dbine": {
      "name": "ventas",
      "target": { "connection": "Ventas prod", "database": "ventas" },
      "chart": { "type": "bar", "x": "region", "y": ["total"] },
      "keep_result": true
    }
  },
  "execution_count": 3,
  "outputs": [
    {
      "output_type": "execute_result",
      "execution_count": 3,
      "metadata": {
        "dbine": {
          "rows_total": 4,
          "rows_kept": 4,
          "truncated": false,
          "ran_at": "2026-10-18T14:32:05Z",
          "duration_ms": 412,
          "engine": "postgres"
        }
      },
      "data": {
        "text/html": "<table>…</table>",
        "application/vnd.dataresource+json": { "schema": { "fields": [] }, "data": [] },
        "text/plain": "region | total\n…"
      }
    }
  ]
}
```

- The result is stored three ways: `text/html` so any viewer shows a table,
  `application/vnd.dataresource+json` (Table Schema) for data tools, and
  `text/plain` as the last resort.
- An error is stored as a Jupyter `error` output with the server's message.
- The notebook's `metadata` has `kernelspec` and `language_info` set to SQL,
  and `metadata.dbine` holds `version`, the notebook's default target and the
  project environment, if any.

## Where a cell runs

- The notebook has a **default target**: a connection and a database. A cell
  can override it (its own connection or database), so one notebook can query
  several servers.
- Targets are saved **by name**, never with hosts, users or credentials. On
  another machine, a name that doesn't match a local connection asks once
  which connection it is, and that choice stays on that machine (like a
  project's environments). DBine never writes credentials to the file, and
  ignores any it finds there (a target's connection that looks like a
  connection string or URL is dropped too, with a warning).
- **Names are not unique**: when several local connections have the stored
  name, the cell asks which one (only those are offered), and the choice is
  remembered **for that file** on this machine. Picking a connection whose
  name others share remembers it for the file as well. A name that matches
  exactly one connection keeps working with no question.
- **Inside a project with environments** (`.dbine.json`), the target can be
  the **environment** (`dev`, `prod`…) instead of a connection, so the same
  notebook runs on each person's own database for that environment, and
  `confirm_run` asks before running on it.
- A cell can also run on **DataFrames** instead of a connection, as a
  DataFrame SQL cell (see
  [DataFrame SQL cells](#dataframe-sql-cells-cross-engine)); the file stores
  that target as `{ "dataframes": true }`, with no connection name (files
  written before say `{ "results": true }`, which is still read).
- The cell's language follows the target's engine: SQL in each engine's
  dialect, Redis commands, MongoDB's shell syntax, and so on, with that
  engine's completion and highlighting. The file stores it as the cell's
  `language` (the driver's: `sql`, `redis`, `json`…). Nothing about the
  notebook depends on the engine beyond that, so it works with **every
  driver**.

## Running

- **Run cell** (Ctrl/⌘+Enter), **Run and go to the next** (Shift+Enter),
  **Run all**, **Run from here** and **Cancel**.
- A cell runs exactly as in the query editor: same session, same read-only
  rules (a read-only connection refuses writes), same confirmation for
  dangerous statements, same history.
- **One cell runs at a time**: while one runs (or is asking where to run),
  the other run actions are disabled and the shortcuts do nothing.
- **Run all** stops at the first cell that fails. An environment with
  `confirm_run` asks once per target in a Run all, like the editor asks once
  per run. It also stops at every write cell and asks before writing (see
  [Writing a DataFrame to a table](#writing-a-dataframe-to-a-table)).
- The number next to a cell (`[3]`) is the order it last ran in, as in Jupyter.

### Transactions

Each connection and database a notebook's cells run on is one session, kept
until the tab closes, as an editor tab's. Cells run with automatic
transactions; a cell that opens one (`BEGIN`…) leaves it open for the next
cells on that session:

- The notebook's toolbar shows each open transaction with its connection ›
  database, and **Commit** / **Rollback** for it.
- Closing the notebook asks for each open transaction: commit, roll back or
  cancel the close, as the editor does.
- A cancel that closes the session rolls back the open transaction, and says
  so.
- Closing while a cell runs asks first; the run is cancelled only if the
  close goes on.

## Parameters

Values asked for when the notebook runs and used by every cell: the month
of a report, a customer, a date range. Each cell writes them as
`{{name}}`:

```sql
SELECT cliente, sum(total) AS total
FROM ventas
WHERE mes = {{mes}} AND region = {{region}}
LIMIT {{tope}}
```

### Defining them

**Parameters** in the notebook's toolbar opens a panel at the top with one
row per parameter:

| Field | |
|---|---|
| Name | A letter or `_`, then letters, digits or `_` (case doesn't matter, as in `{{MES}}`). Renaming one renames its `{{name}}` in the cells |
| Type | Text, number, date, date and time, boolean, or a fixed list of values |
| Default | Used when the run gives no value; optional |
| Required | The run can't go on without a value (given or the default) |
| Description | Shown when the value is asked for |

They are saved in the file, in `metadata.dbine.parameters`, defaults
included. A default is never a credential or a connection's details: one
under a name like `password` or `token`, or one that looks like a
connection string, is dropped when the file is read and when it is written
(with the notice about ignored credentials). A file without parameters
opens as before.

```json
"metadata": {
  "dbine": {
    "version": 1,
    "parameters": [
      { "name": "mes", "type": "text", "required": true, "description": "AAAA-MM" },
      { "name": "region", "type": "list", "options": ["Norte", "Sur"], "default": "Norte" },
      { "name": "tope", "type": "number", "default": 100 }
    ]
  }
}
```

### Using them in cells

- `{{name}}` goes **where a value goes**, in SQL cells on any connection, in
  DataFrame SQL cells and in the other languages (MongoDB, Redis, Cypher,
  Flux, CQL…). Completion offers the parameters after `{{` (and with
  Ctrl+Space).
- **Never inside a string**: `'%{{x}}%'` is refused, with the way to write
  it (`'%' || {{x}} || '%'`), because the value already goes as text.
- **Never as a name**: a parameter where a table, column, schema or function
  goes (`FROM {{t}}`, `t.{{c}}`, `{{f}}(…)`, `tabla_{{mes}}`, `ORDER BY
  {{c}}`, `"{{x}}"`, a MongoDB key or collection, a Redis command) is refused
  with a clear message. Identifiers aren't parameterizable.
- **In a comment** it stays as it is.
- In a SQL cell, `{{name}}` with no parameter of that name is an error. In
  other languages it stays as text (an Elasticsearch search template has its
  own `{{x}}`).

Why `{{name}}`: it means nothing in any engine's own syntax outside strings.
`@x` is a T-SQL variable, `:x` a PostgreSQL cast and an Oracle bind, `$1`
PostgreSQL's placeholder and `?` an operator in several engines.

### How a value reaches the engine

Never as raw text. Each value goes **bound** (the driver sends it apart from
the statement, with its own placeholders) where the driver binds, and
otherwise **as a literal written by the engine's own quoting**:
`E'O''Brien'` in PostgreSQL, `N'…'` in SQL Server, `ISODate("…")` in
MongoDB, redis-cli quoting in Redis. Numbers, dates and booleans are checked
against their type before anything runs, so they can't hold anything but
themselves. Which engines bind and which get literals:
[engine-support.md](engine-support.md#notebook-parameters).

### Running

- **Run all** and **Run cell** ask for the values that are missing (a
  required parameter the cells use with no value), in one dialog prefilled
  with this session's values and the defaults.
- The values in use are shown under the toolbar; a click changes them.
  They belong to the open notebook (this session): the file keeps only the
  defaults.
- Each result records the values that produced it (those of the parameters
  its cell used; a DataFrame SQL cell also those of the cells it read) in its
  output metadata, `metadata.dbine.params`, and its summary line shows them
  (also on GitHub and in the exports). When a value its cell uses changes,
  the result is marked **outdated** until the cell runs again.
- A DataFrame SQL cell reuses a cell's rows from this session only if they
  came from the values in use; otherwise that cell's query runs again with
  them.

### Without the UI

`notebook::params::prepare_notebook(notebook, values, driver_of)` turns a
notebook and a map of values into each code cell's executable form: the text
with the values as literals, and per statement the bound form for the
drivers that bind. Missing values take their defaults; a required one with
neither, or a parameter used where it can't go, fails before anything runs.
`notebook::params::run_prepared` runs one such cell on a session as the
editor does. The scheduled **Run a notebook** step uses it with the values
it stores ([scheduled-tasks.md](scheduled-tasks.md)): each cell's values are
bound or quoted as in the editor, and each result records them.

## DataFrame SQL cells (cross-engine)

Each cell's result is a **DataFrame** with the cell's name (`c1`,
`ventas`…), and so is a file read with `read_csv`, `st_read` and the other
readers. A **DataFrame SQL** cell (target **DataFrames**; its header says
*DataFrame SQL · DuckDB*) queries and joins those DataFrames on your
computer with [DuckDB's SQL](https://duckdb.org/docs/stable/sql/introduction),
using no connection: each DataFrame is a table named after its cell. One
notebook can query PostgreSQL in one cell, SQL Server in another and MongoDB
in a third, and join them in a fourth:

```sql
SELECT c.nombre, r.region, sum(v.total) AS total
FROM ventas_pg v
JOIN clientes_mssql c ON c.id = v.cliente
JOIN regiones_mongo r ON r.cliente = v.cliente
GROUP BY ALL
```

- **Cell names.** Every SQL cell has a name, shown in its header and edited
  there with a click: letters, digits and `_`, starting with a letter or `_`,
  up to 63, not an SQL keyword, unique in the notebook (ignoring case, as
  DuckDB does). A new cell is named `c<n>` by its place among the SQL cells
  (the next free number if that one is taken), and it keeps the name when it
  moves. The file stores it as the cell's `metadata.dbine.name`; a file
  without names (older ones, Azure Data Studio's, Jupyter's) gets them when
  it opens, and keeps them once saved.
- **What gets loaded.** Only the cells the SQL names as tables, read outside
  strings and comments (`t.c1` is a column, not a table), and only cells
  above it. A source that is itself a DataFrame SQL cell is loaded as its
  query over the tables above it (`CREATE TABLE … AS`), so DataFrame SQL
  cells can build on each other; it must be a single query.
- **Where the rows come from.** Never from a preview read from the file.
  - The source's **run in this session**, when it ran exactly its current
    text on its current target and nothing cut it (not truncated by the
    row limit, no error): those rows, as they were shown.
  - Otherwise its query **runs again** on its own connection, through the
    same read path as the whole-result charts: a read-only session of its
    own, with the server's read-only mode where the engine has one, else the
    read-only guard. A statement the guard sees as a write is **never run
    again**: the cell says so and asks to run that cell first (then its run
    is used). The source's target is resolved and connected first, as when
    running it (an environment with `confirm_run` asks).
- **Every engine as a source.** Rows come through the generic result path,
  so any driver works: SQL engines, MongoDB, Redis, Cassandra, Elasticsearch…
  Each column gets a DuckDB type from the source's type name and its values
  (integers, exact decimals, dates, timestamps, UUIDs, JSON documents);
  drivers that give no type names (PostgreSQL's results, MongoDB, Redis) are
  typed from the values. Anything else is `VARCHAR`. If DuckDB refuses a
  value (a date it can't read), only the columns whose values don't convert
  stay text, and the cell says which.
- **Cap.** At most 1,000,000 rows per table (**Settings › Notebooks**, from
  1,000 to 10,000,000); a table cut at the cap says so in the cell.
- **Cache.** A table is loaded again only when its source ran again, its
  text or target changed, the cap changed, or the table is gone (the cell's
  own SQL may drop it). Otherwise the earlier load is reused.
- **What the cell shows.** Under its summary, each table it used: rows, how
  long the load took, and where it came from (its run, its query run again,
  the tables above, or already loaded); a cap or columns left as text are
  also warnings in its messages, which the file keeps.
- **Its own result** is like any other cell's: table, chart and map views,
  the preview caps when saved, export. The assistant gets its query, columns
  and counts, never its rows. Charts plot the cell's rows (all of them up to
  the query row limit), and a full export exports the rows shown.
- **Calcular sobre todo el resultado.** The cell's full result can be larger
  than what it shows (the query row limit). With the switch on, the chart is
  computed by DuckDB over every row: the cell's SQL, checked again with the
  same refusals as a run (`COPY`, `ATTACH`, `SET`…), is wrapped in a
  `GROUP BY` (or, without X, the totals) on the notebook's own in-memory
  session, still locked down, and over the very DataFrames its last run read.
  Histograms and box plots read the rows from that same session and DBine
  computes them (up to 1,000,000 rows). Nothing is loaded for it: no source
  runs again, no file is read, nothing reaches the network or the disk. The
  note says *Agregado en DuckDB sobre N filas*. If the notebook's session is
  gone, the cell hasn't run in it, or a DataFrame it reads was loaded again
  since (its source ran again), it asks to run the cell first.

### Reading local files

A DataFrame SQL cell can also read local data files with DuckDB's readers and
join them with the cells' tables:

```sql
SELECT *
FROM c2 c
JOIN read_csv('~/Downloads/CustomerTest.csv') t ON t.CustomerID = c.CustomerID
```

- **Readers:** `read_csv`, `read_csv_auto`, `read_parquet` / `parquet_scan`,
  `read_json`, `read_json_auto`, `read_ndjson`, `read_ndjson_auto` and
  `read_xlsx` (an Excel workbook, one file; DuckDB's `excel` extension is
  fetched the first time, as for `.xlsx` data-file connections). Their
  options work as in DuckDB (`read_csv('x.csv', delim = ';', header = true)`).
  Also geographic files with `st_read` and tables of SQLite and DuckDB
  database files with `sqlite_scan` and `read_duckdb` (below).
- **The path** is the reader's first argument, written as text: `'a.csv'`,
  a glob (`'exports/ventas_*.csv'`, `'**/*.parquet'`) or a list
  (`['a.csv', 'b.csv']`). `~` is the home folder; a relative path is read
  from the notebook's folder (an unsaved notebook refuses it and asks to
  save first, or to write the full path). A path built from a column or an
  expression is refused. Each path is resolved to the real file (symlinks
  followed, `..` removed); a missing file, a folder, or a glob that matches
  nothing is an error naming the path.
- **URLs:** `https://`, `s3://`, `gs://` and `az://` / `abfss://` are read
  too, by DBine and never by DuckDB (see
  [Reading files from the web and cloud storage](#reading-files-from-the-web-and-cloud-storage));
  any other scheme (`http`, `file`, `hf`…) is refused.
- **What the cell shows:** under its summary, next to the cells' tables,
  each file read (shortened: relative to the notebook's folder or with
  `~`), its rows, size and modification time, and whether it was read now or
  was already loaded; the tooltip lists every file of a glob or list in
  full.
- **Rows and cache:** a file read is a table like a cell's: capped at the
  same rows per table (and said), and read again only when the call
  changes or a file's size or modification time does. A DataFrame SQL cell
  that reads a file can be used as a table by the cells below it, and is built
  again when the file changes.
- **Only reads.** The files are read in a DuckDB session of their own,
  granted exactly those files (`allowed_paths`) and then locked down like
  the notebook's; it runs only the reader call, rebuilt with the resolved
  paths, never the cell's SQL. The rows come into the notebook's database
  as a table and the call in the cell's SQL is replaced by it. The
  notebook's database is never granted a file, so `COPY … TO`, `ATTACH`,
  `read_text('…')` or `FROM 'file.csv'` on a file the cell reads are still
  refused by DuckDB. Nothing is written to disk, except the short-lived
  private copies described for downloaded objects and GeoPackages with a
  pending `-wal`.
- **Refused statements.** These cells refuse, before running anything,
  statements that write, attach or configure: `COPY`, `EXPORT` / `IMPORT
  DATABASE`, `ATTACH` / `DETACH`, `INSTALL` / `LOAD` (and `FORCE INSTALL`,
  `UPDATE EXTENSIONS`), `CREATE` / `DROP SECRET`, `SET` / `RESET` (except
  `SET VARIABLE`), `PRAGMA` (the `pragma_…()` table functions still work)
  and `CHECKPOINT`.
- **Completion:** after `FROM`, `JOIN` or a comma in a `FROM` list, the
  editor offers the cells above (a cell that hasn't run too, without
  columns) and these readers (`st_read`, `sqlite_scan` and `read_duckdb`
  too); picking a reader writes `read_csv('')` with the cursor between the
  quotes (`sqlite_scan('', '')` and `read_duckdb('', '')`: Tab goes to the
  table's quotes, where no file is offered), where "Elegir archivo…" opens the system's
  file dialog and writes the chosen path. The editor never lists a folder:
  choosing or typing a path grants nothing: the file is approved when the cell runs. After `name.` or an alias come
  the cell's columns, and the select list offers the columns of the tables
  the statement reads.
- **Approval:** a notebook reads no file the user hasn't allowed (see
  [Approving the files a notebook reads](#approving-the-files-a-notebook-reads)).

#### Geographic files (`st_read`)

```sql
SELECT z.name, sum(v.total)
FROM st_read('~/geo/zonas.shp') z
JOIN ventas v ON ST_Contains(z.geom, ST_Point(v.lon, v.lat))
GROUP BY ALL
```

- **What it reads:** one local file, through DuckDB's `spatial` extension
  (GDAL), fetched the first time like `excel`: GeoJSON (`.geojson`,
  `.json`), GeoJSON sequences (`.geojsonl`, `.geojsons`), TopoJSON,
  Shapefile (`.shp`, a lone `.dbf`, and zipped as `.shp.zip` or `.shz`,
  which GDAL opens by itself), GeoPackage (`.gpkg`), KML, GPX, FlatGeobuf
  (`.fgb`), CSV (with a WKT column or coordinate columns, through
  `open_options`) and DXF. GDAL may only use the drivers for those formats:
  DBine always passes `allowed_drivers` itself, so a file is never opened
  by another driver whatever its name or content says (a VRT renamed
  `.geojson` is not read as a VRT).
- **Sidecars:** a Shapefile's `.shx`, `.dbf`, `.prj`, `.cpg`, `.sbn`,
  `.sbx`, `.qix`, `.fix`, `.qpj` and `.shp.xml` (same stem, any case) and a
  CSV's `.csvt` and `.prj` are found next to the file and read with it.
  They are part of the read: each one is checked against the always-blocked
  list, granted to the reader session with the file and approved with it
  (the dialog shows the file with *"+ N archivos asociados"*, listed in its
  tooltip; one answer covers them; *Permitir siempre* grants each one, so a
  sidecar that appears later asks again), and a change in any of them reads
  the file again and raises the change notice. The *DataFrames:* line shows the
  file with *"+ N archivos asociados"*, and its tooltip lists them.
- **A GeoPackage with a pending `-wal`** (open in another program, in WAL
  mode) or a hot `-journal` isn't the whole layer set by itself: that
  journal is a sidecar, as for `sqlite_scan` (approved, blocked and cached
  with the file; the `-shm` is never read). The file and its journal are
  copied into a private folder in the temp folder, SQLite applies the
  journal to the copy and leaves it in rollback mode, GDAL reads the copy
  (the reader session is granted only the copy) and the folder is removed.
  The originals are never written, so their bytes and modification times
  don't change.
- **Parameters:** `layer`, `keep_wkb`, `max_batch_size` and
  `open_options`, with literal values. `open_options` takes only these GDAL
  options, whose values can't contain `/`, `\`, `:` or `vsi`:
  `X_POSSIBLE_NAMES`, `Y_POSSIBLE_NAMES`, `Z_POSSIBLE_NAMES`,
  `GEOM_POSSIBLE_NAMES`, `KEEP_GEOM_COLUMNS`, `SEPARATOR`, `HEADERS`,
  `AUTODETECT_TYPE`, `AUTODETECT_WIDTH`, `AUTODETECT_SIZE_LIMIT`,
  `QUOTED_FIELDS_AS_STRING`, `EMPTY_STRING_AS_NULL`, `MAX_LINE_SIZE`,
  `ENCODING`, `ADJUST_TYPE`, `ADJUST_GEOM_TYPE`,
  `FLATTEN_NESTED_ATTRIBUTES`, `NESTED_ATTRIBUTE_SEPARATOR`,
  `ARRAY_AS_STRING`, `DATE_AS_STRING` and `LIST_ALL_TABLES`. Anything else
  is refused before running: `allowed_drivers` and `sibling_files` (DBine's
  own), GeoPackage's `PRELUDE_STATEMENTS` (SQL), GeoJSON's
  `FEATURE_SERVER_PAGING` (network), and any other parameter or option.
- **Refused paths and formats:** GDAL virtual paths (`/vsizip/`,
  `/vsicurl/`, `/vsis3/`, `/vsigzip/`, `/vsimem/`… any path segment
  `vsi…`), URLs (anything with `://`, also GDAL's `WFS:http://…`), a glob or
  a list, and the formats that point GDAL at other files or the network:
  `.vrt`, `.xml`, `.gml`, `.xsd`, `.gfs`, `.tab`, `.map`, `.osm`, `.pbf`,
  `.ovr`, `.wfs`, `.mbtiles`, `.pmtiles`. A `.zip` other than a zipped
  shapefile is refused with a message to unzip it and read the file inside.
  The GDAL drivers left out are `OGR_VRT`, `GML`, `SQLite` (SQLite virtual
  tables over other sources), `MapInfo File`, `OSM` (temporary files) and
  every network driver.
- **Geometry:** the geometry column (DuckDB's `GEOMETRY`) comes into the
  notebook's database as WKB and is rebuilt there as `GEOMETRY`
  (`ST_GeomFromWKB`, part of DuckDB itself), so the cell's spatial
  functions work on it and the result's map view draws it. The same goes
  for a `GEOMETRY` column of a cell above (another DuckDB result); a value
  that isn't WKB keeps the column as text, and the cell says so.
- **Spatial functions in the cell** (`ST_Contains`, `ST_Point`…) need the
  `spatial` extension in the notebook's database too: when a cell (or a
  DataFrame SQL cell it uses) calls an `ST_…` function or `st_read`, the
  notebook's database is opened with `spatial` loaded before the lockdown
  (a database opened without it is opened again, and its tables are loaded
  again as needed). If the extension can't be fetched, the cell says so.

#### Database files (`sqlite_scan`, `read_duckdb`)

```sql
SELECT c.nombre, z.zona
FROM sqlite_scan('~/crm/crm.db', 'clientes') c
JOIN read_duckdb('~/datos/ventas.duckdb', 'arch.zonas') z ON z.cliente = c.id
```

- **`sqlite_scan('file.db', 'tabla')`**: one SQLite file and one table.
  DBine reads it itself through SQLite, never with DuckDB's sqlite
  extension (whose SQLite opens files outside DuckDB's `allowed_paths`
  and, for a database in WAL mode, creates `-wal` and `-shm` files next to
  it). The file is opened read-only and `immutable`, so nothing is locked,
  created or changed next to it, with `trusted_schema` off. A file with a
  pending, non-empty `-wal` or `-journal` (open in another program, or left
  mid-write) isn't the whole database by itself: that journal is a sidecar
  of the file (approved, blocked and cached like a shapefile's), and the
  file and its journal are copied into a private folder in the temp folder,
  the copy is read and the folder is removed. The original files are never
  written.
- **`read_duckdb('file.duckdb', 'tabla')`** (or `'esquema.tabla'`, or
  `table_name = '…'`, `schema_name = '…'`): DuckDB's own `read_duckdb`
  (present in the bundled DuckDB 1.5), which attaches the file read-only,
  run in a reader session granted that file. Globs and lists work as for
  the other readers. DuckDB looks for the file's write-ahead log
  (`file.duckdb.wal`): that path is granted too; when it exists it is a
  sidecar of the file. Other `read_duckdb` parameters are refused. The
  notebook's database never attaches anything (`ATTACH` stays refused).
- **Table names** are letters, digits, `_`, `$`, `-` and spaces
  (`esquema.tabla` at most, and no schema for SQLite), checked before
  running and passed as strings, never written into SQL.
- **DBine's own databases** (its state and cache) are in its own folders,
  which are always blocked.

#### One reader session per call

Each reader call is read in a DuckDB session of its own, granted only that
call's files and their sidecars, with only the extension it needs (`excel`,
`spatial`); `sqlite_scan` doesn't use DuckDB at all. A file's content can't
reach another call's files.

#### Approving the files a notebook reads

A notebook from elsewhere could name any readable file (`~/.aws/credentials`)
and its result would be shown and saved in the file's preview. So, before
the reader session runs, every resolved file (canonical path) is checked
in one place (`allowed_files` in `src-tauri/src/notebook/files.rs`, rules in
`src-tauri/src/notebook/access.rs`):

- **Always blocked**, never asked: the run is refused with a message naming
  the path. Checked after canonicalization, so a symlink into one of these
  is blocked too, and ignoring case (`~/.SSH` is `~/.ssh`):
  - folders under the home folder: `~/.ssh`, `~/.aws`, `~/.gnupg`,
    `~/.config`, `~/.kube`, `~/.docker`, `~/.azure`, `~/.gcloud`, `~/.oci`,
    `~/.password-store`, `~/.mozilla`, `~/.local/share/keyrings`,
    `~/Library/Keychains`, `~/Library/Cookies`, `~/Library/Safari` and the
    browsers' profiles under `~/Library/Application Support` (Chrome,
    Chromium, Brave, Edge, Firefox, Vivaldi, Arc, Opera);
  - on Windows, `%APPDATA%` / `%LOCALAPPDATA%` (and the same folders under
    `%USERPROFILE%\AppData`): `Microsoft\Credentials`, `Microsoft\Protect`,
    `Microsoft\Crypto`, `Microsoft\Vault`, Chrome's, Edge's and Brave's
    `User Data`, `Mozilla`, Opera's, and `postgresql` (`pgpass.conf`);
  - the system's `/etc`, `/private/etc`, `/var` and `/private/var` (the
    temp folder of macOS is under `/var` too);
  - DBine's own folders (state, data, cache, logs, downloaded components);
  - shell history and rc files directly in the home folder: `~/.*_history`,
    `~/.zsh*`, `~/.bash*`, `~/.profile`, `~/.zprofile`, `~/.zlogin`,
    `~/.zlogout`, `~/.history`, `~/.lesshst`, `~/.viminfo` (when one is a
    symlink, its target is blocked too);
  - credential files anywhere: `.netrc` / `_netrc`, `.npmrc`, `.pypirc`,
    `.git-credentials`, `.pgpass` / `pgpass.conf`, `.my.cnf`, and any
    `.env` or `.env.*`.

  The list is kept in one place (`HOME_DIRS`, `SYSTEM_DIRS`,
  `CREDENTIAL_NAMES` and the name rules in `access.rs`) and tested.
- **Not allowed yet:** the run stops before anything is read and the
  notebook shows *"Este notebook quiere leer estos archivos:"* with every
  pending file in full (canonical path, size and modification time) and,
  for a glob, its pattern above its files. One dialog per run; one answer
  covers all of them:
  - **Permitir**: this run only; nothing is remembered.
  - **Denegar**: the cell doesn't run; its result is an error naming the
    files.
  - **Permitir siempre**: it runs, and this machine remembers that this
    notebook may read these files. The grant is the pair (notebook file,
    file), both canonical: a new file asks again, and so does another
    notebook. A file found by a glob is granted as the pattern resolved to
    its folder (`/Users/ana/exports/ventas_*.csv`), so new files of that
    same pattern don't ask again (a file the glob found that resolves
    outside that folder, through a symlink, is granted as itself). For an
    unsaved notebook it is disabled ("guardá el notebook para recordarlo").
- **Enforced in the backend:** a run that needs approval returns a
  one-time token for exactly the files it asked about; the dialog's answer
  goes to `notebook_file_decision` (the only command that writes a grant,
  and only for that token's files), and the run that follows brings the
  token back. The backend resolves the files again and reads only if the
  grants and that token's files cover every one of them (a new file is
  asked about again); the token works once, for that tab and notebook, for
  15 minutes. A blocked file is refused whatever the token says. No MCP
  tool or assistant tool runs a DataFrame SQL cell.
- **URLs** are approved like files, in the same dialog and with the same
  three answers, keyed by the canonical URL (see
  [Reading files from the web and cloud storage](#reading-files-from-the-web-and-cloud-storage)).
- **Content change notice** (it doesn't stop the run): each run keeps, for
  each file read, its size, modification time and SHA-256, per notebook
  (for an object on the web or in cloud storage, its `ETag` instead of the
  hash, else its `Last-Modified` and size).
  The hash is computed only when the size or the modification time changed
  since the stored record (no big file is hashed on every run). When a file
  read before has a different hash, the result's *DataFrames:* line says
  *"cambió desde la última ejecución (fecha anterior → fecha nueva)"*. An
  unsaved notebook keeps no records.
- **Where it lives:** the grants and the records are in DBine's local
  state on this machine (the `notebook_file_grants` and
  `notebook_file_reads` tables of the state database), never in the
  `.ipynb`, and they aren't part of the cloud backup or the export. They
  are keyed by the notebook's path: a notebook moved or renamed asks again
  (its old grants stay listed until removed). A project's notebook is
  keyed by its path on disk (the project's folder + its path).
- **Managing them:** Configuración › Notebooks › *Archivos que los
  notebooks pueden leer* lists every grant (notebook → file or pattern,
  when it was granted), each one with its remove button, and *Quitar
  todos*. A notebook's toolbar has *Permisos de archivos…* with that
  notebook's grants.

### Reading files from the web and cloud storage

```sql
SELECT r.region, sum(v.total)
FROM read_parquet('s3://ventas-anuales/2026/*.parquet') v
JOIN read_csv('https://datos.gob.ar/dataset/regiones.csv') r USING (cliente)
GROUP BY ALL
```

- **What is read:** `read_csv`, `read_csv_auto`, `read_parquet` /
  `parquet_scan`, `read_json`, `read_json_auto`, `read_ndjson` and
  `read_ndjson_auto` take URLs as their path; `st_read` takes one
  `https://` file of a format that needs no sidecars (a shapefile only
  zipped, `.shp.zip` or `.shz`), with the same parameters, options and
  `allowed_drivers` as a local file. `read_xlsx`, `sqlite_scan` and
  `read_duckdb` read local files only. A call reads either local files or
  URLs, not both.
- **Which URLs:**
  - `https://host/path?query`: one file; no globs. `http://` is refused,
    since it travels unencrypted (and anyone on the way could change the
    data).
  - `s3://bucket/key`, `gs://bucket/key` (or `gcs://`),
    `az://account.blob.core.windows.net/container/blob`,
    `abfss://container@account.dfs.core.windows.net/path` and, with
    `AZURE_STORAGE_ACCOUNT` set, `az://container/blob`. Their keys may be
    globs (`s3://ventas/2026/*.parquet`, `gs://b/**/*.csv`): DBine lists the
    bucket under the prefix before the first wildcard and reads the objects
    that match, up to 1,000.
  - Refused: a URL with a user or password (`https://ana:clave@…`), or with
    a signature or token in its query (`X-Amz-Signature`, `X-Goog-Signature`,
    Azure's `sig=`, `token=`, `key=`, `access_token=`…). Credentials never go
    in the notebook or the cell's SQL; the message says to use the URL
    without them and the computer's sign-in. Also refused: keys with `.` or
    `..` segments, and any other scheme (`file://`, `hf://`, `ftp://`…).
- **Never DuckDB's network.** Neither the notebook's database nor the
  reader session ever loads `httpfs` or reaches the network. DuckDB 1.5's
  `allowed_paths` does filter URLs by their text (a URL not in the list is
  refused, tested), but `httpfs` would resolve the host itself (so a public
  name pointing at `127.0.0.1` or `169.254.169.254` couldn't be checked),
  follow redirects to any host, and need the credentials inside DuckDB.
  So, once approved, **DBine fetches each object itself**: into a private
  folder in the temp folder (`0700`, one per reader call), which the reader
  session is granted exactly like local files (`allowed_paths`, then the
  lockdown), and which is removed as soon as the call has been read (also
  when it fails or is cancelled). Each object keeps its key's path below
  that folder, so Hive partitions (`año=2026/`) and compressed extensions
  (`.csv.gz`) work as for local files. This is the one place DataFrame
  SQL cells write to disk.
- **Which hosts.** Before connecting, DBine resolves the host itself and
  refuses it if any of its addresses is loopback (`127.0.0.0/8`, `::1`),
  private (`10/8`, `172.16/12`, `192.168/16`), link-local (`169.254/16`,
  the cloud metadata service `169.254.169.254` included; `fe80::/10`),
  unique-local IPv6 (`fc00::/7`, `fd00::/8` included), CGNAT
  (`100.64/10`), unspecified, multicast, broadcast, documentation or
  reserved, IPv4-mapped and NAT64 forms included; names such as
  `localhost`, `*.localhost`, `*.local`, `*.internal` and
  `metadata.google.internal` are refused before resolving. The connection
  is pinned to the addresses that were checked (no second lookup that
  could answer differently) and never goes through a proxy. A redirect is
  followed only to the same scheme, host and port (at most 5); one to
  another server stops the read with a message naming it.
- **Credentials** are the computer's sign-ins, never the notebook's, the
  same chains as the connections' cloud sign-ins:
  - **S3:** the AWS chain: environment keys, the profile of
    **Settings › Notebooks › Perfil de AWS** (else `AWS_PROFILE`, else
    `default`) with its keys, `credential_process`, SSO or roles through the
    AWS CLI. DBine signs each request with SigV4 (the keys never leave the
    process); a bucket in another region is asked again in the region its
    answer names.
  - **Google Cloud Storage:** gcloud, then the application default
    credentials (`gcloud auth application-default login`).
  - **Azure:** the Azure CLI (`az login`), the Azure Developer CLI, the
    managed identity.

  Without a sign-in an object is read anonymously (public buckets); a
  denied read says how to sign in (`aws sso login`,
  `gcloud auth application-default login`, `az login`). No secret is
  stored anywhere by this feature, and DuckDB never gets one.
- **Approval:** the same dialog, grants and token as local files (see
  [Approving the files a notebook reads](#approving-the-files-a-notebook-reads)).
  Nothing is sent over the network before the answer. The dialog shows
  each URL with its host (or bucket and cloud) first, in bold, and the
  full URL under it. A grant is keyed by the canonical URL (scheme, host,
  port, path and query; the fragment dropped); a cloud pattern is granted
  as written (`s3://ventas/2026/*.parquet`) and covers the objects that
  match it later. The always-blocked list is for local files; URLs have
  the rules above.
- **Limits:** 2 GiB per object (`MAX_OBJECT_BYTES`, checked against the
  listing, the announced length and the bytes received) and 8 GiB per
  reader call, plus the rows per table of Settings › Notebooks; 15 s to
  connect and 60 s without a byte. **Cancel** stops the listing or the
  download.
- **Cache and change notice:** each run asks for the objects' `ETag` and
  `Last-Modified` (a `HEAD`, or the listing for a pattern); an object
  downloads again only when its version changed (an object whose server
  gives neither is read every run). The change notice compares the
  `ETag` (else `Last-Modified` and size); nothing is hashed.
- **What the cell shows:** the URLs on the *DataFrames:* line, with size,
  time (when the server gave one) and *"descargado de la red"*.
- **Completion** keeps its rules; granted URLs and cloud patterns are
  offered inside a reader's quotes like granted files.

### The DuckDB database

- One per notebook tab, **in memory only**, opened on the first DataFrame SQL cell
  and closed with the tab: nothing is written to disk, and closing the tab
  drops every loaded table. It is a database of its own (the DuckDB driver's
  `isolated` option), shared with no DuckDB connection of the app.
- DuckDB is the one the DuckDB driver uses, downloaded on first use like
  opening a DuckDB connection (the driver too, in builds that download
  drivers).
- **Sandbox.** Before anything is loaded, the session switches off external
  access, extension autoloading and installing, community extensions and
  spilling to a temp directory, then locks the configuration, and checks
  that it holds (the cell refuses to run otherwise). The one extension it
  may have is `spatial`, loaded before that when a cell uses spatial
  functions (see [Geographic files](#geographic-files-st_read)). The cell's SQL can't
  use it to write files or reach the network on the user's behalf:
  `SELECT * FROM 'file.csv'`, `read_text('/…')`, `COPY … TO`, `ATTACH`,
  `EXPORT DATABASE`, `INSTALL` / `LOAD`, URLs and `SET` are all refused.
  The readers of [Reading local files](#reading-local-files) are the one
  way files come in, through a separate reader session. Data goes in
  through the driver's bulk load (DuckDB's appender), never through SQL
  that reads files in this session. No connection names, hosts or
  credentials reach it, and the `.ipynb` stores only the cell names and
  `{ "dataframes": true }`.

## Writing a DataFrame to a table

A **write cell** writes a cell's whole result (its DataFrame) into a table
of any engine that writes. The case it is for: a notebook combines SQL
Server, PostgreSQL and a CSV in DataFrame SQL cells and, at the end, updates
a table with the result, the same way every month.

- **Adding one:** **+ Escritura** next to **+ SQL** / **+ Texto**, or
  **Escribir en una tabla…** in a SQL cell's menu (and in its result's
  summary bar), which adds a write cell under it already set to that cell's
  DataFrame.
- **The form** (not code):
  - **DataFrame:** one of the named SQL cells above it.
  - **Destino:** a connection and database, or a project environment,
    picked like a cell's target (read-only connections aren't offered);
    the schema, where the engine has them, and the table: an existing one
    or **Crear tabla nueva** (it must not exist; its columns come from the
    DataFrame in the engine's types, with the key as its primary key).
  - **Modo:** **Insertar**, **Actualizar por clave** (rows whose key isn't
    in the table are left out), **Insertar o actualizar** (upsert) or
    **Reemplazar contenido** (the table's rows are deleted, then the
    DataFrame's inserted). Only the modes the target's engine does can be
    picked.
  - **Clave:** the target columns that identify a row (update and upsert).
  - **Columnas:** each DataFrame column and the table column it goes to,
    paired by name (ignoring case) until changed; **— no se escribe —**
    leaves one out, and the table's columns nothing goes into are listed
    (they keep their default).
  - **Vista previa:** loads the DataFrame and checks everything, writing
    nothing: the row count, each column's type on both sides (a value that
    changes kind is marked), warnings (texts longer than the column,
    decimals rounded, fractions or time zones dropped), the `CREATE TABLE`
    of a new table and the first rows.
- **The rows written are the whole DataFrame, never the saved preview.** It
  is loaded into the notebook's DuckDB database with the same rules as a
  DataFrame SQL cell (the source's complete run in this session, or its
  query run again read-only; local files only with their approval). A
  DataFrame that got cut at the row cap on the way (it or any table it
  comes from) is not written: the cell says so (raise the cap in
  **Settings › Notebooks**, or filter before). For update and upsert, a key
  repeated or empty in the DataFrame stops it before anything is written.
- **Every run asks**, in the app: *"Se van a escribir N filas en:"* with the
  connection › database › table, the mode, the key and the columns, whether
  it is one transaction, and the warnings. **Reemplazar contenido** says
  that the table's rows are deleted first. A **production** connection
  (tagged `prod`, or a project environment with `confirm_run`) is shown in
  red and asks to type the table's name, as DBine's other destructive
  dialogs do; an environment with `confirm_run` also asks its own
  confirmation first, as SQL cells do. **Run all** stops at each write cell
  with this dialog: **No escribir esta** skips that one and goes on,
  **Cancelar todo** stops the run. Nothing is ever written without it.
- **Enforced in the backend:** the preview returns a one-time token bound to
  the tab, the cell, the target (connection, database, schema, table, new
  or not), the mode, the keys, the mapping and the row count; the write
  runs only with that token, once, for 15 minutes, and refuses it when any
  of those changed or the DataFrame no longer has those rows. A read-only
  connection is refused there too (its session refuses writes as well). No
  MCP tool or assistant tool reaches it (the assistant never writes;
  MCP's own `execute` takes one statement and asks in DBine).
- **How it writes** (`dbine_driver::write`):
  - SQL engines: the rows are bulk loaded (the engine's own bulk path:
    `INSERT BULK`, `COPY`, `LOAD DATA`, array DML, the appender…) into a
    **staging table** that copies the target's column types (a temporary
    table: `#temp` in SQL Server, `TEMPORARY` in PostgreSQL and MySQL, a
    global temporary table in Oracle; DuckDB and SQLite use a table next to
    the target or `TEMP`), then applied in **one transaction**: `INSERT …
    SELECT`; the engine's update from a join (`UPDATE … FROM`, `UPDATE …
    JOIN`, `MERGE … WHEN MATCHED`) of the non-key columns; for upsert that
    update and then `INSERT … SELECT … WHERE NOT EXISTS` of the missing
    keys (two statements instead of one `MERGE` / `ON CONFLICT`: no unique
    index is needed on the key, and updated and inserted are counted
    apart); for replace `DELETE FROM` (never `TRUNCATE`: not transactional
    everywhere, and refused by foreign keys) and the insert. A failure
    rolls it all back: the table is as it was. The staging table is dropped
    afterwards. Writing an identity column in SQL Server turns
    `IDENTITY_INSERT` on around the inserts.
  - MongoDB (and FerretDB, DocumentDB): `insertMany`; `update` commands by
    key (the key fields as the filter, the others in `$set`, `upsert: true`
    for upsert); replace is `deleteMany({})` then the insert. There is no
    transaction: what was written before a failure stays, and the result
    says how much.
  - Engines without these modes insert through their bulk load, without a
    transaction (see `docs/engine-support.md`).
  - Values always travel through the bulk or parameter path; the
    statements carry only identifiers, quoted for the dialect.
- **Its result:** rows inserted, updated and deleted, when, how long and
  where, and the warnings. A failure says whether nothing was written (the
  transaction rolled back) or how many rows were and stay. **Cancel**
  stops it; a write cut short closes its session, so the server rolls the
  open transaction back.
- **In a scheduled task** ([scheduled-tasks.md](scheduled-tasks.md)), a
  write cell runs only when the step authorizes that exact cell, target and
  mode (with its keys); the authorization replaces the dialog, nothing else
  changes. Otherwise the step fails at that cell and nothing is written.
- **Python export:** a write cell becomes a commented `df.to_sql(…)` with a
  note: running the exported notebook writes nothing.
- **In the file**, the cell is a code cell with `metadata.dbine.kind:
  "write"` and its definition: the DataFrame's name, the target by
  connection **name** (or environment) and database, like any cell's
  (never ids, hosts or credentials), the schema, the table, whether it
  creates it, the mode, the keys and the mapping. Its `source` is a short
  readable description for other programs. The last run is kept only as a
  summary (`write_result`: the counts, when, how long, where, and the
  error), also as a text output; never rows. A mode a file names that
  DBine doesn't know reads as **Insertar**.

```json
{
  "cell_type": "code",
  "metadata": {
    "language": "dbine-write",
    "dbine": {
      "kind": "write",
      "write": {
        "source": "resumen",
        "target": { "connection": "Ventas prod", "database": "ventas" },
        "schema": "dbo",
        "table": "resumen_mensual",
        "create": false,
        "mode": "upsert",
        "keys": ["id"],
        "mapping": [{ "source": "id", "target": "id" }, { "source": "total", "target": "importe" }]
      },
      "write_result": { "inserted": 96, "updated": 1152, "deleted": 0, "ran_at": "2026-10-18T14:32:05Z", "duration_ms": 1840, "target_label": "Ventas prod › ventas › dbo.resumen_mensual", "error": null }
    }
  },
  "source": ["-- DBine write cell (edit it in DBine)\n", "-- DataFrame: resumen\n", "…"],
  "outputs": [{ "output_type": "stream", "name": "stdout", "text": ["96 inserted, 1152 updated · …\n"] }]
}
```

## The result and what is saved

On screen, a cell's result is the editor's results grid (paged, with sorting,
filtering, the chart view and export). **The file never stores a whole
result, only a preview:**

| | Default | Changeable |
|---|---|---|
| Rows per cell | **100**, shared by the cell's statements (the first ones first) | Settings › Notebooks |
| Size per cell | 1 MB for all the cell's outputs (a preview over it is cut, even with fewer rows) | Settings › Notebooks |

- The size covers everything the cell stores: the summaries, the rows, the
  server's messages, the error and the chart's points. When it doesn't all
  fit, the summaries and at least the first 2,000 characters of the error
  stay, then the rest of the error, the rows (the last ones go first), the
  messages and the chart's points, in that order, while they fit; cut text
  ends in "…" and the result is marked as cut.

- **Every result keeps its summary**, even with no rows: total rows, when it
  ran, how long it took, the target and whether the preview was cut. Someone
  reading the file sees *"100 of 100,000,000 rows · 18/10 14:32 · 41 s ·
  Ventas prod › ventas"*.
- A **chart** keeps its settings and the data it plots only when that data
  is **aggregated** (sum, average, count, min, max): at most the cell's rows
  per series, long labels cut like cell values, and only if they fit in the
  size per cell. A scatter or a chart without aggregation plots the raw
  rows, so only its settings are kept. Changing a chart on a stored preview
  drops the stored points (they'd no longer match); the next run brings new
  ones.
- **Per cell, "Don't save result"**, for sensitive queries: the summary is
  kept, the rows aren't.
- **Per notebook, "Save without results"**: the file keeps queries, text and
  summaries only. It's meant for sharing and for clean diffs in git.
- **In a project linked to git it is on by default**, because those files end
  up in a shared repository: results are saved only if that notebook turns it
  off. This applies to new notebooks and to notebooks from elsewhere that
  don't say (no `metadata.dbine.save_results`); a project whose folder isn't
  a git repository saves previews by default. Outside projects, previews are
  saved by default.
- The first time a notebook with results is saved, DBine says: *"This file
  includes a preview of the data you queried."*

## AI

It follows the assistant's rules ([ai-assistant.md](ai-assistant.md)): the
AI proposes, the user runs.

- **Explain this result**: asks the assistant about the cell's query and
  result (the SQL, the column names and types, the row count, whether the
  preview was cut and the error; **never row values**, as everywhere else).
  The answer comes in the assistant's panel, and **Insert as text** adds it
  as a draft text cell under the active cell.
- **Propose the next cell**: a cell built from the notebook so far,
  inserted but not run.
- **Summarise it**: when a preview was cut, the cell offers *"Do you want it
  grouped?"*, and the AI proposes an aggregated query in a new cell, not run.
- The assistant gets the **active cell's** connection and database (each
  cell may run elsewhere), and the prompts follow its engine: SQL engines
  are asked for SQL (`GROUP BY`…), the others (Redis, MongoDB, Cypher…) in
  their own language.

## Notebooks from elsewhere

- **Azure Data Studio** (SQL kernel): SQL and Markdown cells are taken as
  they are, and each cell asks for its target the first time.
- **Jupyter with `%%sql`** (jupysql, ipython-sql): those cells become SQL
  cells, without the magic line, and the connection string in them is
  **ignored** (a target is chosen instead).
- **Other cells** (Python, R…) are shown read-only and greyed out, with their
  stored output. DBine never runs them, and keeps them unchanged when saving.

## Export

- **HTML** (one self-contained page) and **Markdown**, with the previews the
  file would keep (the limits in Settings › Notebooks).
- The result of one cell, in full, through the normal export (CSV, Excel,
  Parquet…), which runs the query again: only for a cell that ran in this
  session (exactly the query that ran). A preview read from the file exports
  just its rows.
- **Python notebook** (see below).

### Export as a Python notebook

**Exportar › Notebook de Python (.ipynb)** writes a new Jupyter notebook (kernel
`python3`) that reproduces this one outside DBine, saved as
`<name> (Python).ipynb`:

- **First, a Markdown cell** with what to do before running it: the
  `pip install` line with the exact packages the notebook needs, and the
  environment variables to set, one per secret (`DBINE_VENTAS_PG_PASSWORD`,
  `DBINE_LAKE_TOKEN`…), named after the connection. It also says what to set
  up by hand: an SSH tunnel (with the `ssh -L` line), cloud sign-in (AWS
  IAM, Google Cloud, Entra ID, key pairs), a secret manager, extra ODBC
  attributes.
- **Then a setup cell**: the imports and one client per connection and
  database the cells use, built from the connection's non-secret fields
  (host, port, database, user and a few named options such as Oracle's
  service or Snowflake's warehouse) with the password read from its
  variable: `password=os.environ["DBINE_VENTAS_PG_PASSWORD"]`.
- **Each SQL cell** reads its result into a pandas DataFrame named after the
  cell, and shows it: `ventas = pd.read_sql(text("""…"""), engine_ventas_pg)`
  (SQLAlchemy's `text()`; a `:word` that isn't a parameter is escaped so it
  stays as written). Other engines use their own client's call (below).
- **Each DataFrame SQL cell** becomes `cruce = duckdb.sql("""…""").df()`:
  DuckDB finds the DataFrames above by their names (its replacement scans
  of pandas variables), and the file readers (`read_csv`, `st_read`,
  `sqlite_scan`, `read_xlsx`…) are kept as written, paths included; the
  extensions they need are loaded in the setup cell. A relative path is read
  from the folder the Python notebook runs in, so it goes next to the
  original. A cell name that is a Python keyword or a name the code uses
  (`pd`, `os`…) gets `_df` and is registered under its name
  (`duckdb.register("pd", pd_df)`).
- **Text cells** stay Markdown cells (with their attachments); cells from
  other notebooks in Python are kept, others are kept as comments.
- **Charts**: a plain chart (bars, lines, areas, scatter or pie, with no
  split) becomes a pandas plot (matplotlib) under its cell; the other kinds
  are left out.
- **Results** are left out (a clean notebook) unless **Incluir vistas
  previas** is checked: then each cell's stored preview is its output, with
  the limits in Settings › Notebooks.
- **[Parameters](#parameters)**: a first cell tagged `parameters`
  (papermill) with their defaults (`None` without one), and each cell's
  `{{name}}` as a `:name` bind with `params=` in the SQLAlchemy queries.
  A cell on another client (MongoDB, Redis, DuckDB…) that uses one is left
  commented, saying so: binding them there is a pending item. Write
  cells, when there are any, are to become a commented `to_sql(…)` with a
  warning, never run by default.
- A cell whose target isn't a connection on this computer, or whose text
  can't be mapped, is a **commented cell** with a `TODO` that says why and
  its original text.

**Secrets never go out.** The export reads each connection's non-secret
fields only: passwords, tokens, keys, connection strings, the SSH tunnel's
credentials, secret-manager references and extra ODBC attributes are never
copied, a host URL loses its user, password and query string, and option
values that look like credentials are dropped. A test exports a notebook on
every driver with secrets in every field and checks none appears.

The results can differ from DBine's: the Python code reads every row (DBine
caps them), with each client's own types.

#### Per engine

| Engines | Python | Cells |
|---|---|---|
| PostgreSQL and its family (AlloyDB, Aurora, Cloud SQL, CockroachDB, CrateDB, Denodo, EDB, Fujitsu, Greenplum, Cloudberry, Greengage, H2, Kingbase, Materialize, openGauss, Redshift, RisingWave, TimescaleDB, Yellowbrick, YugabyteDB) | SQLAlchemy `postgresql+psycopg` (`psycopg[binary]`) | `pd.read_sql` |
| Aurora DSQL | `postgresql+psycopg`, with an IAM token in the variable | `pd.read_sql` |
| MySQL and its family (MariaDB, Aurora, Cloud SQL, TiDB, SingleStore, OceanBase, Doris, StarRocks, VeloDB, Databend, GreptimeDB, Manticore) | `mysql+pymysql` (`pymysql`) | `pd.read_sql` |
| SQL Server, Azure SQL, Fabric, Babelfish | `mssql+pymssql` (`pymssql`) | `pd.read_sql` |
| Oracle, Oracle Autonomous Database | `oracle+oracledb` (`oracledb`); service, SID, descriptor or wallet | `pd.read_sql` |
| SQLite | `sqlite` (no package) | `pd.read_sql` |
| libSQL / Turso | `sqlite+libsql` (`sqlalchemy-libsql`), auth token in a variable | `pd.read_sql` |
| DuckDB | `duckdb.connect(…, read_only=True)` | `.sql(…).df()` |
| Snowflake | `snowflake` (`snowflake-sqlalchemy`), PAT or key-pair file | `pd.read_sql` |
| BigQuery | `bigquery` (`sqlalchemy-bigquery`), Google credentials | `pd.read_sql` |
| Spanner | `spanner+spanner` (`sqlalchemy-spanner`) | `pd.read_sql` |
| Athena | `awsathena+rest` (`pyathena[sqlalchemy]`), AWS credentials | `pd.read_sql` |
| Databricks, Azure Databricks | `databricks` (`databricks-sqlalchemy`), personal access token | `pd.read_sql` |
| Trino, Starburst / Presto | `trino` (`trino[sqlalchemy]`) / `presto` (`pyhive[presto]`) | `pd.read_sql` |
| SAP HANA | `hana+hdbcli` (`sqlalchemy-hana`, `hdbcli`) | `pd.read_sql` |
| Firebird | `firebird+firebird` (`sqlalchemy-firebird`) | `pd.read_sql` |
| Drill | `drill+sadrill` (`sqlalchemy-drill`) | `pd.read_sql` |
| Phoenix, Avatica | `phoenix` (`phoenixdb`) | `pd.read_sql` |
| TDengine | `taosrest` (`taospy`) | `pd.read_sql` |
| Db2 (LUW, i, z/OS) | `db2+ibm_db` (`ibm-db-sa`) | `pd.read_sql` |
| Teradata | `teradatasql` (`teradatasqlalchemy`) | `pd.read_sql` |
| Vertica | `vertica+vertica_python` (`sqlalchemy-vertica-python`) | `pd.read_sql` |
| Exasol | `exa+websocket` (`sqlalchemy-exasol`) | `pd.read_sql` |
| Hive, Spark, Kyuubi / Impala | `hive` (`pyhive[hive_pure_sasl]`) / `impala` (`impyla`) | `pd.read_sql` |
| MonetDB | `monetdb` (`sqlalchemy-monetdb`) | `pd.read_sql` |
| InterSystems IRIS, Caché | `iris` (`sqlalchemy-iris`) | `pd.read_sql` |
| The other ODBC engines (Access, Altibase, CUBRID, Dameng, dBase, GBase 8s, HeavyDB, Ignite, Informix, Ingres, Machbase, MaxDB, Mimer, Netezza, NetSuite, NuoDB, Ocient, OpenEdge, SQL Anywhere, SQream, Sybase, Virtuoso, Zen, generic ODBC with a DSN) | `pyodbc.connect` with the ODBC driver's name (or the DSN) | `pd.read_sql` |
| ClickHouse, Timeplus | `clickhouse_connect.get_client` (`clickhouse-connect`) | `.query_df(…)` |
| MongoDB, DocumentDB, FerretDB | `MongoClient` (`pymongo`) | `db.<c>.find(…)` (with `sort`, `skip`, `limit`) and `.aggregate([…])` with strict JSON arguments become pymongo calls (extended JSON through `bson.json_util`); other shell syntax is commented |
| Redis, Valkey, Dragonfly | `redis.Redis` (`redis`) | each command line with `execute_command`; the last reply as the DataFrame |
| Cassandra, ScyllaDB, Keyspaces | `Cluster(…).connect()` (`cassandra-driver`) | `session.execute(…)` |
| Neo4j, Memgraph, Neptune | `GraphDatabase.driver` (`neo4j`) | `execute_query(…, result_transformer_=neo4j.Result.to_df)` |
| InfluxDB 1 / 2 / 3 | `influxdb` / `influxdb-client` / `influxdb3-python` | InfluxQL, Flux (`query_data_frame`) or SQL |
| Elasticsearch, OpenSearch, Open Distro, Solr, CouchDB | `requests` | one read request (`GET`, or `POST` to `_search`, `_find`, `_sql`, `select`…) with a strict JSON body, and Elasticsearch/OpenSearch SQL lines; requests that change data are commented |
| DynamoDB | `boto3` | PartiQL with `execute_statement`, every page |
| Couchbase | `couchbase` | `cluster.query(…)` |
| Flight SQL, Dremio | `adbc-driver-flightsql` (Dremio on its Arrow Flight port) | `cursor.fetch_df()` |
| Cosmos DB | `CosmosClient` (`azure-cosmos`) | **commented**: a query runs on one container, which the cell doesn't name |
| DuckDB over a folder of files | — | **commented**: read the files in a DataFrame SQL cell |
| IoTDB, TimechoDB | — | **commented**: DBine uses the REST API; the Python client uses another port and API |
| ksqlDB | — | **commented**: no maintained Python client |
| OrientDB | — | **commented**: no maintained Python client for current versions |
| etcd | — | **commented**: DBine's commands have no direct Python equivalent |
| Cloudera Data Warehouse | — | **commented**: its HTTP transport has to be set up by hand |

## Where notebooks live

- **File › New notebook**, the command palette and the Welcome view. An
  untitled notebook keeps a draft until it's saved, so restarting DBine
  doesn't lose it.
- **Projects**: a `.ipynb` in a project opens as a notebook, and the project
  tree's menu has **New notebook…**. It's saved with the project's conflict
  check, like any other file there.
- The Library (scripts) doesn't hold notebooks.
- Opening a `.ipynb` from the system (double click, drag and drop, *Open
  with*) opens it in a notebook tab: as the project's file when it is inside
  a linked project's folder, else as a standalone file. Files over 64 MB
  aren't opened.

## Version 1 and later

**Version 1 (this one):** the file format, SQL and text cells, targets (connection,
database or project environment), running, previews with their limits, the
three AI actions, notebooks from elsewhere, export to HTML, Markdown and a
Python notebook,
[DataFrame SQL cells](#dataframe-sql-cells-cross-engine): cells that join the
results of cells on different engines and local files, and
[write cells](#writing-a-dataframe-to-a-table) that write a DataFrame into a
table, and [parameters](#parameters).

**Run a notebook** as a step of a [scheduled task](scheduled-tasks.md#run-a-notebook),
followed by **Send an email**, turns a notebook into a periodic report: it
runs every cell with DBine closed, saves the executed copy and exports it
(tables, without charts), and asks nothing: files need **Permitir siempre**
and environments with confirmation need the step's opt-in.

**Later:**
- **Run notebook** as a step of a [scheduled task](scheduled-tasks.md),
  followed by **Send an email**, which turns a notebook into a periodic
  report.
