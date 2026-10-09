# Query editor: navigation, parameters and snippets

## Go to definition from code

- **⌘+click** (Ctrl+click on Windows and Linux) on the name of a table, view,
  collection or routine opens it. While the key is held, the names DBine
  recognizes are underlined on hover.
- Tables and collections open their **structure**; views and routines, their
  **definition**. What has no columns or code (a Redis key, for example) opens
  its data.
- The editor menu (right-click) adds **Go to definition** and **Show in
  explorer**, which expands the tree down to the object and selects it.

Names are looked up among the objects of the tab's database that the explorer
knows (its cache first, and what the server answers afterwards). It
recognizes schema-qualified names (`sales.orders`), names in quotes or
brackets (`"Orders"`, `[Orders]`, `` `orders` ``) and `FROM`/`JOIN` aliases: a
click on `p` or on `p.total` opens `p`'s table. The exact spelling is looked
up first and, if it isn't there, case-insensitively. In MongoDB it recognizes
the collection in `db.orders.find(…)`; in HTTP consoles, the index in the
path; in Cypher, the label.

## Unknown names

With [Code quality](code-quality.md) on, the editor flags as a **warning**
(never as an error, and it never prevents running):

- `unknown-table`: a table in `FROM`, `JOIN`, `UPDATE` or `INSERT INTO` that
  doesn't exist in the tab's database.
- `unknown-column`: a qualified column (`alias.column`, `table.column`) or
  one in an `INSERT`'s column list that the table doesn't have. Only when the
  table exists and its columns are already loaded.

It flags nothing while the database's objects haven't loaded, nor what can't
be known from the text: temporary tables (`#tmp`), table variables (`@t`),
CTEs, aliases, what the same script creates (`CREATE TABLE`,
`SELECT … INTO`), table functions (`generate_series(…)`), engine catalogs
(`pg_*`, `sys.*`, `information_schema`, `dual`…), names of another schema the
database doesn't have (another database, a linked server) and dynamic SQL,
which is inside strings.

Both rules are turned off in **Settings › Code quality**, like the others.

## Parameters

If the text being run has parameters, before running it **Query parameters**
opens with one per row: the type (**Text**, **Number**, **Date** or
**NULL**) and the value. On the right you see the literal that will end up
in the text, with the engine's quoting (`'O''Brien'`, `N'…'` in SQL Server,
`DATE '2024-05-01'`, `ISODate("…")` in MongoDB…). **Cancel** runs nothing.
Values are remembered per tab.

What is recognized as a parameter:

- `:name`, outside strings and comments. Never a `::type` cast, an array
  slice `a[1:n]`, the `:=` assignment, nor anything attached to another word
  (`a:b`).
- `?` in engines that use it as a placeholder and not as an operator: MySQL,
  MariaDB, SQLite, Db2, Trino, Snowflake, Hive, Spark, Databricks, DynamoDB
  (PartiQL), Couchbase, Cassandra and generic ODBC. Not in PostgreSQL (`?` is
  a jsonb operator) nor in ClickHouse (ternary operator). Each `?` is a
  separate parameter; a repeated `?1` is a single one.
- `@name` only in BigQuery, Spanner and Cosmos DB, where it is a parameter.
  In SQL Server, Sybase and MySQL `@` is a variable and isn't asked for.

A script that defines code (`CREATE PROCEDURE`, `CREATE TRIGGER`,
`EXECUTE BLOCK`…) doesn't ask for parameters: its `:name`s are variables of
the code itself (for example `:new.column` in an Oracle trigger).

Everything that executes goes through the same dialog: **Run**, the
statement at the cursor, plans and **Run on several databases**. The dialog
has **Don't ask for parameters in this tab**, which runs the text as is; it
is turned back on from the editor menu with **Ask for parameters when
running**.

## Snippets

Type an abbreviation and press **Tab**: it expands and the cursor lands in
the first field; **Tab** and **Shift+Tab** move between fields. Abbreviations
also show up in autocomplete.

Each engine has its built-in set in its syntax:

| Abbreviation | Inserts |
|---|---|
| `sel`, `selw` | `SELECT *` with the engine's limit (`TOP`, `LIMIT` or `FETCH FIRST`), with or without `WHERE` |
| `selc`, `seld`, `grp` | `COUNT(*)`, `DISTINCT`, grouped count |
| `ins`, `upd`, `del` | `INSERT`, `UPDATE … WHERE`, `DELETE … WHERE` |
| `cte`, `ij`, `lj`, `exi` | `WITH`, `INNER JOIN`, `LEFT JOIN`, `WHERE EXISTS` |
| `crt` | `CREATE TABLE` with the engine's identity column |
| `tran`, `try`, `proc`, `func`, `ups`, `blk` | Depending on the engine: transaction, `TRY/CATCH`, procedure, plpgsql function, upsert, PL/SQL block |

MongoDB (`find`, `agg`, `ins`, `upd`, `del`, `idx`…), Redis, etcd, Cypher,
Flux, Elasticsearch/OpenSearch/Solr, CouchDB and Cassandra have their own.

In **Settings › Snippets** you add your own: abbreviation, engines (one, all
SQL or all) and the text, with fields `${1:text}` (the number sets the order,
the text is the initial value) and `${}` where the cursor ends. A custom one
with the same abbreviation replaces the built-in one. They are stored in the
settings (`editor.snippets`), which travel with sync.
