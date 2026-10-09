# Script execution

A script with several statements runs statement by statement, with the
server's messages in order and live, the error on its line and the tab's
session intact. It applies to all engines that run text. The per-engine
differences are in
[`engine-support.md`](engine-support.md#script-execution).

## How the script is split

DBine splits the text with a single lexer, configured with the engine's
dialect. A `;` inside a comment, a quoted string, a quoted identifier or a
body (`$$ … $$`, `BEGIN … END`) never splits.

Each engine also has its own terminator, which DBine understands:

| Terminator | Engines | What it does |
|---|---|---|
| `GO` and `GO N` | SQL Server, Azure SQL, Babelfish, ODBC with T-SQL | Ends a batch. `GO N` repeats it N times. `GO 0` runs nothing and gives the error `GO: invalid repeat count` on its line. A `GO` inside a comment or a string doesn't split. |
| `DELIMITER` | MySQL and family | Changes the terminator; routine bodies stay whole. Manticore has no `DELIMITER`. |
| `/` on a line | Oracle | Ends a PL/SQL unit (`BEGIN`, `DECLARE`, `CREATE PROCEDURE`, function, package, trigger, type, query with `WITH FUNCTION`). Other statements end with `;` or `/`. |
| `SET TERM` | Firebird | Changes the terminator. Bodies (`EXECUTE BLOCK`, `CREATE OR ALTER`, packages) also run without it. |
| `--#SET TERMINATOR` | Db2 | Changes the terminator. |
| `$$`, `$tag$` | PostgreSQL and derivatives, DuckDB | Dollar-quoted bodies, with or without a tag. |

PostgreSQL and compatibles also split the script with psql's rules
(metacommands, `COPY` rows): see
[psql scripts](engine-support.md#psql-scripts-in-postgresql-and-compatibles).

## Continue on error

Each tab has the **Continue on error** switch. Off, the script stops at the
first statement with an error; on, it goes on with the next one. The initial
value depends on the engine:

- **Continues:** SQL Server, Babelfish, Oracle, PostgreSQL and derivatives
  (except CockroachDB), SQLite, Firebird, Cassandra, ScyllaDB, Couchbase,
  Redis, Valkey and Elasticsearch.
- **Stops:** the MySQL family, CockroachDB, ClickHouse, Flight SQL, Trino and
  Presto, MongoDB, Neo4j, etcd, Cosmos DB and CouchDB.

A fatal error ends the script even if the switch is on: in SQL Server,
severity 20 or higher (the server closes the connection, DBine opens another
and warns that the session state was lost); in MySQL, errors 1053, 1927 and
4031; in PostgreSQL, `\connect`. Cancelling also ends the script.

Engines that receive the **entire script in a single request** ("All the text
at once" mode: Snowflake, BigQuery, InfluxDB 2 with Flux and the ODBC ones
that need that mode) can't continue after an error, because the next
statement was never sent separately. The switch has no effect there.

A syntax error that prevents splitting the text (an unclosed quote in Redis,
an invalid line in Elasticsearch or MongoDB) rejects the whole script before
running anything.

## Messages

The **Messages** tab shows, in the order the server produced them and while
the script runs, not at the end:

- Server warnings and information: `PRINT`, `RAISERROR` up to severity 10,
  PostgreSQL's `NOTICE` / `WARNING` / `INFO`, Oracle's `DBMS_OUTPUT`, MySQL's
  `SHOW WARNINGS`, Neo4j notifications.
- A count per statement (`N rows affected`, the command tag) and the time it
  took.
- Errors, with the engine's code (`Msg 50000`, `ORA-`, SQLSTATE, MySQL
  number, gRPC code, Trino or Athena error name…), the server's text and the
  **script line**. The line is a link: one click takes the cursor there. When
  the server gives the position within the statement (syntax errors), the
  line is exact; if it doesn't, it's the first line of the statement.

## Run the statement under the cursor

`Cmd+Shift+Enter` (macOS) or `Ctrl+Shift+Enter` runs only the statement where
the cursor is, delimited with the same lexer rules.

## Transactions

Each tab has an **Auto** mode (each statement commits by itself) or
**Manual**. With manual transactions:

- An indicator next to the editor shows the state: no transaction, open or
  failed (when the engine leaves it unusable).
- **Commit** and **Roll back** close the transaction. Committing a failed
  transaction is rejected; rolling back works.
- If you close the tab, or change the tab's database, with a transaction
  open, DBine warns first.
- An `UPDATE` or `DELETE` without `WHERE` asks for confirmation before
  running. It only looks at SQL statements: the text of a `PROMPT`, a comment
  or the inside of a PL/SQL block aren't flagged.
- In Oracle, going back from Manual to Auto commits what's pending.

Which engines offer it and how, in
[`engine-support.md`](engine-support.md#script-execution).

## `USE` and changing database

A `USE` (or its equivalent: `ALTER SESSION SET CURRENT_SCHEMA` in Oracle,
`SELECT n` in Redis, `:use` in Neo4j, `USE` in MongoDB and Cassandra) changes
the tab's database: the tab shows it and follows it in later runs, without
reconnecting.

## Cancel

Cancel uses each engine's native mechanism and **keeps the session**: the
`SET`s, the temporary tables and the transaction continue. The script stops
and the next statement doesn't run, even if "Continue on error" is on.

Two exceptions, where cancelling closes the tab's session and the state (open
transaction, schema or `SET`) is lost:

- **Oracle:** it cancels by killing the session from another connection
  (`ALTER SYSTEM KILL SESSION`); the client has no interrupt call. It needs
  the `ALTER SYSTEM` privilege; without it, the cancellation is only recorded
  in the log.
- **Babelfish:** it cancels with a verified `KILL` of the backend, and the
  app opens a new session.

SQL Server cancels with a TDS attention and keeps the session.

## Query protocol (PostgreSQL)

Some gateways and proxies in front of a PostgreSQL only accept the simple
query protocol: they reject the extended one (prepared statements and
parameters) with an error like `0A000 Extended query protocol is not
supported by this gateway`. PostgreSQL-family connections have, in the
advanced section, **Query protocol**:

- **Automatic** (default): on connecting, DBine tries the extended protocol;
  if the server rejects it, the session continues on a new connection with
  the simple one, with nothing to configure.
- **Simple protocol only**: forces it, for a gateway that detection doesn't
  recognize.

With the simple protocol the editor and the explorer work the same, with two
differences: results don't show each column's type, and bulk transfer, data
comparison and table cloning aren't available (they need the extended
protocol) and say so when attempted.

