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
- **Compare** shows:
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
  too. A text with leading zeros (`"007"`) is compared as text.
- Up to 200,000 rows per side are read. If a table has more, the result says
  so: the comparison is partial.
- Up to 2,000 rows of each type are shown; the totals and the script include
  all of them.
- If a key repeats on one side, the row is compared only once and the result
  says so.

## The script

It is written by the target's driver, in its language (`insert_script`,
`update_script` and `delete_script` of the contract): SQL on SQL engines,
`insertMany`/`updateOne`/`deleteOne` in MongoDB, etc. The order is delete,
update and insert.

- Updates change only the differing columns.
- A delete always carries the key: a `DELETE` without `WHERE` is never
  generated.
- Rows inserted with the value of an identity or auto-increment column carry
  what the engine needs (`data_load_wrap`): in SQL Server,
  `SET IDENTITY_INSERT … ON` before and `OFF` after (without that, error
  544); in PostgreSQL, the sequence is moved past the last value, so the next
  insert doesn't collide with a copied id.
- Read-only connections reject the script when it is run.

## Commands

- `data_compare { left, right, key, columns, limit }` → the totals, the
  sample rows and an `id`.
- `data_compare_script { id, choices }` → one script for each side that
  changes: `{ connection_id, database, side: "left" | "right", script,
  inserts, updates, deletes }`. `choices` has `changed`, `only_left` and
  `only_right`; each carries `all` (`"left"`, `"right"` or `"none"`) and
  `rows`, the exceptions `[index, direction]` over the complete list of that
  view.
