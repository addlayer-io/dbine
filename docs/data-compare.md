# Data compare

Compares the rows of two tables and builds the script that makes one match
the other. The tables can be on the same connection, on different
connections or on different engines. It complements
[schema compare](schema-compare.md).

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
  - the **same** rows (only the count);
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
  its temp files past it. Each row goes as its canonical key, a 128-bit hash
  of its canonical values (equal rows have equal hashes) and the row itself;
  the comparison is one join in DuckDB; only the rows that differ come back.
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

## Commands

- `data_compare { left, right, key, columns, run }` → the totals, the sample
  rows, `duplicate_keys`, `diff_truncated` and `diff_limit` (more rows differ
  than the lists hold), `listed` (rows of each kind in the lists) and an
  `id`. `run` is the UI's id for that comparison. `limit`, which older UIs
  send, is ignored; `truncated_left` and `truncated_right` are always false
  and `ordered` always true.
- `data-compare-progress` (event) `{ run, side: "left" | "right", rows,
  phase: "reading" | "preparing" | "comparing" | "loading_differences",
  differences }`: about four times a second per side while it changes
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
- `data_compare_explain { id, asker }` → `{ text }`, Markdown from the profile
  (`src-tauri/src/commands/data_compare_ai.rs`).
- `data_compare_ai_filter { id, kind, request, asker }` → `{ filters,
  changed_in, sort, note }`, checked against the comparison's columns.
- `data_compare_key_hint { left, asker | null }` → `{ key, by: "unique" |
  "ai" | "" }`.
