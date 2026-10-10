# Rename with impact

Right-click a table, a view, a routine, a trigger, a column, an index or a
schema › **Rename…**. DBine builds a single script that changes the name and
updates the database code that uses it: the views, routines and triggers that
name it are recreated with the new name. The script is shown in full before
running it.

The option appears only where the engine can rename that kind of object and
the connection is not read-only. What each engine renames is in
[Engine support](engine-support.md#rename-with-impact).

## The dialog

1. **New name.** It is typed exactly as it will end up, with its case. If the
   engine needs quotes to preserve it (uppercase in PostgreSQL, lowercase in
   Oracle, spaces or reserved words in any), the dialog says how it will be
   written. If an object with that name already exists, it says so too.
2. **What depends on the object.** It is the same search as **View
   dependencies…**, split into three groups:
   - **Rewritten**: views, routines and triggers whose code names the
     object. Each one shows the lines that change (before and after) and can
     be unchecked. Those with parts DBine couldn't decide start unchecked and
     marked **review manually**, with the line and the reason; if checked,
     those lines stay as they were.
   - **Updated by the engine**: foreign keys, indexes, constraints and the
     objects the engine follows on its own (in PostgreSQL, views and
     triggers). Nothing needs to be done.
   - **Manual attention**: dynamic SQL, definitions that couldn't be read and
     code that names the object in a way DBine doesn't recognize. **Open
     definition** shows them so you can review them later.
3. **Script.** It updates with each checkbox. It can be copied or opened in a
   query to run by hand.
4. **Rename.** Runs the script as a task (it can be followed in the
   background and cancelled). Where the engine allows structure changes
   inside a transaction (PostgreSQL, SQL Server, SQLite), everything runs
   together: if a statement fails, nothing changes. In the others, whatever
   was done before the error stays done, and the dialog says so before
   running.

On a production connection (a `prod` tag or similar, or a project environment
marked to confirm every execution) the new name must be typed again to enable
**Rename**.

When it finishes, the explorer refreshes and the object's open tabs (data,
structure, definition, indexes, dependencies) are renamed to the new object.
Query tabs aren't touched: if any names the old name, DBine says how many.

## Renaming a database

