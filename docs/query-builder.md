# Query builder

Builds a `SELECT` without writing it: you drag tables onto a canvas, join
them, check columns and fill in a grid with aliases, aggregates, sorting and
filters. DBine writes the query in the engine's SQL (or CQL).

## Where it is

- Right-click a **database** › **Design query…**.
- Right-click a **table or view** › **Design query…**: opens the builder with
  that table already on the canvas.
- The **Design** button in the query editor's bar.

It opens in its own tab (**Design query**). It exists only on engines with a
SQL or CQL language; the others (documents, key-value, graphs…) don't offer
it. The design is saved with the tab.

## The canvas

- The table list on the left has a search. A table is added by **dragging
  it** onto the canvas or by **double-clicking**.
- The canvas pans and zooms like the database diagram. Each table shows its
  columns, with primary and foreign keys marked. Double-clicking the name
  changes the **alias**.
- **Joins:**
  - When a table is added, joins by **foreign keys** with the tables already
    there are created automatically.
  - You can also join by **dragging a column onto a column of another
    table**.
  - Clicking a join lets you change its type (`INNER`, `LEFT`, `RIGHT`,
    `FULL`), swap the order of the tables or remove it.
  - A table without a join is combined with all the rows of the others
    (`CROSS JOIN`), and the query warns about it.
- Engines that query **one table at a time** (see
  [`engine-support.md`](engine-support.md#query-builder)) only
  allow one table on the canvas.

## The grid

One row per column. They are added by checking the column in the table, with
**Add column…**, or with `*` (**all columns**).

| Field | What it does |
|---|---|
| **Column** | A column of a table on the canvas. |
| **Alias** | The column's name in the result. |
| **Show** | Whether it is in the `SELECT` list; unchecked, the column only filters or sorts. |
| **Aggregate** | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` or `COUNT DISTINCT`. Columns without an aggregate go into `GROUP BY`. |
| **Sort** and **No.** | Ascending or descending, and the priority when sorting by several. |
| **Filter** | One condition per cell: `=`, `<>`, `<`, `<=`, `>`, `>=`, `LIKE`, `NOT LIKE`, `IN`, `NOT IN`, `BETWEEN`, `IS NULL`, `IS NOT NULL`. |

- Filters are grouped: the cells of the same group (the **Filter** column,
  or **OR** for the following groups) combine with `AND`, and the
  groups with each other with `OR`. **Add an OR group** adds a group.
- A condition on a column with an aggregate goes to `HAVING`; the others, to
  `WHERE`. Each part combines its OR groups separately, and the builder warns
  about it.
- The value is written as text or number depending on the column's type.
  With the expression button (**The value is a SQL expression**) it is
  written as is, without quotes. In `IN` and `NOT IN` the values are
  separated by commas.
- At the top are **Limit** (or **No limit**) and `DISTINCT`.

## The query

The generated SQL is shown live, with **warnings** about what the engine
doesn't do (for example, **this engine does not offer DISTINCT in this
query**). Functions the engine doesn't have aren't offered in the interface.

- **Run preview:** fetches up to **100 rows** on its own **read-only
  session**, and says whether there are more rows. It can be cancelled.
- **Open in a query:** passes the SQL to a query tab so you can run, edit or
  save it.

The builder **never changes the database** and only writes `SELECT`. The SQL
is not read back to build the design: if you edit it in the query tab, the
design doesn't find out.

## Engine particularities

Each engine brings its own names, quotes and row limit:

- Tables are named like the engine's **View data** query (a BigQuery
  dataset, an IoTDB path, a Couchbase keyspace, a Cosmos DB container…), and
  the row limit is the engine's: `LIMIT`, `TOP`, `FETCH FIRST` or `FIRST`.
- Identifiers are quoted with `"…"`, `` `…` `` or `[…]` depending on the
  engine; SQL Server and Sybase strings carry the prefix `N'…'`.
- In **CQL**, a filter outside the primary key adds `ALLOW FILTERING` with a
  warning, because it scans the table.

Which joins, aggregates and operators each engine has is in
[`engine-support.md`](engine-support.md#query-builder).

## Contract

It adds no methods to the drivers. Names and the row limit come from
`Session::browse_query` (the **View data** query); what that text doesn't say
(joins, grouping, `HAVING`, operators) is per dialect in `features()` of
`src-tauri/src/commands/query_builder.rs`. To build the joins it uses the
foreign keys that `database_schema` already reports.

Commands: `build_query` and `preview_built_query` (cancellable with
`cancel_query` on `qb-preview:<sessionId>`). See
[`api-commands.md`](api-commands.md).
