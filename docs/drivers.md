# How to write a DBine driver

Each engine lives in its own crate, in `crates/drivers/<name>`, and fulfils
the contract of `crates/dbine-driver` (`Driver` and `Session` traits). A crate
can serve several engines that share the protocol: the PostgreSQL one also
serves CockroachDB and Redshift, and the MySQL one MariaDB and StarRocks.
`crates/dbine-drivers` registers them, with one cargo feature per crate.

The UI is built from what each driver declares in `DriverInfo`:

- the connection form, from `fields`;
- the explorer folders, from `object_kinds`;
- the editor language, from `language` and `dialect`.

That's why adding an engine doesn't touch the frontend.

## Crate structure

```
crates/drivers/<name>/
  Cargo.toml     # name = "dbine-driver-<name>"; depends on dbine-driver (path = "../../dbine-driver")
  src/lib.rs     # pub fn drivers() -> Vec<Arc<dyn Driver>>
  src/...        # whatever modules are needed
```

- **`drivers()` is the only mandatory public API.** It returns one `Arc` per
  engine the crate serves. Each engine's id (`DriverInfo::id`) is stable,
  lowercase and without spaces: `postgres`, `cockroachdb`, `mongodb`…
- **Dependencies.** Use `workspace = true` for those already in
  `[workspace.dependencies]` (tokio, serde, serde_json, futures, async-trait,
  chrono, tracing, uuid, rusqlite, reqwest). Any other is declared with its
  version in the crate's `Cargo.toml`.
- **Errors.** `dbine_driver::Error` can't have `impl From<ClientError>`,
  because of the orphan rule. Each crate maps its errors with its own function
  (`fn err(e: X) -> Error`) and `.map_err(err)?`:
  - `Error::AuthFailed`: login rejected.
  - `Error::Connect`: the server can't be reached.
  - `Error::Query`: the server rejected the statement.
  - `Error::Unsupported`: what the engine doesn't offer.

  Messages the user sees are written in Spanish; if they come from the
  server, they're passed through as they are.

## What to implement

| Method | What it returns |
|---|---|
| `info()` | `DriverInfo`: name, family, language, port, form fields, object kinds. |
| `connect(cfg, database)` | A `Session`: **one** live connection to that database (keyspace, dataset, index…). No pools. With a connection timeout (15–20 s). |
| `server_version()` | Product and version in one line. |
| `list_databases()` | The level below the connection. If the engine has a single space, a single element (`["main"]`, `["default"]`) and `databases_label: ""`. |
| `list_objects()` | `DbObject`s with `kind` = one of the declared `ObjectKindInfo::id`. Exclude system objects. |
| `columns(obj)` | Columns or fields. If the engine has no schema, they're inferred from a sample (e.g. 100 documents): `data_type` is the observed type and `nullable` is `true` if it doesn't appear in all of them. |
| `definition(obj)` | The source code, a mapping or the definition in JSON; `None` if there's nothing to show. |
| `browse_query(obj, limit)` | The text, **in the driver's language**, that shows the object's first rows or documents. The UI runs it with `execute`. |
| `filtered_browse(browse, filters)` | (in `Driver`, with a default implementation) The `browse_query` query with the column filters applied: a `WHERE` in SQL/CQL with the dialect's quotes and literals, or the engine's native filter. `Error::Unsupported` with the reason if it can't be done; the UI then filters the loaded rows. |
| `execute(text, max_rows, out)` | Runs the whole script (see the result format below). |
| `interrupter()` | A function to cancel from another thread when dropping the session isn't enough. Examples: `KILL QUERY`, a cancel request, an HTTP `DELETE` of the job, or the interrupt of a blocking thread. |

### Results

All results are tabular (`QueryOutcome`):

- **Cell format.** Use `out.begin_result(columns)`, `out.push_row(cells, max_rows)`
  and `out.push_affected(n)`. `push_row` already bounds memory: the stream
  keeps being consumed, but only `max_rows` rows are stored.