Right-click a database in the explorer › **Rename…**. It appears on engines
whose driver says it renames databases (`RenameSpec.databases`), when the
connection is not read-only. ClickHouse keeps its older path: there the
database is the schema, and it is renamed from the schema's node. Which
engines rename databases, and which don't and why, is in
[Engine support](engine-support.md#renaming-a-database).

The dialog shows:

- the engine's note: how it renames and what it cuts;
- the other sessions open on that database, which the rename ends (user,
  host and program);
- whether the new name already exists;
- for engines that move the contents (MySQL, MariaDB, MongoDB), how many
  objects move;
- that code in other databases, jobs, applications and connection strings
  that use the old name is not changed;
- the full script.

On **Rename**:

1. DBine closes its own sessions on that database.
2. The script runs from another database (`database_from` in the spec: `master`
   in SQL Server, `postgres` in PostgreSQL, `admin` in MongoDB). Snowflake
   needs none.
3. It is not atomic: if a statement fails, what was already done stays done.
   The dialog says so before running.

On a production connection the database name must be typed again to enable
**Rename**.

Afterwards DBine's own references follow the new name: the connection's
default database, open tabs, saved queries, migrations, project targets and
scheduled task steps. A scheduled task with steps that change data in that
database has to be approved again.

Per engine:

- **SQL Server:** `ALTER DATABASE … SET SINGLE_USER WITH ROLLBACK IMMEDIATE`
  (it rolls back open transactions and cuts the other sessions), then
  `MODIFY NAME`, then `MULTI_USER`. The data and log file names keep the old
  name. Azure SQL Database and Babelfish run only `MODIFY NAME`. Azure SQL
  Database cuts the other sessions by itself (not tested live); Babelfish
  refuses while another session is connected (tested), so close them first.
  System databases are refused.
- **PostgreSQL family:** `pg_terminate_backend` on the other sessions, then
  `ALTER DATABASE … RENAME TO`, from `postgres` (YugabyteDB: `yugabyte`;
  KingbaseES: `kingbase`; Redshift: `dev`, using `procpid`). CockroachDB and
  RisingWave rename with sessions open and don't close them; sessions that
  had the database as current must reconnect (in RisingWave all of them).
  Yellowbrick doesn't close sessions: DBine asks you to close them first.
- **MySQL, MariaDB, Aurora MySQL, Cloud SQL for MySQL:** there is no
  `RENAME DATABASE`, so DBine emulates it. A guard comes first: if the
  database holds events, MariaDB sequences or objects DBine didn't read, the
  first statement fails on purpose and nothing changes. Then: `CREATE DATABASE`
  with the same charset and collation, drop the triggers, `RENAME TABLE`
  across databases, recreate routines, views and triggers (references to the
  old name rewritten), drop the old objects, and `DROP DATABASE` only if it is
  empty. Grants are not copied and `DEFINER` is kept. Each routine, trigger
  and event is created again under the `sql_mode` and collation it was
  created with, one statement per request. A definition that would not read
  as exactly one statement under every quoting mode is refused, and the
  script stops before creating anything if an object uses MariaDB's
  `ORACLE` or `MSSQL` mode or is gone.
- **Snowflake:** `ALTER DATABASE … RENAME TO`. Granted privileges follow.
  Names with a backslash are not renamed from DBine.
- **MongoDB:** `renameCollection` from `admin`, one collection at a time;
  views are recreated in the new database and `system.views` of the old one
  is dropped. Users and roles are not moved, and time series collections
  stop the rename. Only databases named with letters, digits, `_` and `-`
  are renamed from DBine.

## What is rewritten and what isn't

DBine only changes what it is sure is the object:

- Comments and quoted strings (dynamic SQL) are never touched. A string that
  names the object leaves the routine for review.
- A name qualified with another schema (`sales.Customers` when renaming
  `dbo.Customers`) is not the object. An unqualified name inside an object of
  another schema depends on the search path: it is left for review.
- Words that merely contain the name (`OldCustomers`) don't count.
- A table name followed by `(` may be a function with the same name: it is
  left for review (except in `INSERT INTO Customers (…)` and similar).
- A column is changed when its table or an alias of that table qualifies it
  (`c.Pepe` with `FROM Customers c`), or when the statement reads only that
  table. If the statement reads several and the column has no table, it is
  left for review.
- In a view, a renamed column that appears alone in the `SELECT` list becomes
  `new AS old`, so the view keeps its column names and whatever uses it keeps
  working. The **Keep the view column names** checkbox (on) controls this.
- When renaming a schema only the qualifiers change (`sales.table`); a column
  with the same name as the schema does not.
- Original quotes are respected: `[Customers]` becomes `[New]`.
- In PostgreSQL the body of `sql` and `plpgsql` functions and procedures is
  read as code; the `EXECUTE '…'` inside them remains text.
- In MongoDB the collections in `viewOn`, `$lookup.from`, `$unionWith.coll`,
  `$out` and `$merge` are rewritten. Renaming a field doesn't rewrite views.

Rewriting never edits the clauses that say who the code runs as: MySQL and
MariaDB `DEFINER`, `SQL SECURITY`, `SECURITY DEFINER|INVOKER`, Oracle
`AUTHID` and `EXECUTE AS`. A user or role called like the renamed object
stays as it was. If a rewrite would still change a `DEFINER`, the dependent is
left for review.

Some engines never get their dependents re-created. On Snowflake and on
Trino, Presto and Starburst (`RenameSpec.references` is `None`), everything
that names the object is only listed, to fix by hand: views, materialized
views, functions, procedures, tasks, streams and dynamic tables on Snowflake;
views, materialized views and SQL functions on the Trino family. Re-creating
them would hand the object to the renaming role and lose its security
settings (and, on Snowflake, the schedule of tasks, the offset of streams and
the rows of materialized and dynamic tables). The app also refuses any
rewrite sent for such an engine.

Every statement of the script is checked before it runs: the driver's rename
statements, each rewritten or carried dependent and the final statement must
be exactly one statement as the engine's own splitter cuts it. If stored code
would split into more (a `;` or a `GO` line the quoting doesn't hold, or `//`
comments on Dremio and Drill, which read them as comments like the server),
the rename is refused naming the object. Athena also checks the catalog's
type of a column against a strict type grammar before renaming it.

Rewritten objects come back with the statement that preserves their
permissions when the engine has one (`CREATE OR ALTER` in SQL Server,
`CREATE OR REPLACE` in Oracle, in PostgreSQL routines and in MySQL views).
Where it doesn't, they are dropped before renaming and created afterwards,
and the script warns that the permissions granted on them are lost. In SQL
Server, views with `SCHEMABINDING` are always dropped first, because the
engine won't let you rename what they use.

What is outside the database (other databases, applications, reports,
scripts) isn't checked: anything using the old name stops working.

## How it works

- The contract is in `crates/dbine-driver/src/rename.rs`: the driver says
  what it renames (`Driver::rename_spec`, a `RenameSpec`) and writes only the
  rename statement (`Driver::rename_script`). Rewriting the code
  (`rewrite_references`, `rename_header`, `quote_new`) is common to all
  engines and lives next to the dependency search, so both classify a name the
  same way.
- A database rename uses `Driver::rename_database_script`, plus
  `RenameSpec.database_from`, `database_note` and `database_moves` (the app
  reads the database's objects, `DatabaseObject`, for engines that move
  them).
- The app (`src-tauri/src/commands/rename.rs`) searches for what depends on
  the object, reads the definitions, classifies and builds the script around
  the rename with the same planner as **Compare schemas**. The commands are
  in [api-commands.md](api-commands.md#rename-with-impact).
- Drivers that run in their own process answer `RenameScript` over the
  protocol; one published earlier answers that it doesn't know it, and its
  manifest doesn't offer renaming.
