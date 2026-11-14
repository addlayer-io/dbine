# Query editor: running, navigation, parameters, snippets and result tabs

## Running part of a script

- **Run** (Cmd+Enter on macOS, Ctrl+Enter on Windows and Linux, or the
  toolbar's **Run**) runs the selection or, with nothing selected, the whole
  script.
- **Run the statement at the cursor** (Cmd+Shift+Enter or Ctrl+Shift+Enter,
  the toolbar button next to **Run**, or the editor menu) runs the statement
  the cursor is in, without selecting it. The statement is cut by the
  engine's own rules: `;`, a `GO` batch in SQL Server, a PL/SQL block ending
  in `/` in Oracle, the terminator `DELIMITER` sets in MySQL or `SET TERM`
  in Firebird, a MongoDB or Cypher statement ended by `;`… With the cursor
  between two statements (on a blank line, or after a `;`), the one before
  it runs. `DELIMITER` and `SET TERM` lines are the editor's, not the
  server's: they never run on their own. The engines that don't end
  statements at `;` are cut as they execute: see
  [Statements on other engines](#statements-on-other-engines).
- **The run lens.** With two or more statements in the editor, a dim line
  above each one offers **▶ Run**, and **Plan** (the estimated plan) on
  engines with execution plans, followed by the statement's
  [name](#named-results) when a comment gives it one. A click runs exactly
  that statement, as **Run the statement at the cursor** does. It follows
  the same rules as the **Run** button (it waits while something runs), and
  it isn't shown in a file tab without a database, nor on scripts of more
  than 500 statements or 1 MB. **Settings › General › Show "▶ Run" above
  each statement** turns it off.
- **Estimated plan** (Cmd+L or Ctrl+L) and **Run with actual plan**
  (Cmd+Shift+L or Ctrl+Shift+L) take the selection or the whole script.

### Statements on other engines

The editor cuts a script into the same statements the engine's own execution
runs, so the lens, **Run the statement at the cursor** and
[named results](#named-results) agree on each one. A comment line right above
a statement names its result, and the lens sits on its first line of code.
Comments and blank lines belong to no statement.

- **Elasticsearch, OpenSearch, Open Distro and Solr (console).** A statement
  is a request: its method line (`GET`, `POST`, `PUT`, `DELETE`, `HEAD`,
  `PATCH`) and its JSON or NDJSON body, up to a blank line once the braces
  close or the next request. A line starting with `SELECT`, `SHOW`,
  `DESCRIBE` or `DESC` is an SQL statement up to its `;`, a blank line or the
  next request. Lines starting with `#` or `//` are comments.
- **CouchDB.** A statement is an HTTP line (`GET`, `POST`, `PUT`, `DELETE`,
  `HEAD`, `COPY`) with its path (`POST` and `PUT` take the JSON after it as
  their body) or a Mango query. Several queries in a row are one statement
  each.
- **MongoDB, FerretDB and Amazon DocumentDB.** A statement is one shell
  unit: a `db.…` call chain (it goes on over lines that start with `.`, such
  as `.sort(…)`), a `{…}` command document, or a `use` / `show` line. The
  `;` between them is optional. Brackets, strings, regular expressions and
  comments are counted, so a multi-line aggregation pipeline is one
  statement.
- **Redis, Valkey, Dragonfly and etcd.** A statement is one command line,
  quoted as in `redis-cli` (`"…"` with backslash escapes, `'…'` verbatim) or
  `etcdctl`. A `#` at the start of an argument outside quotes starts a
  comment in Redis, Valkey and Dragonfly; `//` is not a comment. `MULTI`,
  each queued command and `EXEC` are statements of their own.
- **KDB+.** A statement is a q expression: a line that isn't indented starts
  the next one, and an indented line or an open `{…}` continues it.
- **Cassandra, ScyllaDB and Amazon Keyspaces.** A CQL statement ends at `;`.
  A cqlsh command (`CONSISTENCY`, `PAGING`, `TRACING`, `SOURCE`…) takes its
  line, with or without `;`, and `BEGIN BATCH … APPLY BATCH` is one
  statement.
- **Neo4j, Memgraph and Amazon Neptune.** A Cypher statement ends at `;`. A
  `:command` line (`:use`, `:param`, `:begin`…) is a statement of its own.
- **Azure Cosmos DB.** A statement ends at `;` outside quotes, comments and
  JSON bodies. A `-- container: x` line is a statement of its own and ends
  the one before it.
- **InfluxDB 1.** A statement ends at `;`. A `USE db` line without `;` is a
  statement of its own. InfluxDB 2 (Flux) has no statements: a Flux script
  is one query and runs whole.
- **DynamoDB.** A statement ends at `;` outside quotes, comments and `{…}` /
  `[…]` bodies.
- **Power BI (DAX).** A statement is a query, cut as Power BI runs it: `DEFINE`
  starts one and keeps every `EVALUATE` after it, and an `EVALUATE` with no
  `DEFINE` before it is a statement of its own. See [DAX](#dax-power-bi).

A line that is a statement of its own because it sets the context (`:use`,
`-- container:`, `USE`, a cqlsh command) can be run alone: it then uses the
session's current container or database. A statement that doesn't parse yet
is still one: the others keep their lens, and running it reports the error on
its line. Running the whole script runs nothing while a statement has a
syntax error, on the engines that check the script first (the consoles,
CouchDB, MongoDB, Redis, Valkey, Dragonfly and etcd).

In SQL, blank lines don't split statements: two statements with no
terminator between them are one for the engine, however far apart they are. Before
running, the text goes through [parameters](#parameters); how a script runs
statement by statement, its messages and its errors are in
[Script execution](script-execution.md). Every key above can be changed:
[Keyboard shortcuts](#keyboard-shortcuts).

## Function completion

The editor suggests the functions of the tab's engine, or what plays their
part in its language, next to the keywords, tables and columns it already
suggests. Where each engine's list comes from, and what it leaves out, is in
[Engine support](engine-support.md#function-completion).

Each suggestion shows:

- the **signature** and the **return type** (`(x, y) → integer`), when the
  source has them;
- the **description** the server gives, exactly as the server writes it
  (usually English). Engines that list no descriptions, and the lists DBine
  carries for engines that have no catalog to read, show none;
- **`+N overloads`** when a name has several signatures. They are one
  suggestion, not one per signature; the detail shows the first signature.

Picking one inserts `name()` with the cursor between the parentheses (just the
name when a `(` already follows). Functions rank above keywords, and a keyword
that has the same name as a function in the list (ignoring case) is left out,
so the function is offered once. A function in a schema or package is found by
its name (`toUp`) and shown and inserted whole (`strings.toUpper`).

The list follows the **server's version** and what is installed on it
(extensions, plugins, modules), and includes the user's own functions with
their schema. Where a server lists no functions, DBine uses a catalog built
from the engine's documentation and filtered by the server's version.

### Where it appears

In SQL-like languages (SQL, CQL, PartiQL, SQL++), Cypher and Flux:

- **Scalar, aggregate and window functions** where an expression starts: after
  `SELECT`, `WHERE`, `ON`, `BY`, `THEN`, an operator, `(` or a comma. Not in a
  column or parameter list being defined, and not inside a Cypher map or a Flux
  call, where the next word is a key.
- **Table functions** where a table goes: after `FROM`, `JOIN`, `LATERAL`,
  `APPLY`, `TABLE(` or a comma in a `FROM` list.
- **Never** in a string or a comment, where a name goes (an alias, after
  `AS`, the table after `INTO`), after a whole expression (where a clause
  keyword goes) or after a dot, unless the name before the dot is the
  functions' schema or package (`sales.`, `strings.`, `apoc.coll.`).

Functions that SQL writes without parentheses (`CURRENT_DATE`, `USER`…) are
completed as keywords, not as functions.

### By language

- **SQL dialects.** The engine's built-in functions, plus its stored and
  loadable functions, aggregates and table functions, as above.
- **Cypher and Flux.** The language's own functions: Neo4j's, with the
  installed plugins' (`apoc.coll.sum`), Memgraph's and Neptune's; Flux's
  prelude and the standard library's packages (`strings.toUpper`, shown with
  the package's import path as its schema). Neither has a grammar in the
  editor, so strings and comments are found by scanning the text with the same
  rules.
- **Redis, Valkey, Dragonfly and etcd.** Commands, offered as the **first
  word of a line**. After a command that has subcommands, its subcommands
  follow (`CLIENT ` offers `CLIENT KILL`). Picking one inserts `NAME ` with the
  blank. Inside an open `MULTI`, nothing is sent to the server to read the
  list: Redis offers the catalog DBine carries.
- **MongoDB, FerretDB, Amazon DocumentDB and CouchDB.** Operators where a key
  starts with `$`, quoted or not (`$match`, `$group`, `$gt`, `$set`; in CouchDB
  the Mango operators). A `$` in a **value** is a field path or a variable
  (`"$total"`, `"$$ROOT"`), so no operators are offered there. After a dot that
  follows a name, `)` or `]`, the shell's methods are offered
  (`db.orders.find`).
- **Solr.** Function queries inside JSON string values, because its catalog
  has no operators. Where operators exist (MongoDB, CouchDB), string values
  are data and get no suggestions.
- **q (kdb+).** The catalog joins q's own completion: a name that q already
  offers gains its parameters (`[x;y]`), and the others are added: q's words,
  the `.q` functions and the user's functions by namespace (`.u.upd`). The
  q-sql and control words stay keywords.

- **SQL lines of the Elasticsearch, OpenSearch, Open Distro and Solr
  consoles.** A line starting with `SELECT`, `SHOW`, `DESCRIBE` or `DESC`
  starts an SQL statement, which ends where the console ends it (a blank line,
  a `;` or the next request line). Inside it, the engine's SQL functions are
  offered by the SQL rules above, with strings and comments found by scanning
  the text, and the JSON suggestions (operators, function queries) are not.
  Those SQL functions never appear in the JSON lines. In a string, a backslash
  escapes a quote only when a letter or digit follows that quote (`'it\'s'`
  reads as one string); an escaped quote followed by anything else, as in
  `'say \'hi\' now'`, is misread, and nothing is offered after it in that
  statement. On Solr the SQL functions exist only on SolrCloud, and only when
  its `/sql` handler loads.
- **DataFrame SQL notebook cells.** They have no connection, so their
  functions are DuckDB's, read from the DuckDB DBine keeps on the computer.
  That DuckDB is downloaded the first time a DataFrame SQL cell runs; asking
  for the functions never downloads it. Until then these cells offer no
  functions, and after that run they offer DuckDB's built-ins. The list is
  read once per app run. See [Notebooks](notebooks.md).

Engines with no catalog (and Elasticsearch's, OpenSearch's and Open Distro's
JSON lines, which call no functions) get nothing extra; their completion is
unchanged.

### When the list is read

The catalog is read **once per connection and database**, the first time the
editor needs it, and kept while the app runs. It is dropped when the
connection disconnects, reconnects, is edited or is deleted, and the next
request reads it again, so functions created or dropped meanwhile show up. Each
database has its own list: the editor reads the one of the database it is on. Reading is one request to the
server (never one per function), and if it fails the editor falls back to the
documentation catalog instead of showing an error. Connections through the
Enterprise gateway get no functions yet
([Engine support](engine-support.md#function-completion)).

### Contract

A driver offers the list through `Session::function_catalog`, which by default
returns `Error::Unsupported` (the editor then offers nothing extra; the Tauri
command `function_catalog` turns it into an empty list). Each entry is a
`FunctionInfo` with `name`, `kind`, `args`, `returns`, `description` and
`schema`. `kind` is one of `Scalar`, `Aggregate`, `Window`, `Table`,
`Operator` (MongoDB and CouchDB operators) or `Command` (Redis and etcd). A
read-only session forwards the call. Documentation catalogs are text files with
one entry per line, `<kind> name(args) [-> returns] [since=x.y] [until=x.y]`,
read with `functions::parse_catalog(text, version)`.

## Compare a query with another

**Comparar con…** in the editor's menu (right-click) takes the selection, or
else the statement at the cursor, and opens a [data compare of two
queries](data-compare.md#comparing-two-queries) with that query on the left
and the right side empty. The statement's `:name` parameters are asked for
first, as running it does; cancelling that dialog cancels the compare. The
same item is in a result tab's menu ([Result tabs](#result-tabs)).

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

## DAX (Power BI)

A Power BI connection (semantic models) uses the DAX editor, also for `.dax`
files before they have a connection.

- **Highlighting.** Comments (`//`, `--` and `/* */`), strings (`"…"`, with
  `""` for a quote), table names (`'Sales'`), column and measure references
  (`[Amount]`), keywords, numbers, `TRUE` and `FALSE`, the operators, and
  functions, dotted names included (`INFO.VIEW.TABLES`, `STDEV.S`).
- **Statements.** A script is cut into units as the service runs them:
  `DEFINE` starts a unit and keeps the `EVALUATE`s after it, and an
  `EVALUATE` without a `DEFINE` before it is a unit of its own. The run
  lens, **Run the statement at the cursor** and the [named
  results](#named-results) work on those units. The comment line above a
  unit (`//` or `--`) names its result. A `DEFINE` followed by several
  `EVALUATE`s answers one result per `EVALUATE`, shown in the unit's tab
  and named by the unit's comment; naming each `EVALUATE` by the comment
  above it is pending
  ([engine support](engine-support.md#named-results)).
- **Completion.** The tables, columns and measures come from the explorer,
  and the functions from the model's catalog
  ([function completion](#function-completion)).
  - After `'`: the model's tables and the `DEFINE TABLE` names of the unit;
    picking one closes the quote.
  - After `'Table'[` or `Table[`: that table's columns, then its measures.
  - After a bare `[`: the measures, the unit's own `DEFINE MEASURE` and
    `COLUMN` names, and the columns of the tables the unit names.
  - On a plain word: tables (quoted where DAX needs it), the unit's `VAR`
    names, measures (inserted with their brackets), the model's
    user-defined functions, the keywords and the DAX functions, inserted as
    `NAME()`. Table functions are offered wherever an expression starts,
    and dotted names whole.
  - After `DEFINE MEASURE` or `DEFINE COLUMN` only the tables are offered,
    because the table comes first (`MEASURE 'Sales'[Margin] = …`).
  - Nothing is offered where you give a new name (after `VAR`, `DEFINE
    TABLE` or `DEFINE FUNCTION`, or in a user-defined function's parameter
    list), nor inside strings and comments.
  - Picking a name inside quotes or brackets also replaces the rest of that
    name up to its closing quote or bracket, but not when what follows is
    other code. Snippets and notebook `{{parameters}}` are never offered
    inside a `'table name'` or a `[column or measure]`.
- **Units in the results.** Each `EVALUATE` returns a table; a value goes
  in braces (`EVALUATE { [Total Sales] }`). Every result is a normal result
  tab: export, chart and the other result features work on it.
- **Navigation.** ⌘/Ctrl+click on a table opens its structure, on a
  `[Measure]` its definition (the measure of the table written before it
  first), and on `'T'[col]` the structure of `T`. Hovering a table's name
  shows its columns; hovering a column or a measure shows nothing yet
  (pending).
- **Snippets** (type the abbreviation and press **Tab**):

  | Abbreviation | Inserts |
  |---|---|
  | `ev` | `EVALUATE TOPN(n, 'Table')` |
  | `def` | `DEFINE MEASURE … EVALUATE { [Measure] }` |
  | `sc` | `EVALUATE SUMMARIZECOLUMNS(…)` with a column and a measure |
  | `topn` | `TOPN(n, …, [Measure], DESC)` |
  | `calc` | `CALCULATE([Measure], filter)` |
  | `var` | `VAR … RETURN …` |
  | `info` | `EVALUATE INFO.VIEW.TABLES()` |
  | `cmp` | a measure next to the `SUM` of a column, by year |

- **Files.** Scripts are saved and opened as `.dax` files, in the library,
  projects and Git.
- **What DAX doesn't have here.** No `:name` query parameters (the service
  takes only the DAX text, which has no marker for one; a notebook's
  `{{name}}` parameters do work, written as DAX literals) and no
  **Format** (no DAX formatter runs inside the app, and the online ones
  would receive your code). Code quality has no DAX rules yet.

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
Flux, Elasticsearch/OpenSearch/Solr, CouchDB, Cassandra and KDB+'s q
(`sel`, `agg`, `aj`, `upd`, `ins`, `ups`, `del`…) have their own.

In **Settings › Snippets** you add your own: abbreviation, engines (one, all
SQL or all) and the text, with fields `${1:text}` (the number sets the order,
the text is the initial value) and `${}` where the cursor ends. A custom one
with the same abbreviation replaces the built-in one. They are stored in the
settings (`editor.snippets`), which travel with sync.

## Result tabs

Each run's results stay in the editor until a later run replaces them or you
close them, so you can go back to an earlier result without running it again.

- **How many are kept.** By default each editor keeps one unpinned result: a
  new run replaces it. **Settings › General › Results each editor keeps**
  raises that, up to 20. Past it, the unpinned result that ran longest ago
  closes when a run ends. [Named results](#named-results) don't count, as
  pinned ones don't.
- **Pin.** The pin on the results bar, or on a result's tab, keeps that
  result: later runs open a tab next to it instead of replacing it. A pinned
  result doesn't count toward the limit and only closes when you close it.
- **The tabs.** With more than one result, or a pinned or named one, the
  tabs show above the results, each with its name or the first line of its
  query, and the time it ran. Hovering one shows its name, its query, the
  connection and database it ran on, and the date. A spinner marks the run
  in progress and a red mark one that ended with an error.
- **A tab's menu** (right-click, or Shift+F10): **Pin** or **Unpin**, **Run
  again**, **Rename**, **Copy query**, **Comparar con…**, **Close**, **Close
  other unpinned** and **Close unpinned**. **Comparar con…** opens a
  [data compare of two queries](data-compare.md#comparing-two-queries) with
  that result's query on the left; it is off for the result of a run on
  several databases. A middle click closes a tab; the one running can't
  be closed.
- **Run again** runs the same text, with the parameter values it had, and
  its results replace that tab's. It's offered while the editor is on the
  connection and database where the result ran. A result of **Run on several
  databases** runs again from that dialog.

Each tab keeps its own sub-tab, sort, view (table, JSON, chart, map) and
pending edits. An export or a whole-result chart of a tab runs that tab's
query again, on the connection and database where it ran, not the editor's
current text.

**Kept when DBine closes.** With **Settings › General › Keep results when
closing DBine** on (the default), each editor's result tabs are also saved
to an encrypted file on this computer. Close DBine, or it crashes, and each
editor shows its tabs again when it opens: pinned and named ones, the order
and the rows as they were, without running anything or connecting. Closing an
editor deletes its saved results, and with none left the file goes too.
**Space for saved results** (2 GB by default) caps them, dropping the
oldest unpinned ones first, and **Clear now** deletes them all. Without a system keychain they stay in memory only.
Details: [results-store.md](results-store.md).

## Named results

A line comment right above a statement names its result:

```sql
-- Active customers
SELECT id, name FROM customers WHERE active;

-- Orders this month
SELECT id, customer_id, total FROM orders
WHERE created >= date_trunc('month', now());
```

- **The name** is the comment on the line just before the statement, with
  no blank line between, without its marker and spaces. The markers are
  the engine's line comments: `--` in SQL; `#` where the engine has it
  (MySQL and family, ClickHouse, BigQuery, Spanner, Redis and the JSON
  consoles); `//` where it has it (Snowflake, Dremio, Drill, Cassandra,
  MongoDB, Neo4j, Flux and the JSON consoles); `/` in KDB+'s q. With
  several comment lines, the closest one wins. Where the engine keeps the
  comment as part of the statement (MongoDB's and Neo4j's `//`, Redis's
  `#`), the comment lines the statement starts with name it the same way,
  and its lens goes over its first line of code. Block comments
  (`/* … */`), a comment after code on the same line and Db2's
  `--#SET TERMINATOR` lines don't name. A long name is kept whole: the tab
  shows it shortened, and hovering the tab shows all of it.
- **Its own tab.** A named statement's result goes to a result tab with its
  name, whatever ran it: the whole script, a selection, **Run the statement
  at the cursor** or its lens. Running a statement with that name again
  refreshes that tab in place instead of opening another one, and the tab
  keeps its place. Named tabs don't count toward **Results each editor
  keeps**: like pinned ones, they close only when you close them. A name is
  a statement's: a SQL Server or Sybase batch that holds several statements
  (a script without `GO` is one) takes none, and its results stay in the
  run's tab; **Run the statement at the cursor** or the lens runs one of
  them, which takes its name.
- **A script with named statements** runs as always, its progress live in
  the run's own tab. When it ends, each named statement's results, messages
  and errors move to its tab. What's left (statements without a name,
  messages of no statement) stays in the run's tab, which then shows its
  first statement without a name; if nothing is left, that tab closes and
  the run's first named tab shows. **Run again** on the run's tab runs the
  script again and splits it the same way. When two statements of a run
  have the same name, the first one gets the tab.
- **Run again** and **Export** on a named tab run only its statement, and
  the line of each message still takes you to the statement in the editor.
  On the run's own tab, which keeps only some of the script's results,
  **Export**, editing the grid, the jumps by foreign key and the chart over
  the whole result each read the statement its result came from, not the
  whole script.
- **Engines that get the whole text in one request.** Where the engine
  numbers the statements itself (MongoDB, Redis, Neo4j, Firebird and
  others), a script splits the same way, by the engine's own statements;
  any run there, one statement's too, shows its progress in a tab of its
  own, and its results move to their names' tabs when it ends. Where
  nothing tells which statement each result came from (Snowflake,
  BigQuery, InfluxDB 2 with Flux, DynamoDB, Solr, and plans on most
  engines, since a plan runs its text whole), a script's results stay in
  the run's tab, and only a run of one statement takes its name: the
  statement at the cursor, its lens, or a selection of one statement. Per
  engine: [engine-support.md](engine-support.md#named-results).
- **Rename** a tab with a double-click on it, F2, or **Rename** in its menu:
  Enter keeps the name, Esc cancels, and an empty name removes it. A
  renamed tab is named from then on: a statement with that name refreshes
  it. Another tab that had that name goes back to unnamed.

Names are saved with the results when DBine closes
([results-store.md](results-store.md)).

## Keyboard shortcuts

The keys of every action, the editor's included (**Run the selection or
everything**, **Run the statement at the cursor**, **Estimated plan**, **Run
with actual plan**, **Format**, **Comment or uncomment**, **Find and
replace**, **Go to line…**, **Autocomplete suggestions**), are changed in
**Settings › Keyboard shortcuts**:

- The actions are listed by group (General, View, Editor, Results…), each
  with its keys. The search box filters by action or by key.
- Click an action's combination and press the new one; Esc cancels. The
  change applies at once, in every editor and in the macOS menu, and
  travels with the rest of the settings.
- A combination another action uses where both act is allowed, but the row
  shows the conflict and names the other action. A system shortcut (copy,
  paste, undo…) is flagged too: it stops working where the action runs.
- Each row can remove its combination (the action is left without keys) or
  go back to the original one; **Reset all** restores every default.
- The editor's right-click menu shows its run, plan, format, comment, find
  and go-to-line actions with the keys they have now.