- **Cell types.** Cells are JSON:
  - `null`, `bool`, numbers with `json_i64`/`json_u64`/`json_f64` (integers
    beyond 2^53 become strings);
  - decimals and dates as strings (dates in ISO format: `2024-01-31 13:45:00`);
  - binaries with `json_bytes`;
  - nested objects or arrays as compact JSON strings.
- **Documents** (Mongo, CouchDB, Cosmos, DynamoDB, Elastic, Solr):
  - one row per document;
  - the columns are the union of the top-level keys, in order of appearance;
  - nested values go as JSON strings.
- **Key-value** (Redis): columns depend on the command. `GET` returns
  `value`; `HGETALL` returns `field, value`; a scalar goes in a `result`
  column.
- **Server messages and warnings** go through `out.info(text)` and
  `out.warning(text)` as soon as they arrive: they stay in order in `out.log`
  and the UI shows them live. `out.messages.push` still works (it counts as
  `info`).
- **Errors and scripts.** If a statement fails, `execute` returns `Err`; what
  ran before stays in `out`. If the engine gives a code, SQLSTATE or position,
  return `Error::Statement` (`ScriptError::new(msg).with_code(…)
  .with_sqlstate(…).at_offset(…)` or `.at_line(…)`, relative to the received
  text; `.fatal()` if the script can't continue).
- **How a script is split.** Declare the dialect in
  `Driver::script_dialect()` (`ScriptDialect::postgres()`, `mysql()`,
  `tsql()`, `oracle()`, `firebird()`, `db2()` or `generic()` with changes):
  quotes, comments, blocks, `GO [N]`, `/`, `DELIMITER`, `SET TERM`.
  If you don't declare it, the preset for `DriverInfo`'s `dialect` is used
  (`ScriptDialect::for_hint`: `postgres`, `mysql`, `mssql`/`sybase`,
  `oracle`, `db2`; the rest, `generic()`). With that, `Driver::script_mode()`
  set to `PerStatement` (or `Batches` for T-SQL) makes the app run statement
  by statement, report each one and continue or stop on an error depending on
  the tab (`script_defaults()` gives the default behavior of the engine's
  tool). `Whole` (the default) passes the entire script to `execute`: for
  engines that need it in a single request. If the server accepts a single
  statement per request and the driver stays on `Whole`, split the script with
  `dbine_driver::sql::split_statements` (SQL) or by lines or documents.
  A `Whole` driver that splits the script itself has to behave like the app
  in the editor: in editor runs the app sets `out.continue_on_error`
  (`Some(true)`: continue after an error, like the engine's console; `None`:
  stop at the first one, which is what Users and permissions, Backups and the
  rest receive) and `out.progress_sink` (each statement that finishes, with
  its results, messages and errors, live). The `steps.rs` pattern of the
  non-SQL drivers (mongodb, neo4j, cassandra, redis…) solves it:
  `Step::start` before each statement and `step.end(out, r)?` after. When a
  statement changes the session's database (`use db`, `:use`,
  `USE keyspace`), set `out.database`: the tab follows it.
- **Manual transactions.** `Session::transaction_state()`,
  `set_autocommit(bool)`, `commit()`, `rollback()` and
  `Driver::supports_manual_transactions()`.

### Read-only

`cfg.read_only`:

- **SQL drivers:** the registry already wraps them in `ReadOnlySession`. If
  the engine supports it, also enforce it on the server side, for example
  with `SET SESSION TRANSACTION READ ONLY`.
- **The rest of the drivers:** they enforce it themselves. They reject with
  `Error::Query` the commands that write (an allowlist of read commands).

### Security

- Never concatenate user strings into catalog queries: use parameters, or the
  dialect's identifier quoting helper (`dbine_driver::sql`).
- Secrets (`password` and fields with `.secret()`) arrive in `cfg`; they're
  never logged.

## Connection fields

