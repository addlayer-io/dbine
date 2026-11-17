# Run on several databases

Runs the editor's code (or the selection) on **several databases of the same
connection** and gathers the results. It's useful, for example, to query the
same schema in each of a customer's databases or to apply a change to all of
them.

## Where it is

In the query editor, the **Run on several databases…** button in the bar. It
appears only for engines that have several databases per connection (those
that show databases in the explorer); the list of those that don't is in
[`engine-support.md`](engine-support.md#run-on-several-databases).

If the editor is empty, it says **There is no code to run**.

## How databases are chosen

- The dialog lists the connection's databases (if it is still opening, it
  waits for it to load).
- The **filter** accepts `*` (for example `*tenant-n*`). Without `*` it
  searches for the text inside the name. It is case-insensitive.
- **All** and **None** act on what the filter shows, so several filters can
  be combined.
- It shows how many databases are selected, how many are visible and which
  is the **current** one (the tab's).
- The selection is **remembered per connection**. The first time, the tab's
  database comes selected.
- **Run on N databases** is enabled with at least one selected.

## How it runs

- Each database uses its **own session** (never the explorer's), **4 at a
  time**. The rest wait their turn.
- The code runs the way the editor runs it on that engine: statement by
  statement, batch by batch or whole. It respects the editor's maximum rows,
  **per result and per database**, and the option to continue after an error.
- It runs as a **task**: if you close the dialog it keeps going in the
  background and shows in the **Tasks** panel. The dialog shows each
  database's status and **N of M databases finished**.

### Safety

- A **read-only** connection still rejects anything that isn't a read, here
  too.
- If the code is **not read-only**, it asks for confirmation before running,
  with the first statement that writes and how many databases it will run on.
  The same if the engine isn't SQL and it can't be verified. It never runs
  without your confirmation.

## The results

- If the **first result** of every database that finished OK has the **same
  columns** (case-insensitive), they are merged into **a single grid**. Its
  first column is **`base`** and carries the name of the database each
  row came from. Rows follow the order in which you chose the databases. The
  tab says **Result (N databases)**.
- If the columns don't match, nothing is merged: there is **one tab per
  database** (and per result, if a database returned several).
- If there is a merged grid, each database's other results go in separate
  tabs, with the database name and a number.
- Copying takes the loaded rows, like any grid. Exporting takes **every
  row**: see [Exporting the results](#exporting-the-results).
- The **Messages** tab has one line per database: status, rows (or affected
  rows) and time, the server's messages and the error if there was one. Above
  the result you see the summary, for example **3 of 4 databases OK, 1 with
  error**.

## Exporting the results

The grid keeps the editor's maximum rows per database, but an export writes
**every row** of the result, as with a single database:

- The **Export** menu and the **all rows** option of the advanced export
  (⌘E) run the code again on **each database, one after the other**, in the
  order of the grid, and write the rows to the file as they arrive.
- The **merged grid** gives one file with the **`base`** column first and the
  rows of every database that finished OK. A database's own result (its tab)
  runs again only on that database.
- Each database runs in a **read-only session** of its own, the way the run
  did (statement by statement, batch by batch or whole), so the file has the
  same result the grid shows. If the code writes, the engine rejects it.
- The progress shows the rows written, against the total the run counted (an
  estimate: the data may have changed since).
- If a database fails, the export stops and says which database and why. If
  a database's result no longer has the first database's columns, it stops
  too. It can be cancelled. In every case the half-written file is deleted.
- A database that ended with an error or cancelled exports only the rows its
  tab has.

## Errors per database

An error in one database **doesn't stop the others**. Each database ends in
one of these states:

- **OK.**
- **Error:** the database couldn't be opened or the code failed (the first
  error is reported). Its partial results stay in its tab.
- **Cancelled:** it was interrupted while running.
- **Not run:** it was cancelled before its turn came.

Databases with an error, cancelled or not run **don't enter the merged
grid**; those that finished OK are merged anyway. Within a database, after an
error the code continues or stops according to the editor's continue-after-
error option (by default, the engine's); a fatal error or a lost connection
always stops it.

## Cancel

From the dialog (**Cancel**) or from the **Tasks** panel. No further database
starts (they stay **Not run**) and the running ones are interrupted, like the
editor's Cancel. If a statement doesn't stop within 5 seconds, that
database's connection is closed and it ends **Cancelled**. The results of
databases that already finished are kept.

## Limits

- Only **databases of the same connection**; it doesn't mix connections.
- **4 databases at a time.**
- The editor's maximum rows applies **per result and per database**, so the
  merged grid can have up to that maximum multiplied by the databases.
- Only the **first result** of each database is merged.
- There is no transaction across databases: if the code writes, databases
  that already finished are **not rolled back** when another fails or is
  cancelled.

## Engine particularities

It uses only each driver's execution (`Session::execute`) and each engine's
way of splitting the script, so it works the same on all engines that have
several databases. Those that don't, and what was tested against real
servers, are in
[`engine-support.md`](engine-support.md#run-on-several-databases).

## Contract

Commands `run_multi_db` and `cancel_multi_db`
(`src-tauri/src/commands/multi_db.rs`). Event `multi-db-progress` for each
database that finishes (status, rows, time, error, `done` / `total`). The
action is offered according to `DriverInfo::databases_label`. It adds no
methods to the driver contract.

The export of every row is `export_query_to_file` with
`multi: { databases, set, base }` (`src-tauri/src/commands/export.rs`): `set`
is which of each run's result sets with columns (0, the first, for the
merged grid) and `base` puts the database's name first. The UI builds it in
`web/src/composables/multiDbParts.ts`.
