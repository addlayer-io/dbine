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
- The app (`src-tauri/src/commands/rename.rs`) searches for what depends on
  the object, reads the definitions, classifies and builds the script around
  the rename with the same planner as **Compare schemas**. The commands are
  in [api-commands.md](api-commands.md#rename-with-impact).
- Drivers that run in their own process answer `RenameScript` over the
  protocol; one published earlier answers that it doesn't know it, and its
  manifest doesn't offer renaming.