The shortcuts `Field::host()`, `port()`, `database()`, `username()`,
`password()`, `encrypt()`, `trust_cert()` and `read_only()` are stored in the
typed fields of `ConnectionConfig`. `Field::server_set()` is the full set.

Any other key (`region`, `project_id`, `auth_mode`, `api_key`,
`service_account_json`…) goes to `cfg.options` and is read with
`cfg.option("key")`. If it's a secret, it carries `.secret()`: the UI stores
it in the keychain.

## Tests

- **Unit tests** for the pure helpers: value conversion, query building,
  response parsing.
- **Integration test against a real server, if there's a Docker image.**
  - Put it in `tests/integration.rs`, marked `#[ignore]`, and have it read the
    URL from a `DBINE_TEST_<ENGINE>_URL` environment variable.
  - Containers: name `dbine-test-<engine>`, a high free host port, and remove
    it when done (`docker rm -f dbine-test-<engine>`).
  - **Never** touch containers that don't start with `dbine-test-`: they
    belong to other projects.
- To validate the crate:
  - `cargo check -p dbine-driver-<name>`
  - `cargo test -p dbine-driver-<name>`
  - `cargo clippy -p dbine-driver-<name>`

## Engines without a native Rust client

For engines that only have the vendor's JDBC or ODBC driver (DB2, Sybase,
Informix, Teradata, Hive, Vertica…), the `odbc` crate is used. It relies on
the driver manager (unixODBC on macOS and Linux, the native one on Windows)
and on the ODBC driver the user installs. In those cases the form asks for
the ODBC driver name or a DSN.

## Third-party native libraries: downloaded on use

A driver that relies on a large third-party native library (C or C++) doesn't
bundle it inside the app: it downloads it the first time someone uses it and
loads it at runtime. Whoever doesn't use that engine doesn't pay for its
weight. It's the same strategy as the built-in AI model.

Today it applies to DuckDB (`crates/drivers/duckdb/src/loader.rs`, about
31 MB less in the executable):

- **What is downloaded:** the official DuckDB build for the platform
  (`libduckdb-<platform>.zip` from its GitHub releases), with the version and
  SHA-256 fixed in the code. It includes `parquet`, `json` and `icu`, so the
  files preset works without further downloads.
- **When:** on the first `connect`. The download resumes if it's cut off, the
  SHA-256 is verified and it ends up in
  `<app data>/components/duckdb-<version>/`. On macOS only the machine's
  architecture is stored.
- **How the user sees it:** the connection node shows "Downloading DuckDB,
  this one time only… 45%" instead of "Connecting…". Progress arrives through
  the `component-download` event (`dbine_driver::runtime::report_progress`).
- **How it's loaded:** the `duckdb` crate is compiled with
  `loadable-extension`, which routes every C API call through a function
  table; `loader` fills it from the library with `libloading`
  (`src/api_table.rs`). The rest of the driver uses the crate as usual.
- **Without internet:** the `DBINE_DUCKDB_LIB` variable points to an
  already-downloaded library.
- **When updating DuckDB:** bump the `duckdb` crate, change `VERSION` and the
  sizes and SHA-256 of `ASSET` in `loader.rs`, and regenerate `api_table.rs`
  if it doesn't compile (the command is in its header).
- **Signed macOS:** if the app is signed with the hardened runtime, it needs
  the `com.apple.security.cs.disable-library-validation` entitlement to load a
  library signed by another team.

A new engine that depends on a third-party native library follows the same
path: the folder and the progress come from `dbine_driver::runtime`.

## Downloadable drivers

In release builds, each driver crate (except those in
`dbine_drivers::BUILT_IN`) is compiled as a separate program that the app
downloads on use. A new crate needs its feature in
`crates/dbine-plugin-host/Cargo.toml` too, and a new contract method needs
its forwarding in `crates/dbine-plugin`. Details:
[`on-demand-drivers.md`](on-demand-drivers.md).