## Contract (drivers)

Everything comes in through `crates/dbine-driver`, with default values that
keep the previous behavior:

- `Driver::script_dialect()`: the lexer's dialect (generic, PostgreSQL,
  MySQL, T-SQL, Oracle, Firebird, Db2).
- `Driver::split_script(text)`: the driver's own split; by default it uses
  the dialect's lexer. It returns `ScriptStatement { text, start, end, line,
  kind, repeat }` units, with `kind` in `Sql`, `Block`, `Batch` or
  `ClientCommand`.
- `Driver::script_mode()`: `PerStatement`, `Batches` or `Whole` (default).
- `Driver::script_defaults()`: `continue_on_error` and `confirm_unsafe_dml`.
- `Driver::supports_manual_transactions()` and, in `Session`,
  `transaction_state`, `set_autocommit`, `commit` and `rollback`.
- Results: `StatementResult` with `offset`, `line`, `tag` and `elapsed`;
  ordered messages `{level, text, code, line, statement}`; a statement's
  errors are `Error::Statement(ScriptError { message, code, sqlstate, offset,
  line, fatal })`.
- `QueryOutcome.database`: the database the session ended up in after a
  `USE`.

Plugin hosts answer the driver's own split with the `SplitScript` call; an
older host replies `Unsupported` and the app splits with the dialect.
