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
- A cell can also run on **Notebook results** instead of a connection (see
  [Notebook results](#notebook-results-cross-engine)); the file stores that
  target as `{ "results": true }`, with no connection name.
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

## Notebook results (cross-engine)

A cell whose target is **Resultados del notebook** runs DuckDB SQL over the
results of the cells above it, each one a table named after its cell. One
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
  above it. A source that is itself on Notebook results is loaded as its
  query over the tables above it (`CREATE TABLE … AS`), so results cells can
  build on each other; it must be a single query.
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

### The DuckDB database

- One per notebook tab, **in memory only**, opened on the first results cell
  and closed with the tab: nothing is written to disk, and closing the tab
  drops every loaded table. It is a database of its own (the DuckDB driver's
  `isolated` option), shared with no DuckDB connection of the app.
- DuckDB is the one the DuckDB driver uses, downloaded on first use like
  opening a DuckDB connection (the driver too, in builds that download
  drivers).
- **Sandbox.** Before anything is loaded, the session switches off external
  access, extension autoloading and installing, community extensions and
  spilling to a temp directory, then locks the configuration, and checks
  that it holds (the cell refuses to run otherwise). A notebook from
  elsewhere can't use it to read or write files or reach the network on the
  user's behalf: `read_csv('/…')`, `SELECT * FROM 'file.csv'`, `COPY … TO`,
  `ATTACH`, `EXPORT DATABASE`, `INSTALL` / `LOAD`, URLs and `SET` are all
  refused. Data goes in through the driver's bulk load (DuckDB's appender),
  never through SQL that reads files. No connection names, hosts or
  credentials reach it, and the `.ipynb` stores only the cell names and
  `{ "results": true }`.

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
[Notebook results](#notebook-results-cross-engine): cells that join the
results of cells on different engines.

**Later:**
- **Run notebook** as a step of a [scheduled task](scheduled-tasks.md),
  followed by **Send an email**, which turns a notebook into a periodic
  report.
- **Export as a Python notebook**: each SQL cell becomes `pd.read_sql(…)`,
  with the connection as an environment variable and no password.
- Notebook **parameters** (values the cells use, asked for when running).
