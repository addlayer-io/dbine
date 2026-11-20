# Data compare

Compares the rows of two tables and builds the script that makes one match
the other. The tables can be on the same connection, on different
connections or on different engines. It complements
[schema compare](schema-compare.md).

It also compares the whole results of **two queries**, on any connections
and engines (a Power BI measure against the `SUM` in its source database,
for instance), read-only: see [Comparing two queries](#comparing-two-queries).

## How to use it

- Right-click a table › **Compare data…**: opens a tab with that table on the
  left. It is also in the database menu.
- Choose the right-hand table (connection, database and table). If the other
  database has a table with the same name, it is proposed automatically.
- **Compare** reads both tables whole, however big. While it runs it shows
  the rows read on each side, the phase (reading, comparing, bringing the
  different rows), the time elapsed and **Cancel**, which stops both reads
  and the work on disk. Then it shows how many rows were compared on each
  side and:
  - the **same** rows: the count, and their list with **Include equal
    rows** (see [The equal rows](#the-equal-rows));
  - the **different** ones, with each differing value marked (the left one on
    top, the right one below);
  - those **only on the left** and **only on the right**.
- On each row of the three views there are two arrows, as in schema compare:
  **→** makes the right-hand row match the left (updates the different one,
  inserts the missing one or deletes the extra one) and **←** does the same
  toward the left. Another click on the active arrow leaves it unchosen. At
  the bottom of each view, **← All**, **All →** and **None** choose all rows
  of that view, including those not shown (the view loads up to 2,000).
- **Make the right match →** / **← Make the left match** mark in one step the
  different rows and those missing on that side. Those that would have to be
  deleted aren't marked automatically: they are chosen one by one or with
  **All** in their view.
- With what is chosen, the sync button builds **one script for each side that
  changes**, in tabs. It can be copied, opened in a query or run. **Run**
  runs the left one first and then the right one, and stops at the first
  error (what already ran isn't undone). Nothing runs without that click. If
  it succeeds, the comparison is run again.

## How it compares

- Rows are paired by the **primary key** of the left table. If it has none,
  or another is wanted, the key columns are chosen after the first
  comparison.
- The columns both tables have are compared (by name, case-insensitive). Those
  that only one has are listed and not compared.
- Values are compared by what they are worth, not by how each engine returns
  them: `1`, `1.0` and `"1.00"` are equal; a date with `T` or with a space,
  with or without trailing zeros in its fraction, with `Z` or `+00:00`,
  too. In a boolean column, PostgreSQL's `t` equals another engine's `true` or
  `1`. A text with leading zeros (`"007"`) is compared as text.
- **Whole tables.** Each side is read whole with its driver's batched
  read, the same one table copies use (`read_batches`): each engine pages
  the way it can (a server cursor, Elasticsearch's point in time, Solr's
  cursor, Cassandra's paging…), never one query with a huge `LIMIT`. Both
  sides are read at once, each on its own read-only session, only the
  compared columns. No order is needed, so it doesn't matter how each engine
  sorts text or whether it can sort at all. The totals are exact over the
  whole tables: `same + different + only left`, plus the left's repeated
  keys, is the left's row count; likewise on the right. Every row of one side
  is confirmed absent on the other, so all of them can be inserted or
  deleted. Checked live on SQL Server, PostgreSQL, MySQL, ClickHouse,
  Cassandra, MongoDB, Elasticsearch and Solr; the engines where the read
  isn't batched are in [engine-support.md](engine-support.md#data-compare).
- **The compared columns come first.** They are taken from each table's
  catalog (from a one-row read where the catalog lists none) before
  reading, so each side's rows are cut to them as they arrive, whatever the
  other side is doing: a side whose first rows are slow to come (a cloud
  warehouse running its job, a view that sorts) doesn't make the other one
  pile up rows.
- **Small tables in memory.** While both sides are under 250,000 rows and
  256 MB (estimated from the values read), their rows stay in memory and are
  compared there.
- **Big tables in a local DuckDB.** Once a side passes 250,000 rows or
  256 MB, the rows of both sides go to a DuckDB database on disk, made for
  that comparison only. It starts opening when a side reaches half of
  either, so the reads rarely wait for it; while one does, the progress says
  "Preparando la base local…". It lives in its own folder next to the app's
  data (`<app config>/compare/<folder>/`), encrypted with a key made for that
  comparison and kept in memory only (never in the vault, never in a log),
  with its WAL and spilled temp files encrypted too (checked by a test that
  makes DuckDB spill), extensions off and its configuration locked: the same
  setup as the [results store](results-store.md), checked the same way before
  anything is written. DuckDB takes at most 512 MB of memory and works on
  its temp files past it. Each row goes as a 128-bit hash of its canonical
  key, a 128-bit hash of its canonical values (equal rows have equal hashes)
  and the row itself; the comparison is one join in DuckDB on the key's
  hash, so a key as long as a long text column fits that memory, and the
  first differences in key order are picked a few characters of the key at a
  time, never sorting whole keys; only the rows that differ come back.
  The reads wait while DuckDB is behind (at most 64 MB queued), and that wait
  is on the task that reads, never on a driver host's reader thread, so
  comparing DuckDB or file connections, whose host also runs the local
  DuckDB, doesn't block it. The folder is deleted when the comparison ends,
  whether it finished, failed or was cancelled, and what a crash left is
  deleted the next time DBine starts. DuckDB is downloaded on first use; if
  it can't be opened (offline the first time…), a comparison that needs it
  says the table is too big to compare in memory and why DuckDB didn't open
  (one that stays in memory doesn't need it).
- **Up to 1,000,000 differences.** The screen and the script cover up to
  1,000,000 differing rows in all, the first ones in key order (numbers by
  value, text without case, NULL last). Past that the totals are still
  exact, and the result says how many differ: after syncing, comparing again
  brings the next ones.
- Up to 2,000 rows of each type are shown; the totals and the script include
  all of them (up to the 1,000,000 above).
- If a key repeats on one side, the first row read is compared once and the
  result says so.

## Vectors

Vector columns compare by value and show their first numbers and dimensions.
Details: [vectors.md](vectors.md#data-compare).

## The equal rows

The equal rows are always counted. To see them too, check **Include equal
rows** next to **Compare** (tables or queries), then compare: the
count of equal rows becomes a list of its own, next to the differences.

- It lists the first **100,000** equal rows in key order, with the left
  side's values (or the right's, in a query compare, for a row only that
  side has that counts as the same under NULL as BLANK). Past that the
  count is still exact and the result says how many are listed.
- It sorts and filters like the differences, over all the rows listed.
- It has no arrows and never goes into a script: there's nothing to sync.
- Listing them takes longer and more memory on big tables, so it's off by
  default, and the tab remembers the choice. On the local DuckDB, the equal
  rows are picked after the differences, the same way and in the same
  order.

## Narrowing a table compare with a query

**Switch to queries**, next to **Compare** in a table compare, turns it
into a [compare of two queries](#comparing-two-queries): each side gets the
query that reads its table whole, in that engine's language (the driver's
own browse query without its limit: `SELECT * FROM [dbo].[Orders]`,
`db.orders.find({})`…), on the same connection and database, and the
table compare's key becomes the key in the column table once the columns
are read. Add a `WHERE` (or the engine's own filter) on either side, or
both, and **Compare**: only those rows are compared.

A query compare is read-only, so the result has no arrows and no script.
If the **Queries** mode already had queries, it asks before replacing
them.

## Sorting and filtering the differences

Every column of the differences can be sorted and filtered, over **all** the
rows of that kind, not only the 2,000 on screen.

- **Sort:** a click on a column's name goes ascending, descending, none.
  Shift+click adds another key. Numbers sort by value even when they come as
  text, and NULL goes last.
- **Filter:** the three dots on a column offer its order and a condition by its
  type: equal, different, greater, less, contains, starts with, NULL… Several
  columns add up. Values are compared by what they are worth, and a date against
  a whole day takes all its hours.
- **Changed rows:** a filter passes if either side does, and sorting goes by the
  left side and then the right. "Only rows where this column changed" keeps the
  rows that differ in that column.
- **With a filter on:** the "All" buttons mark only the rows that pass, and
  "Make the right match" is off: it would also take the rows filtered out.

`data_compare_rows { id, kind, sort, filters, changed_in }` answers the first
2,000 rows that pass, how many pass, and, with a filter, every row's place in
its kind's list, which is what the script's choices count by.

## Exporting a list

**Export…**, next to the lists, writes the list on screen to a file. That list can be the different rows, the rows only on one side, or the equal rows when they're listed. It uses the result export's dialog and formats: CSV (comma, semicolon, Excel), TSV, Excel, JSON, JSON Lines, SQL INSERT and XML.

- It writes **every row of the list that passes its filters, in its order**, not only the 2,000 on screen, straight from the comparison DBine keeps.
- If the comparison listed only the first rows (more than 1,000,000 differences, or more than 100,000 equal rows), the dialog says so and the file has those first rows.
- A one-side or equal row is written as it is, with the compared columns.
- A **different** row writes its key columns once, then each other compared column twice, `name (Left)` and `name (Right)`, then a last column, **Different columns**, with the names of those that differ. The left and right words follow the interface language. In a compare of two queries the right column keeps its own name.
- An SQL export writes string literals the way the left side's engine reads them.

`data_compare_export { id, kind, sort, filters, changed_in, path, options, labels }` writes it. `kind`, `sort`, `filters` and `changed_in` are as in `data_compare_rows`, `options` is the result export's, and `labels` gives the words the column names take: `{ left, right, differs }`. It returns `{ rows, elapsed_ms }`.

## The script

It is written by the target's driver, in its language (`insert_script`,
`update_script` and `delete_script` of the contract): SQL on SQL engines,
`insertMany`/`updateOne`/`deleteOne` in MongoDB, etc. The order is delete,
update and insert.

- Updates change only the differing columns.
- Each value is written the way the target column takes it, by its type: a
  number that came as text (PostgreSQL answers them so) goes as a number, as
  long as that keeps it exact; past 15 significant digits it stays as text. A
  boolean goes as a boolean. Where the target doesn't say (an empty collection),
  the source's type decides. A text column keeps its text, even `"007"`.
- A delete always carries the key: a `DELETE` without `WHERE` is never
  generated.
- Rows inserted with the value of an identity or auto-increment column carry
  what the engine needs (`data_load_wrap`): in SQL Server,
  `SET IDENTITY_INSERT … ON` before and `OFF` after (without that, error
  544); in PostgreSQL, the sequence is moved past the last value, so the next
  insert doesn't collide with a copied id.
- Read-only connections reject the script when it is run.

## With AI

Over the differences there's a bar to use the AI with. The AI never sees a row's
values: it gets counts, the columns' names and types, and what you type.

- **Explain differences** computes, over all the differences and not only the
  rows shown, a profile per column:
  - how many rows differ, and in how many it's the only difference;
  - NULL on one side only, and text that differs only in case or spaces;
  - for numbers and dates, the most frequent gap between the two sides and its
    range ("+3 h in 1,180 rows");
  - which columns differ in the same rows;
  - with a one-column key of numbers or dates, how many of the rows a side
    lacks come after the other side's last key (a sync running late).

  The AI tells the likely cause from that (time zone, rounding, an update date)
  and what can be ignored.
- **Filter with AI** turns a request ("the ones that differ only in price, the
  most expensive first") into the column filters, **Differs in** and the sort
  described above. They show as chips and can be removed one by one. What
  these filters can't express (comparing the left value with the right one, a
  percentage of change) the AI says in a note under the bar.
- **Suggest key**, next to the key, for a table without a primary key:
  - first a unique index whose columns don't take NULL;
  - if there's none, with an AI set up, the AI's choice from the columns'
    names and types.

  The comparison says if the key repeats.

## Comparing two queries

Besides two tables, a data compare can compare the results of two
**queries**: a Power BI measure against the `SUM` in the database it comes
from, a report query on two servers, a Parquet file against a warehouse.
Each side is any connection (and database) and a query in that engine's
language: SQL, DAX, a MongoDB `find`, a Redis command…

**Where.**

- In the data compare tab, the **Tablas / Consultas** switch. In **Consultas**
  each side has a connection, a database and a query box.
- In a query editor, **Comparar con…** in the editor's menu (the selection,
  or else the statement at the cursor), and in a result tab's menu (that
  result's query; off for the result of a run on several databases). Both
  open a compare with that query on the left and the right side empty:
  choose the right side's connection and write its query. The statement's
  `:name` parameters are asked for first, as running it does.

**Columns and key.**

- **Leer columnas** (or the first **Compare**, and again whenever a query
  changes) runs each query until its first row to read its columns, and
  stops there. If the engine answers an error after that first row (one
  that cuts the result at the rows asked for), the columns read stand; the
  compare runs the query again and shows any real error.
- The columns are **paired** by name, then by their bare name
  (`Date[Year]` with `year`, `[Total Sales]` with `total_sales`), then in
  order for those left over. The pairs and the key are shown in a column
  table and can be changed; a pair can be switched off (**No se compara**).
  Right-hand columns with no pair are listed and not compared. When a query
  changes, the user's pairs and key are kept where the columns still exist.
- The **key** is preselected from the first of these that applies, and a
  line under the column table says which one it was:
  1. **Switch to queries:** the key of the table compare.
  2. **A DAX side:** its `Table[Column]` columns; the measures are compared.
  3. **A query that reads one table** (`SELECT … FROM t [WHERE …]
     [ORDER BY …]`, no join, union, subquery or grouping): that table's
     primary key, else a unique index where the engine reports them.
  4. **A column that looks like an id** (`id`, `code`, `…_id`, `…Key`…).
  5. **A query that groups** (`GROUP BY`): the columns that describe rather
     than measure, or all but the last when none looks like a measure.

  Otherwise nothing is marked and the line asks for the key: a table's rows
  give no reliable hint, and guessing almost every column would only find
  missing rows, never changed ones. At least one key column is needed. If the key repeats, the result
  says so and hints that the key may be missing a column.
- A result with two columns of the same name (case-insensitive, such as
  `SELECT *` over a join) is refused, both when reading its columns and when
  comparing: give them aliases. A DAX result names its columns as DAX does
  (`'Date'[Year]` comes back as `Date[Year]`, a measure or extension column
  as `[Sales]`); rename them in the query (`SELECTCOLUMNS`, or the names given
  in `SUMMARIZECOLUMNS`) when the bare names don't pair.

**Options.**

- **Tolerance on numbers:** an *absolute* difference (±0.01) and/or a
  *relative* one, as a percentage of the larger number. Two numbers within
  either are the same. It is for a measure against a `SUM` that differs by
  rounding. The key never takes a tolerance.
- **NULL igual a vacío (BLANK):** NULL equals a blank value (0, an empty
  text or false), as DAX's BLANK does; a row only one side has counts as the
  same when all its compared non-key values are blank (DAX's
  `SUMMARIZECOLUMNS` leaves out the rows whose measures are all BLANK, where
  SQL over a date table returns them with NULL or 0). A row with any value
  that isn't blank is still only on its side, and so is every row when only
  key columns are compared. It is switched on by itself when a side is a
  Power BI connection.
- **Sin distinguir mayúsculas** and **Sin espacios al principio y al final**
  for text, keys included.

**Whole results.**

- Both results are read whole, each on a read-only session of its own, never
  only the rows a grid shows. While both are under 250,000 rows and 256 MB
  they are compared in memory; past that they go to the same encrypted,
  temporary local DuckDB the table compare uses, with the same folder, key,
  cleanup and limits (see [How it compares](#how-it-compares)).
- Without a tolerance, two numbers are equal only when they are the same
  number digit by digit (`1`, `1.0` and `"1.00"` are); a tolerance compares
  them as doubles. A date equals a timestamp at its midnight, text compares
  exactly, and PostgreSQL's `t` and `f` equal another engine's `true` and
  `false` (or `1` and `0`), keys too.
- While the compare can't take a driver host's rows (the local DuckDB still
  opening, or downloading on first use), at most 64 MB of rows wait per side
  and the host is held until there is room. A `duckdb` or `duckdb_files`
  side shares its host with the local DuckDB and can't be held: if its result
  gets 512 MB ahead of the compare, the compare stops and says so (narrow the
  query, or compare the table or view).

**It refuses rather than compare partially.** A compare stops, with the
reason, when:

- the engine cut a result (an API's row cap, such as Power BI's
  `executeQueries`: 100,000 rows, 1,000,000 values or 15 MB), since the totals
  would be wrong;
- a query returns more than one result set, or none with columns;
- a paired column is missing from a result;
- a column name repeats (above).

**Results** are the table compare's: same, different (with the differing
columns), only on the left, only on the right, exact counts, repeated keys,
and [sorting and filtering](#sorting-and-filtering-the-differences) and
[the AI](#with-ai) over all the rows (the AI still never sees a value).

**Read-only.** A query isn't a table: there are no arrows and no sync
script, and `data_compare_script` refuses a query compare. The two queries
run on read-only sessions, so a statement that writes is refused (checked
live with six writes that return rows on PostgreSQL, SQL Server and
MongoDB: each was refused and nothing was written).

**Example: a Power BI measure against the source.** Left, the model's
connection:

```dax
EVALUATE
SUMMARIZECOLUMNS('Date'[Year], "Sales", [Total Sales])
```

Right, the SQL Server or PostgreSQL database the model is loaded from:

```sql
SELECT year, SUM(amount) AS sales FROM sales GROUP BY year
```

The columns pair by bare name (`Date[Year]` with `year`, `[Sales]` with
`sales`), the key is the year, and NULL equals blank is already on. With
**absolute** `0.01` the years whose total differs only by rounding are
"same"; the rest show as different with both values, and a year that only
one side has shows in its own list.

## Commands

- `data_compare { left, right, key, columns, run, equal }` → the totals, the
  sample rows, `duplicate_keys`, `diff_truncated` and `diff_limit` (more rows
  differ than the lists hold), `listed` (rows of each kind in the lists) and
  an `id`. With `equal: true`, also `equal` (the first equal rows) and
  `equal_limit` (how many are listed at most; 0 when not asked for), and
  `listed.equal`. `run` is the UI's id for that comparison. `limit`, which older UIs
  send, is ignored; `truncated_left` and `truncated_right` are always false
  and `ordered` always true.
- `data-compare-progress` (event) `{ run, side: "left" | "right", rows,
  phase: "reading" | "preparing" | "comparing" | "loading_differences" |
  "loading_equal", differences }`: about four times a second per side while it changes
  (`preparing`: a read waits while the local DuckDB opens).
- `data_compare_cancel { run }` stops that comparison: both reads (as
  `cancel_query` does, on its sessions `dcmp:<run>:left` and
  `dcmp:<run>:right`) and the DuckDB work. The comparison then fails as
  cancelled and deletes its folder.
- `data_compare_script { id, choices }` → one script for each side that
  changes: `{ connection_id, database, side: "left" | "right", script,
  inserts, updates, deletes }`. `choices` has `changed`, `only_left` and
  `only_right`; each carries `all` (`"left"`, `"right"` or `"none"`) and
  `rows`, the exceptions `[index, direction]` over the complete list of that
  view.
- `data_compare_query { left, right, columns, options, run }` and
  `data_compare_query_columns { side, run }` compare two queries and read
  one's columns ([`api-commands.md`](api-commands.md#data-compare-of-two-queries-data-comparemd)).
  Its progress event and `data_compare_cancel { run }` are the table
  compare's; cancelling also stops the column reads.
- `data_compare_explain { id, asker }` → `{ text }`, Markdown from the profile
  (`src-tauri/src/commands/data_compare_ai.rs`).
- `data_compare_ai_filter { id, kind, request, asker }` → `{ filters,
  changed_in, sort, note }`, checked against the comparison's columns.
- `data_compare_key_hint { left, asker | null }` → `{ key, by: "unique" |
  "ai" | "" }`.
