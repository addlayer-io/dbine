# Data subset

Copies **some rows** of a table to another database, with **all the parent
rows they need** (following foreign keys, recursively) and, if you want, the
rows that hang from them. Along the way, **personal data is masked**. It's
useful to build a small, consistent development or test database, without
real data, from a large one.

## Where it is

Right-click on a **table** › **Copy a subset…**. It opens in its own tab
(**Subset**).

The **source is never modified**: it's read with a read-only session, and the
target can't be the same database (same server, port and database).

## What is copied

1. **Rows of the start table:**
   - **Filter:** **No filter**; **Condition** (a condition in the engine's
     language, without the word `WHERE`, for example
     `status = 'active' AND created >= '2024-01-01'`), in engines that have a
     condition language; or **By column**, with the data grid's filters
     (equals, contains, starts with, is one of, is null…).
   - **Amount:** **All rows**, **First N rows** or **N % of the rows**.
2. **Parents:** always. Each chosen row drags along the rows of the tables it
   points to, and those theirs, all the way up.
3. **Children (optional):** **Include what hangs from these rows**, with
   **Levels** (1 to 10; 1 is the direct children) and a **Cap per table**. A
   table that reaches the cap is marked **reached the cap**.

Tables are written parents first. If there's a **foreign key cycle**, it's
cut at a column that accepts `NULL`: it's copied with `NULL` and completed at
the end. If none accepts it, the plan warns that the copy may fail if the
target validates the keys.

## The target

A **Connection** and a database, of the same engine or another.

- Tables that **already exist** receive the rows; source columns that aren't
  in the target table aren't copied (the plan says which).
- Those that **don't exist are created** with the source's structure,
  converted to the target engine when they differ. After the data, their
  indexes and foreign keys are created. **See how it is created in the
  target** shows the DDL.
- Computed columns aren't copied: the target computes them.
- A table that can't be created or copied is marked **cannot be copied**,
  with the reason, and blocks the copy.
- A **read-only** target is rejected.

## The plan

**Calculate the plan** reads source and target (without writing) and shows,
in write order:

- each table, its role (**Start**, **Parent**, **Child**), how many rows,
  whether it **already exists** or **will be created**, and the total rows
  and tables;
- per column, the **masking**;
- cut cycles and notes (for example, that a table has more than a million
  rows).

The copy reads the data again, because it may have changed since the plan.

## Masking

Each column has a rule. Those that look like they hold personal data by
their **name and type** (in Spanish and English) come suggested and marked as
**Personal data**. **Back to the suggested rules** undoes the changes.

| Rule | What it does |
|---|---|
| **Keep** | Copies the value. |
| **Fake** | A fake value of a kind: full name, first name, last name, email, phone, address, city or company. |
| **Fake: document** | Keeps the original's shape (national ID, tax ID, IBAN, card…) and replaces each digit: `20-12345678-9` becomes another number with the same format. |
| **Shift the date** | Moves the date up to N days in either direction; keeps the time. |
| **Vary the number** | Moves the number up to N %. |
| **Fixed value** | The same value for all rows. |
| **Null** | `NULL`. |
| **Hash** | A hash of the value. Suggested for passwords and tokens. |

- A `NULL` stays `NULL` with any rule.
- The result depends only on the run's seed, the rule and the original
  value: **the same value is masked the same way in all tables**. That way,
  an email repeated in two tables ends up the same and joins on masked
  columns still match.
- Keys are never suggested. If you mask one, the columns that reference it
  have to use **the same rule**; the plan warns about it (**Masked key**).
- Masking is done in DBine, before the data reaches the target.

### The hash is salted per run

The hash's salt (and that of all the rules) is a **new random seed on each
run**. Two copies of the same data give **different** hashes, so they can't be
compared or joined across runs. If you need stable values across copies, use
**Fixed value** or don't mask that column.

## Production

If the target has the tag `prod`, `production`, `producción` or `prd`,
**Copy** asks you to type the target database's name (if it has none, the
connection's) to confirm. The backend requires it too: without that text the
copy doesn't run.

The confirmation summarizes how many rows of how many tables are copied, how
many tables are created and how many columns are masked.

## Limits

- **With N % of the rows**, **the first 1,000,000 rows** of the start table
  (that meet the filter) are read and an **even** sample of N % is taken from
  them. It isn't a sample of the whole table: if it has more than a million,
  the plan warns that the first ones were taken. With **All rows** the same
  1,000,000 cap and the same warning apply; **First N rows** has that value as
  its maximum.
- **Cap of 2,000,000 rows in memory** in total (start, parents and
  children). Rows are gathered in DBine before being written. When exceeded,
  the copy is rejected: narrow the filter, the levels or the cap per table.
- **A partial copy isn't undone.** There's no transaction around the copy: it
  is written table by table, and the first one that fails cuts off the
  following ones. The tables created and the rows already written **stay in
  the target**. The result says per table whether it ended up **Copied**,
  with **Error**, **Cancelled** or **Not copied** (it wasn't reached), with
  the rows written. Cancelling has the same effect.
- Rows are written in batches of 500, and keys are looked up in the source
  500 per query.
- Engines without foreign keys (documents, key-value, time series) copy the
  chosen table or collection with its filter and masking, without parents or
  children.

## Result and tasks

**Copy** runs in the background, with per-table progress in the tasks panel,
and can be cancelled. When it ends the **Result** is shown, per table: rows
copied, columns masked, whether the table was **created** and the notes.

Which engines can be source or target is in
[`engine-support.md`](engine-support.md#copy-a-subset).

## Contract

It adds no methods to the drivers: it uses `database_schema` (structure and
foreign keys), `Driver::filtered_browse` (the keys' `IN` filter),
`table_ddl` and `insert_script` and `update_script` (the same as test data
and migration), and the `dbine_schema` converter when the engines differ.

Commands: `subset_plan` and `subset_run`, with the `subset-progress` event;
`cancel_query` on `subset:<runId>:src` and `subset:<runId>:tgt`. See
[`api-commands.md`](api-commands.md).
