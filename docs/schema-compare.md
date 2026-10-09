# Compare schemas

Compares the structure of two databases side by side and applies the
differences in one direction or the other. It's opened from a database's menu
in the explorer: **Compare schemas…**.

## How to use it

1. **Choose the two databases.**
   - The left one is the database it was opened from; the right one, by
     default, is another database of the same connection.
   - Each side can be any connection and database, even of another engine.
   - If both sides choose a schema, those two schemas are compared with each
     other (for example `sales` against `sales_qa` in the same database).
     Tables are matched by name.
2. **Compare.** While comparing, the **Stop** button is shown, which cancels
   the comparison. On the left appear the tables, views, procedures,
   functions and triggers with their status:

   | Icon | Status |
   |---|---|
   | `≠` | different |
   | `◧` | left only |
   | `◨` | right only |
   | `=` | equal (shown by unchecking "Differences only") |

3. **Review a table.** When you pick it, it's shown side by side: columns,
   indexes, foreign keys and primary key. What differs in each column (type,
   nulls, default value, auto-increment, comment) is highlighted. `CHECK`
   constraints go in their own section, "CHECK constraints", and the options
   of the table and its columns, in the "Table properties" row. Views and
   procedures show their code with the differing lines marked.
4. **Carry the changes with the arrows.**
   - `→` makes the right side equal to the left; `←`, the opposite. There are
     arrows per table and per column, index, foreign key or primary key.
   - If the object doesn't exist on the source side, the arrow **deletes** it
     from the other side. Each arrow's tooltip says what it's going to do.
   - **Delete** removes an object from one side without carrying it from the
     other; if it exists on both sides, it can be deleted from each one.
     Before running, "Sync" shows what depends on what's being deleted. Which
     engines and types allow it:
     [`engine-support.md`](engine-support.md#deleting-in-the-comparison).
   - Carrying a change doesn't touch the database: it only modifies that
     side's in-memory copy. The object is marked with a dot and the footer
     counts each side's unapplied changes.
   - **Undo** reverts the last step; **Discard** forgets all of a side's
     changes.
5. **Sync.**
   - Generates the script for that side's engine (`CREATE`, `ALTER`, `DROP`)
     and shows it with the warnings: data that is lost, changes that may fail
     with existing rows, things the engine doesn't allow.
   - **Open as query** puts it in an editor.
   - **Run** runs it, after confirmation, statement by statement. It stops at
     the first error and says which one it was.
   - Afterwards it re-reads that database. If something failed, what wasn't
     applied stays pending.
   - You can't sync onto a read-only connection.

## What the script generates

The order avoids dependency errors:

1. new or changed types, domains, sequences and full-text catalogs (types
   ordered by dependency: first the ones the others use);
2. foreign keys that change or depend on columns that change;
3. tables that are dropped;
4. indexes and primary keys that change;
5. the columns;
6. the primary key and the indexes again;
7. new tables;
8. the foreign keys;
9. the dropping of types, domains, sequences and catalogs that are gone,
   after the tables that used them.

In addition:

- **Views:** those that use a table whose columns change type or are dropped
  are dropped first and re-created afterwards. PostgreSQL, for example, won't
  let you change a column used by a view. Views over tables that are dropped
  are just dropped.
- **Views and procedures:** they're replaced by dropping the previous version
  and running the other side's definition. They're only carried between
  databases of the same engine.

How each family changes a column (the per-engine detail is in
`docs/engine-support.md`):

| Family | Column change |
|---|---|
| PostgreSQL and compatibles | `ALTER COLUMN … TYPE … USING`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT` |
| SQL Server, Azure SQL | `ALTER COLUMN … [NOT] NULL`. The default (a named constraint) and the indexes on the column are removed first and put back afterwards. |
| MySQL and compatibles | `MODIFY COLUMN` with the whole column |
| Oracle | `MODIFY (…)` |
| SQLite, libSQL | The table is rebuilt: new table, data copy, drop and rename. Adding columns is done in place. |

When the two sides are different engines, what is carried is converted with
the same converter as the migration (types, defaults, names), and the
conversion warnings are shown when carrying the change. What the target engine
doesn't have is dropped with a warning:

- `CHECK`, index options, full-text indexes, projections and `EXCLUDE`: they
  are dropped with a warning.
- An index's `INCLUDE` columns are kept only if the target supports them.

## Comparison

- **Matching:** tables are matched by schema and name, or by name only if two
  schemas are compared.
  - Case-insensitive by default ("Ignore case" option).
  - Columns: by name.
  - Indexes: by name and, failing that, by their columns, because generated
    names often change.
  - Foreign keys: by what they link (columns, table and referenced columns).
- **Types:** they're equal if they're written the same, regardless of case or
  whitespace, or if they mean the same: `int` = `integer` = `int4`. Between
  different engines the logical type is compared.
- **Default values:** they're compared without the wrapper each catalog adds
  (`('x')`, `'x'::text`).
- **Comments:** they count except with "Ignore comments".
- **Code:** views and procedures are compared with whitespace collapsed.
- **Indexes:** besides the columns, the `INCLUDE` (or `STORING`) columns, the
  order (`DESC`, `NULLS`), the filter, the engine's own options (fillfactor,
  compression, visibility, storage parameters…) and the type count: full-text,
  spatial and each engine's specific ones (for example `gin`, `gist`,
  `bitmap`, columnstore, `EXCLUDE`). A method DBine doesn't know is kept as
  is, not re-created as btree.
- **CHECK constraints:** they're read from the engine's catalog. In some the
  name is generated by the engine (`SYS_C…` in Oracle, `INTEG_n` in Firebird)
  and DuckDB doesn't store names.
- **Table properties:** between two databases of the same engine, the options
  of the table and its columns are compared (for example the clustering key
  in Snowflake, `SHARD KEY` in SingleStore, `ttl` in GreptimeDB or
  ClickHouse's `ASSUME`).
- **Other code objects:** besides views, procedures, functions and triggers,
  depending on the engine, sequences, synonyms, types, domains, catalogs and
  full-text stop lists (SQL Server), virtual tables (SQLite: FTS5, FTS4,
  R\*Tree) and dictionaries (ClickHouse) are compared. They're replaced with
  `DROP` and `CREATE`, and only between databases of the same engine.

What is compared in each engine and what isn't: `docs/engine-support.md`,
"Compare schemas" section.

## Modify a table

**Modify…**, in a table's menu in the explorer, opens the "New table"
designer in edit mode, with the table as it is today. You can add, change and
delete columns, indexes and foreign keys, and edit comments and options. The
table's name and schema aren't changed here: that's what **Rename…** is for.

- **Review changes…** builds a single script: the engine's `ALTER` from the
  table as it was to the edited one, with the same machinery as **Compare
  schemas**. It's shown in the SQL tab with its warnings.
- Nothing runs until **Run N statements**. If there are data-loss warnings, it
  asks for one more confirmation; on a production connection you have to type
  the table's name.
- It all runs or nothing in engines that run DDL inside a transaction.
- Views that use the table are dropped and re-created around the `ALTER`. If
  the engine rebuilds the table (SQLite, to change type or nullability), its
  triggers are re-created too.
- The designer keeps what it doesn't show: the `CHECK`s, the `INCLUDE` columns
  and index options, the order of the key's columns and other options.
- Renaming an existing column in the grid opens the **Rename** dialog in
  collect mode: it shows what depends on the column (views, routines…) and
  the rename script with the dependents rewritten. **Use in the designer**
  adds that script, which runs **first**, before the `ALTER`. Where the
  engine's `rename_spec` doesn't cover columns, the name stays fixed in the
  designer and it explains why.
- If the engine can't apply a change (a column's type in Cassandra, existing
  document fields in MongoDB), the script warns about it and nothing runs.

It's offered when the driver has a designer, `supports_schema_sync`, the
object's type is the designer's and the connection isn't read-only. The
engines that don't have it, with the reason, are in `docs/engine-support.md`
("Modify tables").

## Commands

They're in `docs/api-commands.md`, in the "Compare schemas" section.
