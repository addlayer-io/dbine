# Notebooks

A notebook is a document of **cells**, one under another: **SQL cells** that
run against a connection and keep their result under them, and **text cells**
(Markdown) that explain what is being done and why. It keeps a whole
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
  per run.
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
  the query row limit); **Calcular sobre todo el resultado** isn't offered
  for these cells (their result is already in memory: a `GROUP BY` in the
  cell does the same), and a full export exports the rows shown.

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
- **Local files only:** URLs (`http`, `https`, `s3`, `gs`, `azure`, `hf`…)
  are refused; nothing goes over the network.
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
  refused by DuckDB. Nothing is written to disk.
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
- **Content change notice** (it doesn't stop the run): each run keeps, for
  each file read, its size, modification time and SHA-256, per notebook.
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
three AI actions, notebooks from elsewhere, export to HTML and Markdown, and
[DataFrame SQL cells](#dataframe-sql-cells-cross-engine): cells that join the
results of cells on different engines and local files.

**Later:**
- **Run notebook** as a step of a [scheduled task](scheduled-tasks.md),
  followed by **Send an email**, which turns a notebook into a periodic
  report.
- **Export as a Python notebook**: each SQL cell becomes `pd.read_sql(…)`,
  with the connection as an environment variable and no password.
- Notebook **parameters** (values the cells use, asked for when running).
