# Engine support

This document follows the main rule in `AGENTS.md`: every DBine feature has
to be available in every engine. It lists the exceptions, with their reason,
and the verification status of each engine. When a feature is added, its
section is added too.

## Structure, design and database operations

This section covers several features that share the same contract: the designer (new table, collection, index or key), DDL in the engine's language, the structure with foreign keys and indexes (for the ER diagram and the generator), templates for other objects, creating and dropping databases, and insert scripts (to copy, import and dump data).

Three features depend only on the app, not on each engine, so they work in all of them:

- **copying results** in 10 formats;
- **revealing a tab** in the explorer (double click) and **"Go to database"**;
- **import**: files are read in the app and each engine receives its own insert script.

**Tested end to end in the app:**

- **SQLite:**
  - structure with a foreign key and diagram;
  - full script (tables, trigger, view and 20,500 rows) restored into another database, with the same counts and totals;
  - CSV and Excel import of 20,000 rows;
  - designer that creates a table;
  - reveal the tab and "Go to database".
- **SQL Server 2022:**
  - create and drop databases;
  - DDL with identity, comments, a filtered index and a foreign key with CASCADE;
  - copy of the database with data, whose structure came out identical;
  - the identity keeps working after the load (`IDENTITY_INSERT`).
- **PostgreSQL 16:** copy with data and a view; sequences end up after the loaded values and the structure is identical.

### By engine

| Engine | Designer creates | Structure (foreign keys and indexes) | Create/drop database | Notes |
|---|---|---|---|---|
| SQL Server | table | yes | yes | Comments as `MS_Description`; `IDENTITY_INSERT` when dumping data. DEFAULT constraint names are not preserved. |
| PostgreSQL, TimescaleDB, YugabyteDB, KingbaseES | table | yes | yes | Sequences resynchronized after the data. |
| CockroachDB | table | yes | yes | `unique_rowid()` stays as the default value. |
| Redshift | table | yes (informational) | yes | DISTSTYLE/DISTKEY/SORTKEY options; no indexes. |
| Greenplum | table | no foreign keys | yes | DISTRIBUTED BY. |
| Denodo | — | no foreign keys | no | It is a virtual layer: no designer and no DDL. |
| MySQL, MariaDB, TiDB, OceanBase | table | yes | yes | Functional indexes are omitted. |
| SingleStore, StarRocks, Doris, Databend, GreptimeDB | table | no foreign keys | yes | StarRocks and Doris have key model, distribution and buckets. |
| Manticore | table | no foreign keys | no | — |
| SQLite | table | yes | no | A database is a file; foreign keys come back unnamed. |
| Oracle | table | yes | yes (as schema/user, 18c+) | Identity relocated after the data. When copying data to Oracle, an empty text or binary in a column that is not CLOB/BLOB stops the load: Oracle would store it as NULL. Time zone regions (`America/Argentina/Buenos_Aires`) and extended JSON scalars are only preserved from Oracle to Oracle; to other engines they travel as an offset (the same instant) and as standard JSON. Dates before year 1 travel as text with ` BC`. |
| Firebird | table | yes | yes (via API) | Scripts carry the base type, not the domain. |
| SAP HANA | table | yes | yes (as schema) | — |
| Aurora DSQL | table | no foreign keys | no | A single database. |
| Cloud Spanner | table (with interleaving) | yes | yes (admin API) | The descending order of the primary key is not read back. |
| DuckDB | table | yes (ART indexes) | yes (ATTACH/DETACH of files) | Autoincrement with sequences; foreign keys go inside the CREATE TABLE. The library is downloaded on the first connection (13 to 41 MB depending on the platform). |
| ClickHouse, Timeplus | table / stream | skip indexes; no foreign keys | yes in ClickHouse | ENGINE, ORDER BY, PARTITION BY, TTL and codecs; Timeplus with stream modes. |
| Trino, Presto, Starburst | table | no foreign keys or indexes | no (catalogs are configured on the server) | `WITH` properties are not read back. Starburst untested. |
| Athena | table (Iceberg or external) | no foreign keys or indexes | yes | Unit tests only. |
| BigQuery | table | yes (NOT ENFORCED keys) | yes (datasets) | No indexes or autoincrement. |
| Snowflake | table | yes | yes | Transient tables and clustering key. |
| Databricks | table (Delta) | yes | yes (catalogs) | No indexes; liquid clustering is not read back. |
| Phoenix | table | no foreign keys | no | Salting, column families, and local and global indexes. |
| ksqlDB | stream / table | — | no | Windows of windowed tables are not read back. |
| IoTDB | device | — | yes | Encoding and compression per measurement. |
| InfluxDB 1 / 2 / 3 | — | measurements, tags and fields | yes (databases or buckets) | Measurements are created when data is written: no designer and no INSERT. |
| MongoDB | collection (with `$jsonSchema` validator) | indexes; no foreign keys | yes | Capped collections, time series, clustered collections and TTL. |
| CouchDB | — (schemaless) | Mango indexes | yes | Views and indexes come from templates. |
| Cosmos DB | container | unique keys and composite indexes | yes | Extension: `CREATE CONTAINER` / `DROP CONTAINER`. |
| DynamoDB | table (keys, GSI and LSI) | indexes; no foreign keys | no (there are no databases) | Extension: `CREATE TABLE {json}`, `CREATE INDEX`, `DROP`. |
| Elasticsearch, OpenSearch | index (mappings) | mapping fields | no (there are no databases) | Insertion through `_bulk`. |
| Solr | collection / core | fields | no | Extension: `PUT` / `DELETE /solr/<name>`. In standalone mode, cores share the configset. |
| Redis, Valkey, Dragonfly | key (type, values, TTL) | — (keys are not tables) | no | Databases are a fixed numbered set. |
| Cassandra, ScyllaDB | table (partition and clustering) | indexes; no foreign keys | yes (keyspaces) | Materialized views and UDFs have to be enabled in cassandra.yaml. |
| ODBC (39 presets) | table | yes in most | depends on the preset | Only the generic preset was tested against a server (SQL Server via ODBC). NetSuite has no designer. |
| AlloyDB, Cloud SQL and Aurora PostgreSQL, EDB, Fujitsu | table | yes | yes | Same as PostgreSQL. Managed ones come with TLS and accept a CA certificate. |
| openGauss | table | yes | yes | No identity columns (they go as `serial`). Only accepts MD5 passwords: tokio-postgres implements neither SHA-256 nor SM3. |
| Cloudberry, Greengage | table | no foreign keys | yes | DISTRIBUTED BY, like Greenplum. |
| Materialize, RisingWave | table | no foreign keys | yes | No identity; Materialize without PRIMARY KEY. |
| CrateDB | table (shards and replicas) | no foreign keys or indexes | no | No COMMENT. |
| Yellowbrick, H2 (`-pg` mode) | table | yes | yes in Yellowbrick | H2 without TLS. |
| Aurora MySQL, Cloud SQL para MySQL | table | yes | yes | Same as MySQL, with TLS and CA certificate. Cloud SQL without client certificates: use the Cloud SQL Auth Proxy. |
| VeloDB | table | no foreign keys | yes | Same as Doris. |
| Azure SQL Database | table | yes | yes | SQL login or Microsoft Entra ID. |
| Fabric Warehouse | table | `NOT ENFORCED` keys, no indexes | no | Entra ID only; warehouses are created in the portal; no triggers. |
| Babelfish | table | yes | yes | T-SQL on top of PostgreSQL. |
| Oracle Autonomous | table | yes | as Oracle | Wallet with `ewallet.pem` or TLS string; `cwallet.sso` and `.p12` not supported. |
| Archivos CSV / Parquet / JSON | — (each file is a view) | — | no | Through DuckDB; not Excel (it needs an extension that is not bundled). |
| libSQL / Turso | table | yes | no (platform API) | No cancellation: Hrana HTTP has no cancel request. |
| Azure Databricks | table (Delta) | yes | yes (catalogs) | Same as Databricks, with token or Entra ID service principal. |
| Apache Calcite Avatica | — | — | no | No designer, DDL, definitions or monitor: they depend on the database behind the server. |
| FerretDB | collection | indexes | yes | No cancellation (`killOp` does not exist in FerretDB 2). |
| Amazon DocumentDB | collection | indexes | yes | TLS with the AWS bundle; `retryWrites` disabled. |
| Amazon Keyspaces | table | — | yes | No materialized views, indexes, UDFs or compaction options. Service-specific credentials: the CQL client has no SigV4. |
| Open Distro | index | mapping fields | no | No data streams or `flat_object`. |
| TimechoDB | device | — | yes | Same as IoTDB. |
| Neo4j, Memgraph | index / constraint | indexes and constraints | yes in Neo4j Enterprise | Cypher. Neo4j Community cannot create databases. |
| Amazon Neptune | — | — | no | openCypher over HTTPS; the engine has no indexes, constraints or user databases. |
| OrientDB | class (vertex, edge or document) | indexes; LINK as foreign key | yes | Gremlin only with the `-tp3` images. |
| Couchbase | collection (primary index and indexes) | indexes; no foreign keys | yes (buckets) | `bucket.scope` schema; `maxTTL` only in Enterprise. |
| TDengine | table / supertable (tags) | no foreign keys or indexes | yes | Views only in Enterprise. |
| Apache Drill | — (CREATE TABLE AS only) | no foreign keys or indexes | no (plugin workspaces) | No INSERT: there is no insert script. |
| Dremio | Iceberg table (PARTITION BY, LOCALSORT) | no foreign keys or indexes | yes (spaces) | Does not preserve NOT NULL. |
| etcd | key (value and TTL) | — | no (a single keyspace) | etcdctl commands; `watch`, `lock` and `elect` are rejected. |
| Arrow Flight SQL | — | depends on the engine | no | Generic (GizmoSQL, Dremio, InfluxDB 3, Doris). No "trust the certificate": tonic does not allow it. |

## Result export

Supported in **all engines**, with no exceptions.

**Formats:** JSON, JSON Lines/NDJSON, SQL (INSERT), CSV, semicolon-separated
CSV, CSV for Excel (UTF-8 with BOM, `;` and CRLF), TSV, Excel (.xlsx) and XML.

**Where:**
- **"Export" menu** of each result: writes the loaded rows right away.
- **Advanced export (⌘E):** adds the options of each format and the
  **all rows** option, which re-runs the query and writes the file as rows
  arrive. On a table, "all" means the whole table.

**How "all rows" works:**
- All drivers deliver their rows through `QueryOutcome::push_row`, and the
  export plugs a destination (`RowSink`) in there, with no changes to the
  drivers. Drivers that build a local result use `out.fork()` / `out.merge()`.
- The re-run happens in a **read-only session**, so an export never writes
  to the database.
- It can be cancelled, and the half-written file is deleted.
- Measured: 3 million rows from PostgreSQL, a 218 MB CSV, with no growth in
  the app's memory.

**Limitations:**
- Excel allows up to 1,048,576 rows per sheet; for more, CSV is better.
- In Elasticsearch/OpenSearch, "all rows" of an index is limited by the
  server's `index.max_result_window` (10,000 by default).

## Result charts

Supported in **all engines**, with no exceptions: charts work on the results
grid, which has the same format in all drivers. Each result has a
"Table | Chart" selector.

Chart types: bars, horizontal bars, stacked bars, lines, area, pie and
scatter. In addition, "Split by" builds one series per value of a column, and
there are aggregations (sum, average, count, minimum, maximum) and export to
PNG.

## Execution plans

The editor has three actions:

| Action | Shortcut | What it does |
|---|---|---|
| **Estimated plan** | ⌘L | Shows the plan without running anything, writes included. |
| **Run + plan** | ⌘⇧L | Runs the script and shows the results plus the plan with real figures. |

Rules common to all engines:

- Writes are never run twice.
- If the engine can only give real figures by running the statement again
  (`EXPLAIN ANALYZE`), the repeat is done with reads only. Writes show the
  estimated plan.

The diagram follows the SSMS style: the statement on the left, the operators
on the right, and arrows that get thicker the more rows pass through.

**Legend of the "Verified" column:**
- **live:** tested against a real server or an emulator in Docker.
- **fixture:** tested with recorded plans, without a server.
- **emulator without plans:** there is an emulator, but it does not generate real plans.

### Relational

| Engine | Estimated | Real | Verified |
|---|---|---|---|
| SQL Server | `SHOWPLAN_XML` | `STATISTICS XML` (runs once) | live |
| PostgreSQL, TimescaleDB, YugabyteDB, Greenplum, KingbaseES | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE`, reads only | live (PostgreSQL) |
| CockroachDB | `EXPLAIN (VERBOSE)` | `EXPLAIN ANALYZE` | live |
| Redshift | text `EXPLAIN` | estimated + results | fixture |
| MySQL 8 | `FORMAT=TREE` | `EXPLAIN ANALYZE` | live |
| MariaDB | `EXPLAIN FORMAT=JSON` | `ANALYZE FORMAT=JSON` | live |
| TiDB | `EXPLAIN FORMAT='brief'` | `EXPLAIN ANALYZE` | live |
| MySQL 5.7, OceanBase, SingleStore, StarRocks, Doris, Databend, GreptimeDB | tabular or text `EXPLAIN` | estimated + results | fixture |
| SQLite | `EXPLAIN QUERY PLAN` (no costs) | estimated + results | live |
| Oracle | `EXPLAIN PLAN` | cursor just executed; needs `SELECT_CATALOG_ROLE` | live |
| Firebird 3+ | `MON$EXPLAINED_PLAN` (only prepares the statement) | estimated + results | live (Firebird 5) |
| SAP HANA | `EXPLAIN PLAN` | estimated + results | fixture |
| Aurora DSQL | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE`, reads only | live (via PostgreSQL) |
| Cloud Spanner | `queryMode: PLAN` | `queryMode: PROFILE` | emulator without plans + fixture |
| Denodo, Manticore | **no** | **no** | Neither exposes plans through SQL in a usable format. |
| openGauss, Cloudberry, Greengage, AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE`, reads only | live (openGauss, Cloudberry, Greengage; the rest via PostgreSQL) |
| RisingWave, Materialize, CrateDB, H2 | text `EXPLAIN` | estimated + results | live |
| Babelfish | PostgreSQL plan as text | real | live |
| Azure SQL, Fabric | same as SQL Server | same as SQL Server | fixture (Fabric's real plan unverified) |
| Neo4j, Memgraph | `EXPLAIN` | `PROFILE` | live |
| Amazon Neptune | `explain=static` | `explain=dynamic` | fixture |
| OrientDB | `EXPLAIN` | `PROFILE` | live |

### Analytical and cloud

| Engine | Estimated | Real | Verified |
|---|---|---|---|
| DuckDB | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE` | live |
| ClickHouse, Timeplus | `EXPLAIN json=1, indexes=1` (SELECT only) | estimated + results | live (ClickHouse) |
| Trino, Presto, Starburst | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE` | live (Trino) |
| BigQuery | dry run: bytes and tables. There is no operator tree yet, because BigQuery builds the stages only when it runs. | job stages | emulator without plans + fixture |
| Athena | `EXPLAIN (FORMAT JSON)` | `GetQueryRuntimeStatistics` | fixture |
| Snowflake | `EXPLAIN USING JSON` (no costs or estimated rows) | `GET_QUERY_OPERATOR_STATS` | fixture |
| Databricks | `EXPLAIN FORMATTED` | query totals only; there are no per-operator metrics outside the Spark UI | fixture |
| ksqlDB | `EXPLAIN` | estimated + results | live |
| Phoenix | `EXPLAIN` | estimated + results | live |

### Documents, search, key-value and time series

| Engine | Estimated | Real | Verified |
|---|---|---|---|
| MongoDB | `explain` with `queryPlanner` | `explain` with `executionStats` (without applying writes) | live |
| CouchDB | `_explain` | `execution_stats` | live |
| Cosmos DB | gateway query plan | query and index metrics | emulator (response shape only) + fixture |
| Elasticsearch, OpenSearch | `_validate/query?explain` and `_sql/translate` | `profile: true` | live |
| Solr | `debug=query` with `rows=0`. Solr has no plan without searching, so the search runs but returns no documents. | `debug=timing` | live |
| Cassandra, ScyllaDB | access path based on the keys (partition, index, `ALLOW FILTERING`) | `TRACING` | live |
| DynamoDB | plan **deduced** from the keys: GetItem, Query or Scan. It is not an engine plan, because DynamoDB has none. | consumed capacity | live (DynamoDB Local) |
| InfluxDB 1 | `EXPLAIN` | `EXPLAIN ANALYZE` | live |
| InfluxDB 2 (Flux) | **no**: Flux has no estimated plan | `profiler` package | live |
| InfluxDB 3 | `EXPLAIN` | `EXPLAIN ANALYZE` | live |
| Redis | **no** | **no** | The engine has no execution plans. |
| IoTDB | **no** | **no** | The REST API (v1 and v2) rejects `EXPLAIN`; only the CLI client (Thrift) returns them. |
| Couchbase | `EXPLAIN` (JSON) | `profile: timings` (Enterprise only; in Community, estimated + metrics) | live (Community 8.0) |
| TDengine | `EXPLAIN VERBOSE` | `EXPLAIN ANALYZE`, reads only | live |
| Apache Drill, Dremio | `EXPLAIN PLAN INCLUDING ALL ATTRIBUTES` | query profile | live |
| Arrow Flight SQL | `EXPLAIN (FORMAT JSON)` in DuckDB; text in the others | `EXPLAIN ANALYZE`, reads only | live (GizmoSQL) |
| Apache Calcite Avatica | `EXPLAIN PLAN FOR` | estimated + results | fixture |
| etcd, FerretDB (real) | **no** | **no** | etcd has no plans; FerretDB returns only `queryPlanner`. |

### ODBC presets

Only the generic preset was tested against a server: SQL Server through
Microsoft ODBC Driver 18. The others were written from each vendor's
documentation and need their ODBC driver installed.

| Preset | Estimated | Real | Verified |
|---|---|---|---|
| generic (SQL Server) | `SHOWPLAN_ALL` | `STATISTICS PROFILE` | live |
| generic (other servers) | text `EXPLAIN`, if the server accepts it | estimated + results | — |
| Db2 LUW | `EXPLAIN PLAN` + explain tables (if missing, the error says how to create them) | estimated + results | fixture |
| Db2 z/OS | `PLAN_TABLE` / `DSN_STATEMNT_TABLE` (best effort) | estimated + results | fixture |
| Sybase ASE | `SHOWPLAN` + `NOEXEC` | showplan during execution | fixture |
| SQL Anywhere | `EXPLANATION()` | estimated + results | **untested** |
| Hive, Impala, Teradata, Vertica, Netezza, Dameng | the engine's `EXPLAIN` | estimated + results | fixture |
| Ocient | text `EXPLAIN` | estimated + results | **untested** |
| Exasol | **no**: it has no `EXPLAIN` | session profiling | fixture |
| CUBRID | **no**: the estimated plan only exists in `csql` | `SET TRACE ON` | fixture |
| Informix, GBase 8s | **no** | **no** | `SET EXPLAIN` writes the plan to a file on the server, not over ODBC. |
| Altibase | **no** | **no** | The plan only comes out through its iSQL client. |
| Db2 for i | **no** | **no** | It has no `EXPLAIN` through SQL; plans are seen with Visual Explain. |
| Ingres, Mimer, Caché, Zen, Access, dBase, NetSuite | **no** | **no** | The engine does not return the plan over ODBC. |
| OpenEdge | **no** | `_Sql_Qplan` after running | fixture |
| Ignite 3 | `EXPLAIN PLAN FOR` (syntax unverified) | estimated + results | **untested** |

### Pending verification

They are implemented but still have to be tested against a real server:

- **Cloud without an emulator:** Athena, Snowflake and Databricks. An account
  is needed.
- **Emulators that do not generate real plans:** BigQuery and Spanner.
- **Engines with a heavy Docker image or no image:** Redshift, HANA,
  StarRocks, Doris, OceanBase and SingleStore.
- **ODBC presets:** they need each vendor's ODBC driver.
- **Services without an image:** Aurora, AlloyDB, Cloud SQL, Azure SQL, Fabric,
  Oracle Autonomous, DocumentDB, Keyspaces, Neptune and Azure Databricks (unit
  tests only; those that share an engine were tested against that engine).
- **Flight SQL** against Dremio and InfluxDB 3; **TLS** in Couchbase, TDengine,
  Drill, Dremio, etcd and Flight SQL.

## Server monitor

Right click on the connection → **Monitor**. It opens a tab that queries the
server every 2, 5, 10 or 30 seconds (only while visible) with its own
session. It shows:

- cards by group (CPU, memory, connections, activity, network, disk, cache,
  storage, locks, replication);
- cumulative counters as a per-second rate;
- gauges when there is a cap, with a "high" (75 %) and "critical" (90 %) warning;
- tables of sessions, running queries, locks, waits, databases, largest
  objects, replicas and nodes, depending on the engine;
- at the bottom, what the engine cannot report and why.

All engines have it, except those in the exceptions table. A part that fails
because of permissions is skipped with a note: the rest of the dashboard
still appears.

### What each engine does not report

| Engine | What is missing | Reason |
|---|---|---|
| PostgreSQL and its family, MySQL, MariaDB, Cloud SQL, SingleStore, OceanBase, Doris/VeloDB, Databend, Manticore, Babelfish, Firebird | server CPU | The engine does not expose it through SQL. In managed ones it is in CloudWatch, Cloud Monitoring or the console. |
| Aurora, AlloyDB, Cloud SQL, DocumentDB, Keyspaces, Neptune, Aurora DSQL, DynamoDB | almost all resource metrics | They are only in CloudWatch or Cloud Monitoring. DynamoDB does not query CloudWatch: it adds a dependency and is billed per metric on every read. |
| Azure Cosmos DB, Fabric | CPU, memory, RUs consumed | Only in Azure Monitor or the Fabric Capacity Metrics app. |
| Athena, BigQuery, Databricks, Snowflake | CPU and memory | Services with no visible server: jobs, bytes scanned, slots, warehouses and credits are reported. The Snowflake monitor does not wake a suspended warehouse; the Databricks one does not run SQL, so the warehouse can shut itself down. |
| Cloud Spanner | everything in the emulator | The emulator has no `SPANNER_SYS` or Cloud Monitoring. |
| Cassandra, ScyllaDB | JVM CPU and memory / throughput | Only through JMX (Cassandra) or Prometheus on port 9180 (ScyllaDB). |
| CouchDB, Solr | host CPU (CouchDB), open connections, running queries | The API does not expose them. |
| Redis, MongoDB, InfluxDB | host CPU | They only report the CPU of the process itself. |
| IoTDB, TimechoDB | CPU, memory, disk and network | The DataNode's Prometheus endpoint has to be enabled (`dn_metric_reporter_list=PROMETHEUS`); the form has the "Metrics URL" field. |
| SQLite, DuckDB, libSQL, files | CPU and sessions | There is no server: size, pages, cache and memory are reported. |
| Neo4j Community, Memgraph Community | cache hit rate, QPS, activity | Only in the Enterprise editions. |
| OrientDB | CPU, memory, QPS | Only with the Enterprise profiler. |
| FerretDB | memory, connections, network, `top`, replicas | FerretDB 2 does not implement those `serverStatus` sections. |
| **No monitor:** Spark Thrift, Kyuubi, Actian Zen, Mimer, NetSuite, Apache Calcite Avatica, generic ODBC with an unknown engine | everything | The metrics are only in the engine's own UI or API (Spark UI, Zen Monitor, `sqlmonitor`), or the protocol does not expose them. |

### Tested against real servers

PostgreSQL 16, CockroachDB, TimescaleDB, YugabyteDB, openGauss, Cloudberry,
Greengage, CrateDB, RisingWave, Materialize, H2, MySQL 8.4, MariaDB 11.8, TiDB,
Manticore, GreptimeDB, StarRocks, ClickHouse, Timeplus Proton, SQL Server 2022
(native and via ODBC), Babelfish, Oracle 23ai, Firebird 5, libSQL, MongoDB 7,
FerretDB, Redis, Valkey, Dragonfly, Cassandra 5, ScyllaDB, CouchDB,
Elasticsearch, OpenSearch, Open Distro, Solr (standalone and cloud), InfluxDB 1,
2 and 3, IoTDB 1.3 and 2.0, DynamoDB Local, Cosmos DB emulator, Trino, Presto,
Phoenix, ksqlDB, BigQuery emulator, Neo4j (Community and Enterprise),
Memgraph, OrientDB, Couchbase, TDengine, Drill, Dremio, etcd and Flight SQL
(GizmoSQL). SQLite, DuckDB and files, with real files.

Unit tests only: Redshift, Yellowbrick, Denodo, KingbaseES, OceanBase,
SingleStore, Doris, VeloDB, Databend, HANA, Snowflake, Athena, Databricks,
Neptune and the ODBC presets without a driver on the machine.

## Engines that are not there yet

Known engines that DBine does not support, and why.

| Engine | Status | Reason |
|---|---|---|
| Google Firestore, Google Bigtable, Amazon Timestream, Salesforce, Salesforce Data Cloud | pending | Each one needs its own driver (REST or gRPC) and cloud authentication; it is still to be decided whether they get built. |
| Apache Kylin | pending | The REST API is simple, but the test image asks for about 10 GB of RAM. |
| Huawei GaussDB (managed) | no | It only offers SHA-256 or SM3 authentication, which the PostgreSQL client does not implement. openGauss is supported, with MD5 passwords. |
| Teiid | no | Discontinued project with no image to test it. |
| HSQLDB, Apache Derby, embedded H2 | no | They only have a Java client. H2 is supported through its PostgreSQL server mode (`-pg`). |
| DolphinDB | no | It has no ODBC driver of its own. |
| Raima RDM | by DSN | Its ODBC driver does not document a DSN-less connection: use the generic preset with a DSN. |
| Windows Management Instrumentation | no | It only exists on Windows and is not a database. |
| Jennifer, SnappyData, GemFire XD | no | Jennifer is an APM; SnappyData and GemFire XD are discontinued and only have JDBC. |

## Per-column filters on table data

The filter row under the headers (`Driver::filtered_browse`) is applied on
the server: the driver adds the filters to its own browse query. When an
engine cannot apply a filter, the UI filters the rows already loaded (up to
the chosen row limit) and says so in the filter bar, with the reason.

| Engine | How it filters | What is filtered over the loaded rows |
|---|---|---|
| PostgreSQL and variants, DSQL, SQLite, libSQL, DuckDB, SQL Server, Oracle, SAP HANA, Firebird, Athena, Trino, Flight SQL, Dremio | standard `WHERE` (`ILIKE` in PostgreSQL) | — |
| MySQL and variants, ClickHouse, TDengine, Drill, BigQuery, Spanner, Databricks, Snowflake, Phoenix, InfluxDB 3, IoTDB | `WHERE` with the dialect's quotes and literals | IoTDB: on `Time` only comparisons |
| Manticore | `WHERE` with `REGEX()` for text | "does not contain" |
| ODBC presets | `WHERE` according to the preset | — |
| ksqlDB | `WHERE` before `EMIT CHANGES` | topics (`PRINT`) |
| Cassandra, ScyllaDB, Keyspaces | `WHERE … ALLOW FILTERING` with `fromJson` | `<>`, NOT IN, text (needs a SASI/SAI index), nulls |
| DynamoDB | PartiQL with `begins_with` / `contains` | "ends with" |
| Cosmos DB | case-insensitive `CONTAINS` / `STARTSWITH` / `ENDSWITH` | — |
| Couchbase | SQL++ (`IS VALUED` for nulls) | — |
| OrientDB | `WHERE` with `.left()` / `.right()` / `.indexOf()` | — |
| MongoDB | `find` filter with operators and `$regex` | SQL conditions |
| CouchDB | Mango selector | views, SQL conditions |
| Elasticsearch, OpenSearch | `bool.filter` | SQL conditions |
| Solr | one `fq` per filter | SQL conditions |
| InfluxDB 1 (InfluxQL) | regex for text | nulls |
| InfluxDB 2 (Flux) | `filter()` after the pivot | SQL conditions |
| Neo4j, Memgraph, Neptune | `WHERE n.prop …` before the `RETURN` | the grid shows the whole node in one column, so in practice it filters over the loaded rows; SQL conditions |
| Redis, etcd | — | everything: whole keys or prefixes are read and there are no values to filter by |

SQL conditions ("SQL condition…") cannot be evaluated over the loaded rows:
in the engines that filter in the grid, they are ignored.

**Tested against real servers:** MySQL, Manticore, ClickHouse, Cassandra
5, CouchDB, MongoDB, Neo4j, OrientDB, Solr, InfluxDB 2 and 3, TDengine, IoTDB,
DynamoDB Local, the Spanner emulator, Elasticsearch 8, Couchbase and Drill.
**Pending verification with a server:** BigQuery (the emulator evaluates
LIKE wrongly), Databricks, Snowflake, Cosmos DB, ksqlDB, Phoenix and the
Hive and Spark ODBC presets.

## Cell editing (update code)

When you edit cells in the results grid, DBine generates the code that applies
the change. **It does not run it**: it adds it to the query or opens it in a
new one (`Driver::update_script`).

- **Which results can be edited:**
  - an object's data tab;
  - a query that reads from a single table (no JOIN, UNION, GROUP BY or WITH);
  - in MongoDB, a `db.<collection>.find(…)`.
- **Which row is updated:**
  - by the primary key, which has to be in the result;
  - if the table has no key, by all columns, and the UI warns about it.

Shape of the code per engine:

| Engine | Generated code |
|---|---|
| SQL (all relational and analytical engines with UPDATE) | `UPDATE … SET … WHERE <key>;` with each engine's quotes and literals (`N'…'` in SQL Server, typed literals in Trino, etc.) |
| ClickHouse | `ALTER TABLE … UPDATE … WHERE …;` (mutation) |
| Timeplus | `ALTER STREAM … UPDATE …` (ClickHouse syntax, untested against a server) |
| Phoenix | `UPSERT INTO … (key, columns) VALUES (…);` |
| TDengine | `INSERT` with the same timestamp, which overwrites the row |
| IoTDB | `INSERT` at the same `Time`; for a NULL, `DELETE` of the point |
| ksqlDB (tables) | `INSERT` with the whole row: replaces the key's value |
| MongoDB | `db.getCollection(…).updateOne({ _id }, { $set: {…} })` |
| CouchDB | an update function (`_design/dbine_update`) and a `PUT …/_update/set/<id>` per document; it does not need `_rev` |
| Couchbase | `UPDATE … USE KEYS … SET …` |
| Cosmos DB | `UPDATE "c" SET {…} WHERE {"id": …}`, a DBine-specific statement that merges the fields and saves with an `_etag` check |
| DynamoDB | PartiQL `UPDATE … SET … REMOVE … WHERE <key>` |
| Elasticsearch / OpenSearch | `POST /<index>/_update/<id>`; on data streams, `_update_by_query` |
| Solr | atomic update `{"id": …, "field": {"set": …}}` |
| Redis / Valkey / Dragonfly | `HSET`/`HDEL`, `ZADD`/`ZREM` or an `EVAL` depending on the key's type |
| etcd | `put` (and `del` + `put` if the key changes) |
| Cassandra / ScyllaDB / Keyspaces | `UPDATE … SET … = fromJson(…) WHERE <primary key>;` |
| Neo4j / Memgraph / Neptune | `MATCH … SET n.prop = …` |
| OrientDB | `UPDATE Class SET … WHERE @rid = …` |

**Not supported:**

| Engine | Reason |
|---|---|
| InfluxDB 1/2/3 | Rewriting a point is a line protocol write, which neither Flux, nor InfluxQL, nor v3's (read-only) SQL can express |
| Apache Drill | It has no UPDATE: tables are only recreated with CTAS |
| GreptimeDB | It has no UPDATE: a row is replaced by inserting it again in full |
| NetSuite (ODBC) | SuiteAnalytics Connect is read-only |
| ksqlDB (streams) | Streams only accept appending events |
| Redis streams | Stream entries cannot be modified |
| CouchDB views | They are computed results, not documents |

**Cases the engine rejects even though the code is generated:**

- Manticore does not accept assigning NULL.
- Cloud Spanner, Solr, TDengine and IoTDB need the full key.
- Phoenix does not allow changing the key.
- On OrientDB's `@` attributes and on etcd's revision and lease columns, the
  driver does not generate the change and explains why.
- Some engines only update certain table types: Hive the ACID ones, Impala
  the Kudu ones, and StarRocks and Doris the primary-key ones. There the code
  is generated anyway and the engine raises the error when it runs.

### Deleting rows

Right click on a row (or on several selected ones), or Delete/Backspace on
the selected rows, marks them for deletion: they show struck through and the
same item turns into "Restore row". Edits to a marked row do not count.
"Save" shows a single script with the deletes and the updates, in the
engine's language (`Driver::delete_script`, the same one used by data
comparison), and nothing runs without that click.

- **Which row is deleted:** by the primary key; with no key, by all the
  columns, with the same warning as when editing. Binary or long columns
  (BLOB, bytea, CLOB, XML, geometries…) are left out of that WHERE because
  their value cannot be compared; if the primary key has one of those values,
  the row cannot be deleted from the grid. A NULL in the key goes as
  `IS NULL`.
- **Order:** deletes first and then updates. Rows do not overlap (a marked
  row loses its edits), and deleting first prevents an UPDATE from leaving a
  row equal to another one that was about to be deleted (with no key, the
  DELETE would take both) or from colliding with a unique value that is about
  to disappear.
- The same rules as for editing apply: read-only connections and non-editable
  results.

Engines that do not delete rows (the item shows disabled with the reason):

| Engine | Reason |
|---|---|
| Apache Drill | It has no DELETE: tables are only recreated with CTAS |
| ksqlDB | It has no DELETE: a table row is deleted with a tombstone written directly to the Kafka topic, and streams only accept appending events |
| InfluxDB 2 (Flux) | Flux does not delete points: they are deleted with the `/api/v2/delete` API, by time range and predicate on the tags |
| InfluxDB 3 (SQL) | Its SQL is read-only and does not delete individual points |
| NetSuite (ODBC) | SuiteAnalytics Connect is read-only |

**Cases the engine rejects per row** (the reason shows in the changes bar):
Manticore with a NULL key; TDengine needs the timestamp as the only key (and
`tbname` on a supertable); Solr, Elasticsearch, CouchDB, Cassandra, Cosmos DB
and DynamoDB need the key or the full document id.

### Adding rows and documents

The "Add row" button in the results bar, or the item of the same name in the
context menu, adds a new row at the end of the grid. Its row number shows "+"
and the row is highlighted. Cells that are not touched show "(default)" and
are left out of the INSERT, so the column takes its default value or the
engine generates it (identity, auto-increment, MongoDB's `_id`). "Set NULL"
puts an explicit NULL. "Remove the new row", or Delete with the row selected,
discards it outright.

Document engines (MongoDB and its compatibles, such as FerretDB, Amazon
DocumentDB or Cosmos DB for MongoDB, plus CouchDB, Elasticsearch/OpenSearch
and Solr) show "Add document". It opens a JSON editor with the result's
fields already loaded: an object adds one document and an array adds several.
It also works on an empty collection, which has no columns in the grid. Any
engine whose result has no columns gets the same dialog, titled "Add row".

The code for the new rows goes together with the pending changes, in the same
bar as the edits and the deletes. "Save" shows the full script and runs it
only with that click; "Add to the query" sends it to a query. It is in each
engine's insert language, the same one used by copy and import: `INSERT` in
SQL engines, `insertMany`, `_bulk`, Redis commands, Cypher `CREATE`, etc.

- **Order:** DELETEs first, then UPDATEs and INSERTs last.
- **Batches:** rows that set the same columns go in a single INSERT.
- **Explicit identity:** if a new row sets an identity column, the SQL Server
  script wraps it in `SET IDENTITY_INSERT … ON/OFF`, and PostgreSQL moves the
  sequence past the inserted value.
- **IoTDB:** the new row needs the Time column.
- The same rules as for editing apply: read-only connections behave the same
  as with cell editing.

Engines that do not add rows (the button shows disabled and the tooltip gives
the reason):

| Engine | Reason |
|---|---|
| Apache Drill | It has no INSERT: tables are only recreated with CTAS |
| InfluxDB 1 / 2 / 3 | Points are written with line protocol, and no query language of the engine expresses it |

## JSON tree view

The "{ }" button in the results bar, next to table and chart, shows the
result as a tree: one node per row (a document), with its fields below and
nested objects and arrays down to the last level. It is available in all
engines: in document engines the nested fields arrive as JSON and are
expanded, and in SQL engines `json` / `jsonb` columns are expanded (and any
text that is a JSON object or array). In document engines DBine remembers
the last view chosen, table or tree.

- **Large results:** only the visible nodes are drawn and a node's children
  are built when it is opened. Arrays of more than 100 elements open in
  groups (`[0 … 99]`, `[100 … 199]`…). "Expand all" stops at 20,000 visible
  nodes and says so.
- **Types:** each field shows its type (String, Int, Double, Boolean, Null,
  Object, Array, ObjectId, Date). At the first level it comes from the
  column's type; at nested levels, from the shape of the value.
- **Search** in keys and values: only the documents with matches remain,
  opened down to each one and highlighted; Enter and ⇧Enter step through the
  matches.
- **Keyboard:** arrows to move, → opens and ← closes or goes up to the
  parent, ⌘C copies the value.
- **Context menu:** copy the value, the path (`customer.contact.email`) or
  the whole document as JSON, see the full value, expand everything below
  and, on first-level fields, filter the data by that value.
- **Editing**, with the same rules as the grid and sharing its pending
  changes: double click, Enter, F2 or starting to type edits a value, nested
  ones too; "Edit as JSON…" replaces an object or an array; "Remove this
  field" removes a field or a nested element; Delete marks the document for
  deletion. A number stays a number and a boolean, a boolean. `_id` cannot
  be edited.
- **How a nested change is saved:** the whole first-level field is rewritten
  (for example, `customer` with its new `contact.email`), so each engine's
  update code applies it the same way. In document engines it goes as an
  object, also when the field is edited from the table.

## AI assistant

It works with **all engines**. The prompt tells it each engine's query
language (SQL with its dialect, CQL, JSON/Mongo, Redis, Flux or Cypher) so
that it writes in the right syntax.

The database structure it receives as context comes from `database_schema`.
In an engine that does not implement it, the assistant answers without
structure and tells the user so; there are no other differences. Details in
`docs/ai-assistant.md`.

## Server-enforced reads (MCP and the AI assistant)

`run_query` and `explain` from MCP clients, and the AI assistant's reads,
run without asking only where the server enforces the read
(`Session::run_read_only`: one statement, in a read-only transaction rolled
back afterwards). Everywhere else the user approves each query first (the
MCP dialog, or the chat card for the assistant), and it then runs on the
read-only session with DBine's guard. The guard is defense in depth on both
paths, not the boundary.

| Engine | Reads enforced by the server | How |
|---|---|---|
| PostgreSQL, CockroachDB, Redshift, Greenplum, YugabyteDB, TimescaleDB, AlloyDB, Cloud SQL, Aurora PostgreSQL, Fujitsu Enterprise Postgres (and the other variants listed in `Variant::enforces_read_only`) | yes | `START TRANSACTION READ ONLY`, the statement through the extended protocol (one statement per Parse), `ROLLBACK`. Not with a connection set to the simple query protocol |
| MySQL 5.6.5+, MariaDB 10.0+ | yes | `START TRANSACTION READ ONLY`, one statement, `ROLLBACK` |
| SQLite | yes | One statement, on a connection that can't write, under an authorizer that only allows reading |
| libSQL / Turso | yes (servers with Hrana 3) | The server checks the statement is a read, between `BEGIN` and `ROLLBACK` on a stream of its own |
| EDB, KingbaseES, openGauss | no: approval | Autonomous transactions commit outside the read-only transaction |
| Denodo, RisingWave, Materialize, CrateDB, H2 | no: approval | No read-only transaction the server enforces |
| The other engines of the MySQL driver | no: approval | Not verified to enforce read-only transactions |
| Every other engine | no: approval | Not implemented in its driver yet (`Unsupported`) |

A driver downloaded before this version doesn't know the call and answers
`Unsupported`: its reads ask for approval until it is updated.

Pending: for every engine in the last row that has a read-only mode the
server enforces, implementing `run_read_only` in its driver is an explicit
pending item; until then its reads are approved by the user.

## Format code

The **Format** button of the query (⇧⌥F) formats the selection, or everything
if nothing is selected, in a single step that ⌘Z undoes. The formatters are
loaded the first time they are used (`web/src/composables/formatCode.ts`).

| Language | Engines | How |
|---|---|---|
| SQL | all SQL engines | `sql-formatter` with the engine's dialect: T-SQL (SQL Server, Sybase; `GO` batches are formatted one by one), PostgreSQL, MySQL, MariaDB, SQLite, PL/SQL (Oracle), Db2, Hive, Spark (Databricks), Trino, Snowflake, BigQuery, Redshift, N1QL (Couchbase), DuckDB, TiDB, SingleStore, ClickHouse; generic SQL for the rest. The library's `{{parameters}}` are left alone |
| CQL | Cassandra, ScyllaDB, Keyspaces | generic `sql-formatter` |
| JSON / MongoDB | MongoDB and compatibles, Elasticsearch, Cosmos DB and engines with JSON queries | indented JSON if it is JSON; otherwise `js-beautify` (the MongoDB shell is JavaScript) |

**No formatter** (the button is disabled and explains why): Redis (one command
per line, nothing to format), Flux (InfluxDB 2) and Cypher (Neo4j, Memgraph,
Neptune): there is no reliable formatter for the browser.

## Profiler

Right click on a database › **Profiler** opens a tab with every query that
any client runs against that database, live, like SQL Server's Profiler:
time, duration, text, database, user, client (host or address), application,
rows and error. The application comes from what the engine reports (SQL
Server's program name, PostgreSQL's `application_name`, MongoDB's `appName`,
MySQL's `program_name`…); where it does not report it, it stays empty.

In the tab, the row under the headers filters what is captured: while a
filter is set, only the queries that pass are kept. Duration and rows accept
`> 500`, `< 1s`; text, `contains`, `!doesn't contain`, `=equals`; user, client
and application are chosen from the values seen. Right click on a row filters
by its user, client, application or database, or with **Filter the ones like this query**: the
same text with other values (without literals, numbers or comments). With
that filter it shows how many times it ran and its average, minimum, p95 and
maximum duration. The contract is in `crates/dbine-driver/src/profiler.rs`
and each driver implements it in its `src/profiler.rs`.

There are two modes:

- **Full:** the engine records every query (Extended Events, MongoDB's
  profiler, a history or a query log) and DBine reads what is new every
  second.
- **Sampling:** the engine only shows what is running, or each connection's
  last query. DBine looks every 100 ms and reports each query once, when it
  finishes. Queries shorter than that interval may not appear; the tab says
  so.

**Changes on the server.** If an engine needs something turned on to
capture, DBine turns it on at start, the tab shows it in yellow, and restores
it on stop, on closing the tab or on closing the app. On read-only
connections it changes nothing: it uses what is already available or
explains why it cannot. There is only one active profiler per connection, so
that the one that stops first does not restore what the other still uses. If
the app closes abruptly, the next start cleans up the Extended Events
session that was left; in the other engines the setting stays on (the next
profiler finds it that way and does not touch it) and has to be reverted by
hand.

**Start.** A tab opened from the menu starts by itself. A tab restored when
the app reopens waits for "Start", because starting can change the server's
configuration.

### By engine

| Engine | Mode | Source | What it turns on (and restores) |
|---|---|---|---|
| SQL Server, Managed Instance | full | Extended Events (batches, RPC and errors), filtered by database | An Extended Events session; needs ALTER ANY EVENT SESSION. Without that permission or in read-only: sampling of `sys.dm_exec_requests` |
| Azure SQL Database | full | Extended Events at database level | Same as SQL Server |
| Fabric Warehouse | sampling | `sys.dm_exec_requests` | — (it has no Extended Events) |
| Babelfish | sampling | `pg_stat_activity` + `sys.dm_exec_sessions` | — |
| PostgreSQL and compatibles (Aurora, AlloyDB, Cloud SQL, EDB, Fujitsu, KingbaseES, openGauss, TimescaleDB, YugabyteDB, Greenplum, Cloudberry, Greengage) | sampling | `pg_stat_activity` | — (without superuser or `pg_read_all_stats`, only your own queries) |
| CockroachDB | sampling | `crdb_internal.cluster_queries` (the whole cluster) | — |
| Redshift | full | `sys_query_history` | — |
| Yellowbrick | full | `sys.log_query` | — |
| CrateDB | full | `sys.jobs_log` | — |
| Materialize | full | `mz_internal.mz_recent_activity_log` | Raises the statement logging sample rate to 1; only the `mz_system` role can. Otherwise it explains why it does not capture |
| H2 | sampling | `INFORMATION_SCHEMA.SESSIONS` | — |
| MySQL, Aurora MySQL, Cloud SQL | full | `performance_schema.events_statements_history_long` | The Performance Schema consumers that were off. In read-only: the last 10 queries per connection |
| MariaDB | sampling | `information_schema.PROCESSLIST` | — (Performance Schema is only enabled by restarting) |
| TiDB | full | `CLUSTER_SLOW_QUERY` | `tidb_slow_log_threshold` at 0. In read-only: sampling of `CLUSTER_PROCESSLIST` |
| OceanBase | full | `GV$OB_SQL_AUDIT` | — |
| StarRocks, Doris, VeloDB | sampling | `SHOW FULL PROCESSLIST` (only the connected frontend) | — |
| SingleStore | sampling | PROCESSLIST | — |
| Databend | sampling | `system.processes` | — |
| Manticore | sampling | `SHOW THREADS` (searches longer than ~100 ms) | — |
| GreptimeDB | sampling | `information_schema.process_list` | — |
| Oracle, Autonomous Database | sampling | `V$SESSION` + `V$SQL` + `V$SQLSTATS` | — (needs SELECT_CATALOG_ROLE; ASH/AWR are not used because of their license) |
| Firebird | sampling | `MON$STATEMENTS` | — |
| SAP HANA | full | `M_EXPENSIVE_STATEMENTS` | The expensive statements trace (`global.ini`), with a 1 µs threshold. If it cannot: sampling of `M_ACTIVE_STATEMENTS` |
| ClickHouse, Timeplus Proton | full | `system.query_log` | — (it asks for `SYSTEM FLUSH LOGS`; without that permission queries arrive with up to ~7.5 s of delay) |
| Trino, Presto, Starburst | full | the coordinator's query list (`/v1/query`, ~100 recent) | — |
| TDengine | sampling | `performance_schema.perf_queries` (the whole server; only queries longer than ~1 s) | — |
| MongoDB | full | `system.profile` | Profiling level 2 and `sampleRate` 1. In read-only it reads what is already recorded, or samples `currentOp` if it is off |
| MongoDB Atlas (shared tier), Cosmos DB, mongos, DocumentDB | sampling | `currentOp` | — |
| Redis, Valkey, Dragonfly | full | `MONITOR` (no durations; it has a cost on heavily loaded servers) | — |
| Cassandra 4.0+ | sampling | `system_views.queries` of each node (all keyspaces) | — |
| ScyllaDB | full | `audit.audit_log` | The audit categories and keyspace; requires Scylla started with `audit: table` |
| Couchbase | full | `system:completed_requests` | `queryCompletedThreshold` at 0 |
| Elasticsearch, OpenSearch, Open Distro | sampling | `_tasks` of running searches | — |
| Neo4j | sampling | `SHOW TRANSACTIONS` | — |
| Memgraph | sampling | `SHOW TRANSACTIONS` (the whole server) | — |
| Neptune | sampling | openCypher, Gremlin and SPARQL status pages | — |
| InfluxDB 1.x | sampling | `SHOW QUERIES` | — |
| InfluxDB 3 | full | `system.queries` (the whole server) | — |
| IoTDB, TimechoDB | sampling | `SHOW QUERIES` | — |
| Dremio | full | `sys.jobs_recent` | — |
| Apache Drill | full | REST API profiles | — |
| Snowflake | full | `INFORMATION_SCHEMA.QUERY_HISTORY` (every 3 s; keeps the warehouse on) | — |
| BigQuery | full | `jobs.list` API (needs `bigquery.jobs.listAll` to see everyone's) | — |
| Athena | full | the workgroup's `ListQueryExecutions` (no user) | — |
| Databricks | full | Query History API (the whole workspace) | — |
| Cloud Spanner | sampling | `SPANNER_SYS.OLDEST_ACTIVE_QUERIES` | — |

### CPU, reads and writes

When the engine reports them, the tab adds the **CPU**, **Reads** and
**Writes** columns. They appear only if the engine reports that figure, and
the unit is in the header's tooltip (each engine counts differently: pages,
rows, bytes, documents). With the "same queries" filter on, the summary shows average,
minimum, p95 and maximum of each figure, in addition to the duration.

| Engine | CPU | Reads | Writes |
|---|---|---|---|
| SQL Server (Extended Events) | yes | `logical_reads` (pages) | pages |
| SQL Server (sampling, DMVs) | yes, while it runs | yes, while it runs | yes, while it runs |
| MySQL 8.0.28+ | yes: turns on the `events_statements_cpu` consumer while profiling and turns it off on stop; in read-only no CPU is shown | rows examined | no |
| MariaDB | no | rows examined | no |
| TiDB | no | keys (`Process_keys`) | keys (`Write_keys`) |
| Databend | no | bytes read | bytes written |
| Oracle | yes | `BUFFER_GETS` (blocks) | `DIRECT_WRITES`: direct writes only (normal DML is written by DBWR later) |
| SAP HANA | yes | no | no |
| Firebird | no | page fetches (pages) | page marks (pages) |
| ClickHouse, Timeplus Proton | yes | rows read | rows written |
| Snowflake | no | bytes scanned | bytes written |
| BigQuery | "CPU" is slot time (can exceed the duration) | bytes processed | no |
| Databricks | "CPU" is total task time | bytes read | bytes written |
| Athena | no | bytes scanned | no |
| Trino | yes | rows processed | bytes written |
| Presto | yes | rows | no |
| Dremio | yes | rows scanned | no |
| InfluxDB 3 | compute time | no | no |
| MongoDB | yes (Linux servers only, 6.3+) | documents examined | documents written |
| MongoDB in sampling (`currentOp`) | no | no | no |
| Couchbase | yes | documents read | documents written |
| OpenSearch | yes | no | no |
| Neo4j | if `db.track_query_cpu_time` is on | pages | no |

**No figures**, and why:

| Engine | Reason |
|---|---|
| PostgreSQL and compatibles | `pg_stat_activity` has no per-statement figures; `pg_stat_statements` aggregates by query shape, not by execution |
| Babelfish | It samples `pg_stat_activity`: same case as PostgreSQL |
| Cloud Spanner | `OLDEST_ACTIVE_QUERIES` does not have them; `QUERY_STATS` is per minute and per query shape |
| Apache Drill | They would be in each query's full profile: an extra request per statement is needed |
| TDengine | `performance_schema.perf_queries` only has the elapsed time, the number of subqueries and connection data |
| IoTDB | `SHOW QUERIES` only returns the id, the DataNode, the elapsed time and the statement |
| InfluxDB 1.x | `SHOW QUERIES` only gives the duration and the state |
| Elasticsearch | The tasks API has no CPU or I/O (OpenSearch does report CPU) |
| Cassandra | `system_views.queries` only has the queued time and the running time |
| ScyllaDB | The audit log has no costs, not even the duration |
| Memgraph, Neptune | `SHOW TRANSACTIONS` and the status APIs have no CPU or I/O |
| Redis, Valkey, Dragonfly | `MONITOR` only gives the command |

### Engines without profiling

| Engine | Reason |
|---|---|
| SQLite, DuckDB, local libSQL | They are embedded databases: queries from other processes do not go through any server that shows them |
| Remote libSQL (Turso) | Its protocol does not expose other clients' queries |
| Aurora DSQL | It has no `pg_stat_activity` or query history |
| InfluxDB 2 (Flux) | It does not implement `SHOW QUERIES` and the query log only goes to the server's files |
| FerretDB | It has no `profile` command and its `currentOp` does not show the query, the client or the user |
| Amazon Keyspaces | Other clients' queries are only left in CloudTrail |
| DynamoDB, Cosmos DB (NoSQL) | Only in CloudTrail or Azure Monitor, outside the protocol |
| CouchDB, etcd, OrientDB, Phoenix, Solr, ksqlDB, Flight SQL | Their protocol has no way to see other clients' queries |
| Cassandra 3.x | `system_views` exists since 4.0 |
| ODBC presets | Depends on the engine behind each preset: pending, preset by preset |

### Tested against real servers

With two sessions: one captures and the other runs a slow and a fast query.
The test verifies that each one appears only once, the slow one with its
duration, and that the profiler's own queries do not leak in. When the driver
turns something on in the server, it also verifies that it ends up restored.

- **Passed:** PostgreSQL 16, CockroachDB, YugabyteDB, CrateDB, H2, SQL Server
  (Extended Events and read-only sampling), Babelfish, Oracle, Firebird,
  MySQL 8.4 (and read-only), MariaDB 11, TiDB, StarRocks, Manticore,
  GreptimeDB, ClickHouse, Proton, Trino, Presto, TDengine, MongoDB 7 (and
  `currentOp` sampling), Redis 7, Valkey, Dragonfly, Cassandra 5,
  ScyllaDB, Couchbase 8, Elasticsearch 8, OpenSearch 2, Open Distro, Neo4j 5
  (Community and Enterprise), Memgraph, InfluxDB 1.8 and 3, IoTDB 1.3 and 2.0,
  Dremio 26, Drill 1.22 and the BigQuery emulator.
- **Materialize:** the container does not allow turning on statement logging
  (`mz_system` is needed); it was tested that it explains this.
- **Untested** (no container or no account): Azure SQL, Fabric, HANA,
  Redshift, Yellowbrick, OceanBase, SingleStore, Doris, VeloDB, Databend,
  Starburst, DocumentDB, Neptune, TimechoDB, Snowflake, Athena, Databricks and
  Cloud Spanner (its emulator has no `SPANNER_SYS`).

## Compare schemas

Compare works in all engines that read their structure
(`database_schema`). Applying the changes, that is "Sync"
(`Driver::sync_script`), depends on what each engine lets you change with
DDL. What an engine cannot do is left as a warning in the script, not as a
statement that fails. How to use it: `docs/schema-compare.md`.

### What is compared in each engine

Besides columns, keys and indexes by columns, each engine compares the
following.

**PostgreSQL and compatibles.** Indexes with `INCLUDE`/`STORING`, any method
(`gin`, `gist`, `brin`, `hash`, `spgist`, `bitmap`, `ubtree`, `hnsw`…),
options (storage parameters, `NULLS NOT DISTINCT`, `DESC`, `NULLS`, operator
class, `COLLATE`), expression indexes (including full-text ones with
`tsvector`) and `EXCLUDE` as an index type. `CHECK`. Sequences (not identity
or `serial`), enum, domain, composite and range types, synonyms in openGauss,
EDB and KingbaseES. Materialize: list, map and record types. H2: sequences
and domains. Yugabyte: `HASH`, `DESC`, `ybgin`. RisingWave: `DESC`,
`INCLUDE`, `DISTRIBUTED BY`. CrateDB: full-text indexes (with analyzer and
`INDEX OFF`) and `CHECK`. DSQL: `CHECK` (`NOT VALID` and `ASYNC VALIDATE` are
added), `WHERE`, `NULLS NOT DISTINCT`, domains and sequences.

**SQL Server.** All index types (clustered, nonclustered, unique, filtered,
columnstore, XML, spatial, hash), `INCLUDE`, options (`FILLFACTOR`,
`PAD_INDEX`, `IGNORE_DUP_KEY`, `STATISTICS_NORECOMPUTE`, row and page locks,
`OPTIMIZE_FOR_SEQUENTIAL_KEY`, `DATA_COMPRESSION`, `COMPRESSION_DELAY`,
`BUCKET_COUNT`, spatial), full-text indexes (`KEY INDEX`, catalog, change
tracking, stoplist, search property list, language, type column and
statistical semantics per column), `CHECK`, `UNIQUE` constraint versus index,
sequences, synonyms, alias and table types, full-text catalogs and stoplists
(modified in place). Babelfish: types only. Fabric: none.

**MySQL and compatibles.**
- MySQL, Aurora, Cloud SQL: `CHECK` (including `NOT ENFORCED`), prefixes,
  `DESC`, functional indexes, `INVISIBLE`, `COMMENT`, `FULLTEXT WITH PARSER`,
  `SPATIAL`, `USING HASH/BTREE`, spatial SRID.
- MariaDB: table and column `CHECK` (the latter as part of the column's
  type), `DESC`, `IGNORED`, `COMMENT`, `FULLTEXT`, `SPATIAL`, `HASH` unique,
  sequences.
- TiDB: clustered primary key as a table option, prefixes, expression
  indexes, `INVISIBLE`, `COMMENT`, sequences, `CHECK` (needs
  `tidb_enable_check_constraint`).
- StarRocks, Doris, VeloDB: indexes from `SHOW INDEX` (`BITMAP`, `NGRAMBF`,
  `GIN`/inverted), `bloom_filter_columns` and rollups (`ROLLUP` type).
- GreptimeDB: `INVERTED`, `FULLTEXT` and `SKIPPING` indexes, and the `ttl`,
  `append_mode` and `compaction` options.
- Manticore: table settings (`morphology`, `min_infix_len`…).
- SingleStore: `SHARD KEY`, `SORT KEY` and table type, as table options.
- OceanBase and Databend: sequences. Databend also inverted, ngram and
  aggregating indexes.

**Oracle (and Autonomous).** `CHECK` (not the `NOT NULL` ones), normal,
bitmap, function and domain indexes (Oracle Text as `FULLTEXT`, Spatial as
`SPATIAL`, the rest as `DOMAIN` with `INDEXTYPE` and `PARAMETERS`), `REVERSE`,
`COMPRESS`, `INVISIBLE`, `LOCAL`/`GLOBAL` partitioning, sequences (the
initial value is `LAST_NUMBER`, so a used sequence differs), synonyms (public
ones under `PUBLIC`) and object, `TABLE OF` and `VARRAY` types.

**SAP HANA.** `CHECK`, `FULLTEXT` indexes with all their settings, `CPBTREE`
and `INVERTED`, sequences, synonyms (private and public) and table types.

**Snowflake.** Sequences and the clustering key (`CLUSTER BY` / `DROP
CLUSTERING KEY`).

**SQLite and libSQL.** Table and column `CHECK` (a change rebuilds the
table), `DESC` and collation in indexes, and virtual tables (FTS5, FTS4,
R\*Tree) as the `virtual_table` type, which is no longer synced as an
ordinary table.

**DuckDB.** `CHECK` (without names), expression indexes, sequences and types
(`ENUM`, `STRUCT`, alias, `LIST`, `MAP`, `UNION`).

**Firebird.** `CHECK` (unnamed `INTEG_n`), inactive indexes, domains (as a
type) and sequences (the current value does not make them differ).

**ClickHouse.** Skip indexes (all of them, including the text one), `CHECK`,
`ASSUME` (table option `assume:<name>`), projections (`PROJECTION` type) and
dictionaries (`dictionary` type).

**Cloud.**
- Spanner: `CHECK`, `STORING`, `DESC`, filtered indexes, `NULL_FILTERED`,
  interleaved, search and vector indexes, hidden columns and sequences.
- BigQuery: search and vector indexes.
- Databricks: Delta `CHECK` constraints.
- Trino, Dremio: nothing to compare.

**ODBC presets.**
- Db2 LUW: `CHECK`, `INCLUDE`, sequences, aliases, distinct and structured
  types.
- Db2 for z/OS: `CHECK`, sequences, aliases and distinct types.
- Db2 for i: `CHECK`, sequences and aliases.
- Informix, GBase: `CHECK`, sequences and synonyms.
- SQL Anywhere: `CHECK` and sequences.
- ASE, Teradata: `CHECK`.
- Vertica: `CHECK` and sequences.
- MonetDB: sequences.
- Netezza: sequences and synonyms.
- Dameng: `CHECK`, sequences, synonyms and object types.
- Altibase: `CHECK`, sequences and synonyms.
- CUBRID: serials and synonyms (from 11.2).
- Ingres: `CHECK`, sequences and synonyms.
- Mimer: `CHECK`, sequences, synonyms and domains.
- Exasol, Hive, Impala, Spark: nothing.

**Cassandra, ScyllaDB, Keyspaces.** Options of SAI, SASI and custom indexes,
types, materialized views and functions.

**MongoDB.** The `$jsonSchema` validator as a `CHECK` named `validator`, text
indexes as `FULLTEXT`, `2dsphere`, `2d`, `hashed` and wildcard, and the TTL,
`sparse`, `hidden`, collation and partial filter options. Views.

**Neo4j, Memgraph.** `UNIQUE`, `EXISTS`, `KEY` and `TYPE` constraints as index
types, and `FULLTEXT`, `VECTOR` and `POINT` indexes with their configuration.

**Elasticsearch, OpenSearch.** The analysis settings (`analysis`). To apply
them, the sync closes and reopens the index.

**Couchbase.** GSI indexes (they were not read before).

**CouchDB.** `validate_doc_update` as a `CHECK`.

**OrientDB.** `COLLATE` and Lucene engine and metadata. Sequences no longer
differ by their current value.

**Cosmos DB.** Spatial, full-text and vector indexes.

**DynamoDB.** Index projections (`INCLUDE`, `KEYS_ONLY`).

**TDengine.** Tag indexes.

### What is not compared

What was tested against a real server:

- **With a real server:** PostgreSQL 16, TimescaleDB, CockroachDB 26.3,
  openGauss 7.0, H2 2.1, Materialize 26.43, YugabyteDB, RisingWave and CrateDB
  (temporary container); SQL Server (container with full-text); Oracle
  (except the Oracle Text and Spatial indexes, which the light image does not
  include); Spanner (emulator); DSQL (with PostgreSQL as a stand-in); SQLite,
  libSQL, DuckDB, Firebird and ClickHouse; MySQL 8.4, MariaDB 11.8, TiDB 7.5,
  StarRocks 4.1, GreptimeDB 1.2.1 and Manticore; MongoDB, Cassandra 5,
  ScyllaDB, Neo4j 5, Memgraph, OpenSearch, Couchbase, CouchDB, OrientDB,
  DynamoDB Local, TDengine and the Cosmos DB emulator (which does not store
  its own index policies: Cosmos DB's spatial, full-text and vector indexes
  have unit tests only). Elasticsearch was tested through OpenSearch.
- **Unit tests only, implemented from the vendor's documentation and with no
  test server:** SAP HANA (the `CHECK_CONDITION` columns and those of the
  full-text views), Snowflake, BigQuery, Databricks, OceanBase, Databend, the
  Oracle Text and Spatial indexes, and all ODBC presets (Db2, Informix,
  GBase, SQL Anywhere, ASE, Teradata, Vertica, MonetDB, Netezza, Dameng,
  Altibase, CUBRID, Ingres and Mimer). In the ODBC ones the catalog SQL could
  not be verified against a server, and some columns remain unconfirmed:
  Netezza's views, Altibase's `V$SEQ`/`SYS_SYNONYMS_`, CUBRID's
  `db_serial`/`db_synonym` and Ingres's `iisynonyms`.

| Engine | What is missing | Reason |
|---|---|---|
| SQL Server | Primary key (clustered and options), column collation, table storage (compression, filegroups, partitions, memory-optimized, temporal), disabled indexes and `NOCHECK` constraints, full-text filegroup, search property lists as objects, statistics, CLR types, XML schema collections, partition functions | The comparison contract has no field to represent them. Explicit pending item: add them to the contract. |
| SQL Server | Recreating a memory-optimized table | It is not supported. Replacing a sequence used by a default, or a type used by a column, fails on the `DROP`. |
| Babelfish, Fabric | Almost everything | Babelfish compares types only; Fabric has nothing to compare. |
| PostgreSQL and compatibles | Index tablespaces, primary key storage parameters | Explicit pending item: they are not read. |
| PostgreSQL and compatibles | Replacing a type that a column uses | `DROP` + `CREATE` fails while the column uses it. |
| PostgreSQL and compatibles | `OWNED BY` of a new table's sequence | It is omitted from the script; the next comparison shows it. |
| openGauss | `DROP DOMAIN` | The engine does not allow it. |
| Greenplum, openGauss | `INCLUDE` | Greenplum: the version that supports it is not known. openGauss: it only has `ubtree` indexes. |
| H2 | `NULLS` order of indexes | Explicit pending item. |
| Redshift, Denodo | Everything in this list | There is nothing to compare. |
| Yellowbrick | Everything except sequences | Explicit pending item. |
| CrateDB | Adding a full-text index to an existing table | The engine does not allow it: a warning says the table has to be recreated. |
| MySQL, Aurora, Cloud SQL | `KEY_BLOCK_SIZE`, `ENGINE_ATTRIBUTE` | Explicit pending item. |
| MariaDB | Removing a column `CHECK` with `DROP CONSTRAINT` | It goes as part of the column's type. |
| TiDB | `DESC` in indexes, changing the clustered primary key | TiDB does not store the `DESC`. The clustered key cannot be changed with `ALTER`: a warning is given. |
| SingleStore | Changing `SHARD KEY`, `SORT KEY`, table type | They cannot be changed with `ALTER`: a warning is given. |
| GreptimeDB | Changing `analyzer` and `case_sensitive` of an index | The engine does not change them: a warning is given. |
| StarRocks, Doris, VeloDB | Materialized views of the rollup sync | Out of scope for this batch: explicit pending item. Index changes run as background jobs: a single `ALTER` that is retried every 2 s while the table is busy (10 minutes at most, it can be cancelled). |
| Oracle | Vector indexes (23ai), table partitioning, tablespace and index storage | Explicit pending item. |
| SAP HANA | — | No known gaps; pending testing against a server. |
| Snowflake | Indexes, search optimization, hybrid table indexes, `CHECK`, synonyms, types | Standard tables have no indexes, search optimization is a service, and Snowflake has no `CHECK`, synonyms or types. Hybrid table indexes: explicit pending item, they are not read. |
| SQLite, libSQL | Sequences, types | The engine does not have them. |
| DuckDB | `DESC` keys, synonyms, which user type a column uses | DuckDB discards the `DESC` and has no synonyms. Explicit pending item: which user type a column uses (DuckDB reports the expanded type). |
| Firebird | Which domain a column uses, array domains; replacing a used domain | Explicit pending item; the replacement fails and needs `ALTER DOMAIN`. |
| ClickHouse, Timeplus | Named collections (ClickHouse); constraints and projections (Timeplus) | Named collections belong to the server, not to a database. Timeplus has no constraints or projections. |
| Spanner | `DESC` columns of the primary key, change streams, property graphs, models, proto bundles | Explicit pending item. |
| BigQuery | `CHECK`, sequences, synonyms | The engine does not have them. |
| DSQL | Nothing known | — |
| Databricks | Bloom filter indexes | They are column metadata and Databricks considers them obsolete. |
| Trino, Dremio | Everything | There is nothing to compare. |
| Db2 for i, Informix, GBase | User-defined types | Db2 for i: they are not read. Informix and GBase: distinct and row types pending. |
| Db2 LUW | Array and row types | Explicit pending item. |
| SQL Anywhere, ASE, Teradata, Vertica | Domains (SQL Anywhere), `sp_addtype` types (ASE), UDTs (Teradata), projections (Vertica) | Explicit pending item: they are not read. |
| Netezza | `CHECK`, user types | The engine has no `CHECK` or UDTs. |
| CUBRID | `CHECK`, user types | The engine has no `CHECK` or UDTs. |
| Altibase, Ingres | User types | They have no SQL UDTs. |
| Dameng | Public synonyms | Explicit pending item. |
| Exasol, Hive, Impala, Spark | Everything | There is nothing to compare. |
| MongoDB | — | No known gaps in this batch. |
| Cassandra, ScyllaDB, Keyspaces | — | No known gaps in this batch. |
| Neo4j, Memgraph | Memgraph vector index capacity | The server reports a value that varies. |
| Elasticsearch, OpenSearch | Templates and ingest pipelines | Explicit pending item. |
| Couchbase | Search (FTS) indexes | They live in the Search service API, not in the GSI index one. |
| CouchDB | Views; changing or deleting a `validate_doc_update` | Views: explicit pending item. Changing or deleting needs `_rev`, so the sync only warns. |
| OrientDB | `min`, `max` and `regexp` of properties | Explicit pending item. |
| Cosmos DB | Applying changes to spatial, full-text and vector indexes | There is no statement to modify a container: the sync only warns. |
| Solr | Field types and `copyField` | Explicit pending item. |
| InfluxDB | Sync | It has no DDL: measurements and fields appear when writing. |
| IoTDB | Indexes and `CHECK` | The engine does not have them. |
| Redis | Everything | It has no schema. |

### Sync: relational engines

| Engine | How a column changes | Limits | Tested against a server |
|---|---|---|---|
| PostgreSQL and compatibles (CockroachDB, Timescale, Yugabyte, AlloyDB, Aurora, EDB, Kingbase…) | `ALTER COLUMN … TYPE … USING`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT`. Views that use the column are recreated around the change. | — | PostgreSQL 16 |
| Redshift | `ALTER COLUMN … TYPE`, only for a varchar's length | nullability | no |
| CrateDB, RisingWave, Materialize | adding and dropping columns only | types | no |
| Denodo | — | none: base views are defined in Denodo | — |
| H2 | `SET DATA TYPE` | — | no |
| DSQL | `ADD COLUMN`, `DROP COLUMN`, `DROP NOT NULL`, defaults, `ASYNC` indexes | types, NOT NULL, primary key, foreign keys | yes, with PostgreSQL as a stand-in |
| SQL Server, Azure SQL, Babelfish | `ALTER COLUMN … [NOT] NULL`. The default (named constraint) and the indexes on the column are removed and put back. | — | SQL Server 2022 (the full script, statement by statement) |
| Fabric | adding and dropping columns only | types | no |
| MySQL, MariaDB, TiDB, OceanBase, SingleStore, Aurora/Cloud SQL | `MODIFY COLUMN` with the whole column | — | no |
| StarRocks, Doris, VeloDB | `MODIFY COLUMN` | asynchronous changes (a warning is given) | no |
| Manticore | adding and dropping columns only | types | no |
| Oracle, Dameng | `MODIFY (…)` | — | Oracle |
| Firebird | `ALTER COLUMN … TYPE`, `SET/DROP NOT NULL`. Unnamed keys (`INTEG_n`) are looked up and dropped. | — | yes |
| SAP HANA | `ALTER (<column>)` | — | no |
| SQLite, libSQL | The table is rebuilt: new table, copy, drop and rename. Adding columns is done in place. | — | libSQL |
| DuckDB | `ALTER COLUMN … TYPE`, nullability and defaults in place | Keys and constraints: the table is rebuilt, and it fails if other tables reference it. | yes |
| Snowflake | `SET DATA TYPE` (widening only), nullability | Defaults of existing columns: no. Indexes: no (UNIQUE only). | no |
| BigQuery | `SET DATA TYPE` (widening only), `DROP NOT NULL`, defaults | New NOT NULL columns (they come in as NULLABLE), changing to NOT NULL, indexes | no (the emulator does not apply the ALTERs) |
| Databricks | `ALTER COLUMN … TYPE` (widening only) | New NOT NULL or identity columns. Dropping columns needs *column mapping* (a warning is given). | no |
| Athena | Iceberg: `ADD COLUMNS`, `DROP COLUMN`, `CHANGE COLUMN`. External: `ADD/CHANGE COLUMN`, drop only in CSV. | NOT NULL, defaults, keys, indexes, partition columns | no |
| Spanner | `ALTER COLUMN <whole column>` | Primary key and its columns. Types: only STRING↔BYTES and lengths. | emulator |
| Trino, Starburst | `SET DATA TYPE`, `DROP NOT NULL`, `ADD/DROP COLUMN` (depends on the connector) | changing to NOT NULL, defaults | Trino |
| Presto | adding and dropping columns only | types, nullability, defaults | yes |
| ClickHouse | `MODIFY COLUMN` (nullability = `Nullable(T)`), skip indexes with `MATERIALIZE` | engine, ORDER BY, PARTITION BY, key | yes |
| Timeplus | add columns, indexes, comment and TTL | dropping or changing columns | yes |
| TDengine | `ADD/DROP/MODIFY COLUMN` and tags (MODIFY only to widen) | first time column, subtables | yes |
| Dremio (Iceberg) | `ADD COLUMNS`, `DROP COLUMN`, `ALTER COLUMN` (widening only) | partitions, non-Iceberg sources | yes |
| Phoenix | adding and dropping columns only | types, key (row key) | yes |
| ODBC presets | Depends on the engine: Db2 (`SET DATA TYPE`, with `REORG`), Sybase and Zen (`MODIFY`), Informix (`MODIFY (…)`), Teradata, Exasol, Vertica, MonetDB, Hive/Impala/Spark, etc. Those that do not modify columns only add and drop. | Db2 for z/OS does not change nullability. Hive and Spark have their own limits. | no |
| NetSuite | — | none: SuiteAnalytics Connect is read-only | — |

### Sync: other engines

| Engine | What it syncs | Limits | Tested against a server |
|---|---|---|---|
| Cassandra, ScyllaDB, Keyspaces | tables, columns (add and drop), indexes, options | column type, primary key (the table has to be recreated) | Cassandra and ScyllaDB |
| MongoDB | collections, views, indexes, validator, TTL | capped, time series and clustered are set at creation | yes |
| Couchbase | collections, GSI indexes | maxTTL | yes |
| Cosmos DB | create and drop containers | Partition key, unique keys, index policy, TTL and RU/s: they are changed from the portal or the Azure CLI. | no |
| CouchDB | new Mango indexes and design docs | deleting or changing an index or design doc | yes |
| DynamoDB | tables, global indexes | Key schema: the table has to be recreated. Local indexes: no. | DynamoDB Local |
| OrientDB | classes, properties, indexes | EXTENDS | yes |
| Solr | collections, fields (with a reindex warning) | uniqueKey, configSet, shards | standalone and SolrCloud |
| ksqlDB | streams, tables, add columns | dropping or changing columns, key | yes |
| Elasticsearch, OpenSearch | new fields, replicas, aliases | Field type, dropped fields and shards: reindexing is needed. | yes |
| Neo4j, Memgraph | indexes and constraints | properties are not declared | yes |
| IoTDB | new and dropped series | type, encoding, compression | yes |

No sync (the comparison works all the same):

- **Redis, etcd:** they have no schema, only keys with values. To move data,
  use copy or export.
- **Flight SQL:** it is a protocol, not an engine; connect with the driver of
  the database behind it.
- **Drill:** it queries files; its tables are created with `CREATE TABLE AS`
  and have no columns to modify.
- **InfluxDB:** measurements and their fields appear when writing and are not
  modified with DDL.
- **Neptune:** it has no user-defined schema.
- **Denodo and NetSuite:** the reasons are in the relational engines table.

### Deleting in the comparison

"Delete" drops an index, column, foreign key, `CHECK`, primary key, table or
object (view, procedure, function, trigger, sequence…) on one side without
having to pass the change from the other. Like the arrows, it only modifies
the in-memory copy; the `DROP` comes out in the "Sync" script. Before running,
the dialog looks up what depends on each object being dropped, on the side
where it is dropped and only in engines with `supports_dependencies`. The
lookup is asynchronous and does not block the button. If it fails, the dialog
says it couldn't check the dependencies of the items being deleted and lets you run anyway.
Confirmed and probable dependents count as breakages; those from dynamic SQL
are shown separately, and if something could not be read, the list is marked
as incomplete.

What was tested: the test builds the changes the way the screen builds them,
runs the script, reads both sides again and verifies that the difference is
gone.

| Engine | Tested against a server | What cannot be dropped | Reason |
|---|---|---|---|
| PostgreSQL 16 | yes | nothing of what was tested | index, column, FK, `CHECK`, PK, referenced table, views, triggers, overloaded functions, procedures |
| CockroachDB 26.3 | yes | primary key | it requires every table to have one: it rejects `DROP CONSTRAINT` of the PK without adding another in the same transaction. The script keeps it and warns "CockroachDB doesn't allow a table without a primary key: {0} keeps its own.". |
| SQL Server | yes (2022) | view or function `WITH SCHEMABINDING` that uses the column or table; column used by a computed column | SQL Server rejects the `DROP`. The dependency check should show the first; the second is not covered by the generator. Babelfish and Fabric were not tested. |
| MySQL 8, MariaDB 11 | yes | index that an FK needs; column of a multi-column `CHECK` | MySQL gives error 3959 and MariaDB 1054; the server rejects the `DROP`. The screen removes only the column, so the script fails. A single-column `CHECK` is removed by the server along with it. Column used by an FK: untested. |
| Oracle (Free 23) | yes | packages; Oracle Text and Spatial domain indexes (untested) | packages are not loaded in the comparison and have no `DROP`. The `slim` image has neither Text nor Spatial. Unnamed `CHECK`s and FKs are dropped by looking up the system name (`SYS_C…`) with a PL/SQL block. |
| SQLite | yes | routines (they do not exist) | dropping a column, `CHECK` or FK rebuilds the table (with a warning). A trigger on a rebuilt table is lost: explicit pending item, it has to be recreated in the comparison layer or warned about. libSQL uses the same rebuild and was not checked. |
| MongoDB | yes | fields; foreign keys, triggers and routines do not exist; the `_id` PK is not changed | documents have no fixed schema, dropping a field only gives a warning. A view appears twice in the model (as a `view`-type table and as an object); dropping either removes both. |
| Elasticsearch | yes (8.15.3); OpenSearch untested | mapping fields; custom analysis | it requires reindexing, left as a warning. It has no internal indexes, PK, FK, `CHECK`, views, routines or triggers. The whole index and the description can be dropped. |
| Rest of the engines | no | unverified | the `DROP` comes from the common generator or each engine's `drop_other`. There is no test against a server. |

Explicit pending items:

- **`CHECK`s and indexes of a dropped column** in the common generator
  (`alter.rs`, shared contract): only SQL Server removes them along with the
  column. MySQL and Oracle probably fail the same way with a `CHECK`; untested.
- **Views that depend on a table that loses a column:** they are dropped and
  recreated, and if the view uses that column the recreation fails.
- **CockroachDB primary key:** changing it generates `DROP CONSTRAINT` +
  `ADD PRIMARY KEY` in two statements, which probably fails all the same. Its
  form is `ALTER PRIMARY KEY USING COLUMNS`; untested.
- **Explorer and generated scripts:** the explorer's "Delete" and the `DROP`
  section of the generated scripts still write the old `DROP` of triggers and
  overloaded functions in PostgreSQL.
- **Elasticsearch:** removing the description replaces the whole `_meta`.

### Sync: table and column comments

A comment that is added, changed or removed in the source is carried to the
target, including that of a column that the same sync adds. Each engine
writes it with its own syntax (the contract: `alter::sync_script_with_comments`;
without it, `COMMENT ON`).

| Engine | How it is written | Tested against a server |
|---|---|---|
| SQL Server, Azure SQL, Babelfish | `MS_Description` extended property: `sp_addextendedproperty` or `sp_updateextendedproperty` if it already exists; `sp_dropextendedproperty` when removing it | SQL Server 2022; Babelfish (the three statements) |
| PostgreSQL and compatibles, Oracle, Firebird, DuckDB, Snowflake, SAP HANA, Db2, Exasol and the others with `COMMENT ON` | `COMMENT ON TABLE` / `COMMENT ON COLUMN` | PostgreSQL 16 |
| MySQL, MariaDB, TiDB, OceanBase, SingleStore, Aurora/Cloud SQL, StarRocks, Databend | Column: `MODIFY COLUMN … COMMENT`. Table: `ALTER TABLE … COMMENT =` | MySQL 8, MariaDB 11, StarRocks |
| Doris, VeloDB | Table: `ALTER TABLE … MODIFY COMMENT` | no |
| GreptimeDB | `COMMENT ON TABLE` / `COMMENT ON COLUMN` (its `MODIFY COLUMN` only changes the type) | yes |
| CUBRID (ODBC) | Column: `MODIFY`. Table: `ALTER TABLE … COMMENT =` | no |
| Hive, Impala, Spark (ODBC) | Column: `CHANGE COLUMN` (Spark: `ALTER COLUMN … COMMENT`). Table: `SET TBLPROPERTIES ('comment' = …)` | no |
| Vertica (ODBC) | Table: `COMMENT ON TABLE` | no |
| Cassandra, ScyllaDB | The table's `comment` option; when removing it, `comment = ''` | Cassandra 5 |
| ClickHouse, Trino, Athena, BigQuery, Databricks, TDengine, Elasticsearch | Each engine's own (see its row in the tables above) | depends on the engine |

No comment sync:

- **Fabric Warehouse:** it has no extended properties.
- **Manticore, Spanner, Phoenix, SQLite, libSQL:** the engine has no comments
  that can be written with SQL. Phoenix shows the catalog's `REMARKS`, but has
  no statement to change them.
- **Vertica (ODBC), columns:** `COMMENT ON COLUMN` comments projection
  columns, not table columns.
- **ODBC presets without `COMMENT ON` or inline comments** (Sybase ASE,
  Informix, Ocient, Virtuoso, IRIS, Zen, OpenEdge, Machbase, SQream, Access,
  dBase, NuoDB, HeavyDB, Ignite, generic ODBC): they are not written.
  Explicit pending item for those that have their own syntax: it has to be
  confirmed against each server, and there are no containers to do it.

## Key search in the explorer

In key-value engines, a database can have millions of keys. DBine does not
list them all: the **Keys** node searches on the server, one page at a time.
The contract is in `crates/dbine-driver/src/keys.rs` (`Driver::key_search` and
`Session::scan_keys`).

- **Search box** in the first row of **Keys**. Enter searches on the server
  and Esc shows everything again. It saves each connection's latest searches.
- **Pages** of 500 keys with **Load more**. The node shows how many there are
  in total. A very selective search keeps requesting pages by itself for a few
  seconds, so as not to start with an empty list.
- **Namespace folders** according to the engine's separator (`user:1:cart`
  ends up in `user › user:1`). A prefix with a single key shows the key
  without a folder. Right click on a folder › **Search on the server** shows
  all the keys of that prefix, not only the loaded ones.
- **Type and TTL** of each key, when the engine has them.

| Engine | Search | Separator | Type filter | TTL |
|---|---|---|---|---|
| Redis, Valkey, Dragonfly | `SCAN … MATCH` with wildcards (`*`, `?`, `[…]`); text without wildcards = the keys that contain it, with the exact key first. Case-sensitive. | `:` | On the server with `SCAN … TYPE` (Redis 6 or later); in earlier versions DBine filters | `PTTL` |
| etcd | Prefix, inside the connection's prefix; pages follow the key order | `/` | It has no types | The key's lease |

The total comes from `DBSIZE` in Redis and from the range's `count` in etcd.
Each Redis page checks at most 50,000 keys and returns what it found, so a
search over millions of keys does not block the session.

### Engines without key search

- **DynamoDB:** the explorer lists tables, not items. Items are seen with
  "View data" and filtered with the per-column filters.
- **All other engines:** their objects are tables, collections or indexes,
  which are listed in full and filtered with the explorer's "Filter" box.

### Tested against real servers

`key_search` in `crates/drivers/redis/tests/integration.rs` (Redis 7, Valkey,
Dragonfly) and in `crates/drivers/etcd/tests/integration.rs` (etcd 3.5):
pagination with total and cursor, wildcards, text without wildcards, type
filter and TTL.

## Bulk load

Engines without their own bulk load (`bulk_load`), or with a partial one.
The migration uses generic batched insertion there.

| Engine | What is missing | Reason |
|---|---|---|
| Amazon Neptune | Bulk load | openCypher over HTTPS treats each request as its own transaction and there are no multi-request transactions: a cancelled load with a request in flight would commit it anyway, after finishing. Generic insertion has the same limit. Neptune's bulk loader reads from S3 and needs an IAM role, outside what a connection sees. |
| Neo4j, Memgraph | Loading relationships | The load creates nodes (`UNWIND … CREATE`); a relationship needs its source and target nodes. |
| Neo4j, Memgraph | Leap seconds (`:60`) | Cypher's temporal values do not have them: the load rejects them. |
| Memgraph | Dates outside years 0 to 9999, byte arrays | Memgraph does not store them (the load rejects them); bytes go as `0x…` text. |
| Flight SQL: GizmoSQL (DuckDB) | Loading only some columns of a table when they include `STRUCT`, `LIST`, `MAP` or `UNION` | That load uses a prepared `INSERT`, and GizmoSQL does not accept nested values as parameters (the load rejects it). Loading all the columns uses bulk ingest (`CommandStatementIngest`), which does accept them. |
| Flight SQL: GizmoSQL (DuckDB) | Loading only some columns when a `HUGEINT` has more than 38 digits | GizmoSQL passes each parameter of the prepared `INSERT` through its text, and there a 39-digit `HUGEINT` fails (the load rejects it). Loading all the columns has no limit. |
| Flight SQL: GizmoSQL (DuckDB) | `UHUGEINT`, `BIT`, `BIGNUM` or `TIMETZ` inside a nested type (such as `UHUGEINT[]` or `STRUCT(a BIT)`), on read and on load | Over Arrow they arrive as the raw bits of a `DECIMAL(38,0)`, DuckDB's internal bytes or a time without its zone. On their own they are read and loaded as text, converted on the server; inside a nested type there is no way to do it, and they are rejected. |
| Flight SQL without transactions | Rolling back a commit window that fails | Each batch (or each round of the prepared `INSERT`) is committed separately: what was already committed stays, and the error says so. |

## Bulk transfer (migrating data)

How the migration moves data (`docs/bulk-transfer.md`) in each engine. All
engines read and load through the transfer engine; what changes is the path:

- **Bulk load:** the engine's own load path (`bulk_load`). Without it, the
  migration writes batched `INSERT`s (`insert_script`).
- **Direct copy:** between two databases of the same engine, rows do not go
  through the app (`copy_native`).
- **Typed read:** cells come out with their exact type (decimals with all
  their digits, complete binaries, dates with their precision), not as the
  grid's text. Unless stated otherwise, all engines in the list have it.
- **Faithful clone** and **syncing only what changed** only exist between
  databases of the same engine.

Tested against real servers (`dbine-test-*` containers): all those listed
without a mark. What says "not tested live" has no container or emulator and
follows the vendor's documentation; it is below, in the limitations.

| Engine | Bulk load | Direct copy | Faithful clone | Sync |
|---|---|---|---|---|
| PostgreSQL, TimescaleDB, KingbaseES, AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu | binary `COPY … FROM STDIN` (text if a column has no encoder: arrays, `interval`, `money`, enums) | yes (a `COPY TO` → `COPY FROM`, only with the same native types) | yes | yes |
| YugabyteDB | binary `COPY` | yes | no | yes |
| openGauss | binary `COPY` | yes | no | no |
| Greenplum, Cloudberry, Greengage | text `COPY FROM` | no | no | no |
| CockroachDB, Redshift, CrateDB, H2, Denodo, RisingWave, Yellowbrick, Materialize | none (batched `INSERT`) | no | no | no |
| SQL Server, Azure SQL | `INSERT BULK` with `TABLOCK` and 32,767-byte TDS packets | yes (rows as TDS bytes, not decoded) | yes | yes |
| Fabric, Babelfish | none (batched `INSERT`) | no | no | no |
| MySQL, MariaDB, Aurora MySQL, Cloud SQL MySQL | `LOAD DATA LOCAL INFILE` from memory (multi-row prepared `INSERT` if the server has `local_infile` off) | no | no | no |
| TiDB | multi-row prepared `INSERT` | no | no | no |
| OceanBase, SingleStore | `LOAD DATA LOCAL` (not tested live) | no | no | no |
| StarRocks, Doris, VeloDB, Databend, GreptimeDB | multi-row `INSERT … VALUES` | no | no | no |
| Manticore | none (`insert_script`) | no | no | no |
| Oracle, Oracle Autonomous | array DML (`INSERT` with batch binds) | yes (with time zone regions and extended JSON) | no | no |
| SQLite | prepared `INSERT` in one transaction per window | yes (read-only `ATTACH` + `INSERT … SELECT` by `rowid` ranges) | no | no |
| DuckDB | Appender | yes, only within the same instance | no | no |
| libSQL / Turso | multi-row prepared `INSERT` over HTTP | no | no | no |
| ClickHouse, Timeplus | `INSERT … FORMAT RowBinary` | yes (RowBinary from one to the other) | no | no |
| Firebird | `EXECUTE BLOCK` with many `INSERT`s | no | no | no |
| SAP HANA | batched prepared `INSERT` (not tested live) | no | no | no |
| Db2, Sybase, SQL Anywhere, Informix, Teradata, Vertica, Access, dBase and generic ODBC | prepared `INSERT` with parameter arrays | no | no | no |
| Hive, Impala, Spark, Kyuubi, Cloudera (ODBC) | none (multi-row `INSERT`) | no | no | no |
| NetSuite | read-only | — | — | — |
| Flight SQL (GizmoSQL…) | `CommandStatementIngest` (or prepared `INSERT`) | no | no | no |
| Phoenix, Avatica | batched `UPSERT` / prepared `INSERT` | no | no | no |
| Aurora DSQL | multi-row `INSERT`, windows of up to 3,000 rows over several connections (not tested live against real DSQL) | no | no | no |
| Snowflake | `INSERT … SELECT` from a work table (not tested live) | no | no | no |
| BigQuery | load jobs from line-delimited JSON | no | no | no |
| Databricks | `INSERT … VALUES` per window (not tested live) | no | no | no |
| Athena | `INSERT … VALUES`, Iceberg tables only (not tested live) | no | no | no |
| Trino, Presto, Starburst | `INSERT … VALUES` (one transaction per statement) | no | no | no |
| Dremio | `INSERT … SELECT CAST … FROM (VALUES …)`, only tables with DML (Iceberg) | no | no | no |
| Drill | none: read-only | — | — | — |
| Cloud Spanner | `insert` mutations | no | no | no |
| Cassandra, ScyllaDB | prepared `INSERT`, 256 in flight | no | no | no |
| Amazon Keyspaces | same as Cassandra (not tested live) | no | no | no |
| MongoDB, FerretDB, DocumentDB | unordered `insertMany` | yes, among the three (raw BSON) | no | no |
| Cosmos DB (NoSQL) | transactional batches by partition key | yes, between Cosmos DB | no | no |
| Couchbase | SQL++ `INSERT` with parameters | no | no | no |
| CouchDB | `_bulk_docs` | no | no | no |
| DynamoDB | `TransactWriteItems` with conditional puts | no | no | no |
| Elasticsearch, OpenSearch, Open Distro | `_bulk` NDJSON (Open Distro not tested live) | no | no | no |
| Solr | `/update` with JSON | no | no | no |
| Redis, Valkey, Dragonfly | `HSET` in `MULTI`/`EXEC` | no | no | no |
| etcd | put transactions | no | no | no |
| Neo4j, Memgraph | `UNWIND $rows CREATE (n:Label) SET n += r` | no | no | no |
| Amazon Neptune | none (see "Bulk load") | no | no | no |
| OrientDB | `BEGIN; …; COMMIT;` scripts through `/batch` | no | no | no |
| InfluxDB 1, 2 and 3 | line protocol | no | no | no |
| TDengine | multi-row `INSERT` | no | no | no |
| IoTDB, TimechoDB | `insertTablet` in columns | no | no | no |
| ksqlDB | `/inserts-stream` row by row | no | no | no |

### Speed

Orders of magnitude measured in local containers (a single machine, source
and target on the same network): they are for comparing paths, they promise
nothing in another environment or against a remote server.

- **Millions of rows per second:** SQLite and DuckDB (read and load), Flight
  SQL (against DuckDB) and the direct copy of DuckDB, ClickHouse and
  PostgreSQL.
- **Hundreds of thousands:** PostgreSQL, SQL Server, MySQL and MariaDB,
  ClickHouse, MongoDB, Redis and compatibles, Oracle, TiDB (read) and
  InfluxDB.
- **Tens of thousands:** Cassandra, ScyllaDB, TiDB (load), Firebird, Neo4j,
  Memgraph, Elasticsearch, OpenSearch, CouchDB, Couchbase, etcd, TDengine,
  IoTDB, libSQL and Dremio's read.
- **Thousands:** DynamoDB, Phoenix, ksqlDB, StarRocks, Dremio (load, limited
  by its planner) and Cosmos DB against its emulator (1,000 to 6,000).
- On SQL Server the 700,000 rows/s mark could not be measured: the available
  container is emulated `amd64` on an `arm64` Mac and the limit is the
  server. It gave 200,000 to 270,000 rows/s with direct copy.

### Limitations and rejections

A rejection is a failure with a cause, before or during the load: the
migration does not change the data silently nor leave it half done without
saying so.

**Relational**

- **PostgreSQL and derivatives.** Direct copy only works with the same native
  types on both sides and without columns tied to their server (`money`,
  `oid`, `reg*`); otherwise it is read and loaded. CockroachDB does not load
  through `COPY` (its extended protocol does not support it), and Redshift,
  CrateDB, H2, Denodo, RisingWave and Yellowbrick have no binary `COPY FROM
  STDIN` (Redshift only from S3): they write `INSERT`s. Materialize could load
  through text `COPY`; it stays pending because there is no container to
  verify it. Redshift, Denodo, H2, CrateDB, the streaming engines and
  Yellowbrick read through the simple protocol: everything arrives as the
  server's text. A target failure (a constraint, a full disk) only shows up
  when its window ends; with commit set to 0 the whole table is one window.
- **PostgreSQL with "Simple protocol only".** A connection that uses only the
  simple protocol (a gateway that rejects the extended one; see
  [script execution](script-execution.md#query-protocol-postgresql)) does not
  copy, does not compare data and does not clone: those operations need the
  extended protocol and are rejected with the reason.
- **Cloning and sync in PostgreSQL.** YugabyteDB does not clone (tablets and
  hash partitioning are not in PostgreSQL's catalog); openGauss has a
  PostgreSQL 9.2 catalog with its own storage options and does not sync
  either (it has no `hashtextextended`); Greenplum and derivatives have
  distribution policies, do not load binary `COPY` into temporary tables and
  their version 6 has no `hashtextextended`; CockroachDB has no `COPY FROM`
  over the extended protocol. Sync requires PostgreSQL 11 or later. With
  PostgreSQL 15 and 16 it uses `UPDATE` plus `INSERT` (its `MERGE` does not
  return the action); from 17, a single `MERGE`. Triggers and foreign keys
  are turned off while applying with `session_replication_role = replica`; if
  the role cannot, they fire and the log says so.
- **SQL Server.** `sql_variant` is rejected (it cannot be declared in
  `INSERT BULK` or passed as bytes). Fabric and Babelfish have no bulk load
  or direct copy (`INSERT BULK` could not be verified) and do not clone:
  Fabric has no *filegroups*, partitions, triggers or temporary or in-memory
  tables, and Babelfish's `sys` catalog is a partial emulation.
  `DBINE_SQLSERVER_NO_RAW=1` forces the decoded path instead of the
  raw-bytes copy.
- **Sync in SQL Server.** The key columns have to be `NOT NULL` on both
  sides (a null key falls in no group and does not match in the `MERGE`): it
  is rejected with the reason. An identity column that is not the first in
  the key, or with a negative increment, cannot follow the source and is
  rejected. If the key *collations* differ, all rows from both sides are
  applied; if the target considers two source keys equal, it is rejected.
- **MySQL and family.** `LOAD DATA LOCAL` turns invalid values and duplicate
  keys into warnings: a window with warnings, or with fewer rows than were
  sent, is rolled back and fails. TiDB does not use `LOAD DATA` because it
  commits by itself, even with `autocommit` off, and a failed window could
  not be rolled back. Bytes into a text column have to be valid UTF-8. Unique
  and foreign keys stay active. Table locking only exists in MySQL and
  MariaDB. StarRocks, Doris and VeloDB could load faster with Stream Load,
  but that is HTTP on other ports and the driver has no HTTP client;
  Manticore has no `LOAD DATA` or NULL. OceanBase, SingleStore, Doris,
  VeloDB and Databend, not tested live (no container); GreptimeDB, round trip
  only. Geometry is MySQL's internal value (SRID + WKB).
- **Oracle.** An empty `VARCHAR2`, `CHAR` or `RAW` is rejected, because Oracle
  stores it as NULL. A `GENERATED ALWAYS` identity with "preserve identity" is
  rejected and the message states the `ALTER … MODIFY … GENERATED BY DEFAULT`
  that is needed: DBine never modifies the table. With table locking
  (`APPEND_VALUES`) tables with LOB or JSON load through the normal path
  (ORA-65501). Values over 32 KB go one row at a time. Oracle Autonomous, not
  tested live.
- **SQLite and libSQL.** SQLite rejects `NaN` (it would store it as NULL).
  Direct copy does not accept an in-memory database as the source. When
  copying in batches within the same file, the open read prevents the default
  journal mode from committing: the load fails saying WAL mode avoids it. A
  table without `rowid` (view, `WITHOUT ROWID`) is copied with a single
  statement, and if it takes longer than the file can stay locked, it is
  interrupted and falls back to batches. libSQL has no direct copy (they are
  two servers that cannot see each other) and `sqld` cuts a value or row at
  5,000,000 bytes.
- **DuckDB.** Direct copy only works within the same instance (same file or
  `:memory:`): two different files are two instances and attaching one that is
  open by another is not safe. `TIMESTAMPTZ`, `INTERVAL`, enums and `JSON`
  into `MAP` go through a temporary table and a conversion in SQL, because of
  the Appender's limits.
- **ClickHouse.** `Dynamic` and `Variant` are not read (their text loses the
  subtype and the NULL); `JSON` and `AggregateFunction` are read as text.
  Direct copy is not valid with `AggregateFunction`. A `DateTime` is emitted
  in UTC: the zone name does not travel. When loading into `Date` the time is
  lost, in `DateTime` the fractional seconds, and in `DateTime64(n)` the
  digits beyond `n`: rejecting them when the lost part is not zero is still
  pending.
- **Firebird.** There is no Firebird 4 batch API in the pure Rust client; the
  `INSERT`s go in `EXECUTE BLOCK` of up to 800 rows, and tables with BLOB one
  row at a time. A `TIMESTAMP WITH TIME ZONE` with a region is read as the
  server's text. No direct copy.
- **SAP HANA.** Not tested live. A value that the parameter does not store as
  is gets rejected: decimals beyond precision or scale, out-of-range `REAL`,
  `NaN` and infinity, fractions of a second that do not fit, nonexistent
  dates and times, text that is not UTF-8 and GeoJSON. A session with
  autocommit off is rejected (its open work would be committed or lost). A
  `GENERATED ALWAYS` identity with "preserve identity" stops the load.
- **ODBC (Db2, Sybase, SQL Anywhere, Informix, Teradata, Vertica, Access,
  dBase…).** Reading fails instead of truncating; table locking is ignored;
  "preserve identity" turns on `IDENTITY_INSERT` only in Sybase and in SQL
  Server over ODBC (Db2 `GENERATED ALWAYS` rejects the value). Only the
  generic preset against SQL Server (ODBC 18) was tested live; the row-by-row
  path of drivers without parameter arrays was not tested. Hive, Impala,
  Spark, Kyuubi and Cloudera read row by row (their drivers report the length
  of `STRING` wrongly) and have no bulk load: each `INSERT` is a job that
  writes a file, and the real path (files to HDFS or S3 and `LOAD DATA`) is
  not reachable over ODBC. NetSuite is read-only.
- **Flight SQL.** A DuckDB `INTERVAL` goes as text, inside a nested type it is
  rejected, and the maximum cannot round-trip. With no transactions on the
  server, each batch commits by itself (see "Bulk load"). GizmoSQL without
  ingest loads row by row (~1,500 rows/s).
- **Phoenix and Avatica.** Times and timestamps cross the protocol in
  milliseconds: a finer fraction makes the load fail. `FLOAT` travels as
  `DOUBLE`, and arrays cannot be parameters in JSON serialization. The load
  never overwrites an existing row: a repeated key fails when counting at the
  end. The size bound of each read is measured just before; a concurrent
  writer can enlarge a frame. Avatica's JSON serialization has unit tests
  only.
- **Aurora DSQL.** Not tested live against real DSQL (it was tested with
  PostgreSQL as a stand-in). DSQL cuts every transaction at 5 minutes and
  limits each to 3,000 rows and 10 MiB, indexes included: that is why it does
  not use `COPY` and each window is its own `INSERT`. Reading by key pages
  uses separate snapshots: rows that change during the copy can look old or
  new. Without a primary key, the table is read in a single statement.

**Cloud and analytical engines**

- **Snowflake.** Not tested live. The SQL API rejects `PUT` and `GET`, so
  there is no staged load with files: it inserts into a transient work table
  next to the target and at the end a single atomic `INSERT … SELECT` runs. A
  `VARIANT` value with `undefined`, `NaN` or `Infinity` is rejected.
- **BigQuery.** The load uses load jobs (free, atomic); the quota is 1,500
  per table per day, that is 150 million rows per table and day with windows
  of 100,000. Reading a base table costs nothing; a view, an external table
  or a read with a filter runs a query that is billed. Bytes that are not
  UTF-8 into `STRING` and text that is not JSON into `JSON`/`STRUCT`/`ARRAY`
  are rejected. There is no table locking or identity.
- **Databricks.** Not tested live. Load through `INSERT` into Delta tables; if
  it fails after a commit, the table goes back to the previous version with
  `RESTORE TABLE`, only when the later versions are exactly those of this
  load. Loading through `COPY INTO` from a volume would need a volume that the
  connection does not configure.
- **Athena.** Not tested live. Only Iceberg tables load (an `INSERT` is an
  atomic commit); a Hive table can leave files written in S3 with no way to
  undo them, and views, Delta Lake and Hudi are read-only: all are rejected
  up front with the reason. Nested columns with `VARBINARY` are rejected in
  both directions.
- **Trino, Presto and Starburst.** There is no bulk load API. A decimal with
  more decimals than the column, a time with more fraction than the
  precision, or a timestamp with time into `DATE` are rejected instead of
  rounded. The Iceberg and Delta Lake connectors receive one statement at a
  time (concurrent commits collide).
- **Dremio.** It only loads into tables that accept DML (Iceberg). `STRUCT`,
  `LIST` and `MAP` columns are not loaded. One statement at a time; the load
  is limited by the planner (~2,000 rows/s).
- **Cloud Spanner.** Writes never overwrite rows (`insert`); a commit that
  Spanner rejects for size is split in half. Tested against the emulator.
- **Drill.** Read-only: it has no `INSERT`, only `CREATE TABLE AS SELECT` from
  what Drill itself reads.

**Documents, key-value, search, graphs and time series**

- **Cassandra, ScyllaDB, Keyspaces.** Tables with counters do not load
  (counters do not accept `INSERT`). A value finer than a millisecond in a
  `timestamp` is rejected, and schema conversion still reports it as a loss
  of precision instead of an error: aligning them is pending. A `datetime`
  into a `time` column drops the date silently: rejecting it is pending.
  Without transactions: the commit only sets the pace of progress, and a
  cancelled load can let what it had in flight land. Amazon Keyspaces, not
  tested live.
- **MongoDB, FerretDB, DocumentDB.** Dates have millisecond precision (a finer
  fraction is cut); `Decimal128` stores 34 digits (a longer one fails). A NULL
  field is omitted. Row by row between collections, a column that mixes
  `objectId` and `string`, or `int` and `long`, is rejected because values
  are not told apart one by one; direct copy (raw BSON) has no such limit. A
  document with a repeated field or with `$` keys that Extended JSON would
  read as another type fails on read, with its `_id`. DocumentDB, not tested
  live.
- **Cosmos DB.** Tested only against the emulator (about 1,000 to 2,000
  rows/s of load and 6,000 of read). Items without `id` do not load, an
  existing `id` fails the load, an item over 2 MB is rejected, and a failed
  batch leaves nothing. A lost response (408, 5xx) may have been applied: the
  error says so. An explicit `null` and a missing field are both NULL when
  moving to another engine; between Cosmos DB and Cosmos DB direct copy moves
  the items and tells them apart. Row-level sync from Cosmos to Cosmos does
  not exist.
- **DynamoDB.** An item over 400 KB, a number with more than 38 digits or
  outside 1E-130..9.99E+125 is rejected before sending. The load never
  replaces: an existing or repeated key cancels the transaction and the load
  fails. It does not load into indexes. With no columns requested, a first
  pass reads everything to learn the attributes (it reads twice).
- **Couchbase.** The load goes through SQL++ (the driver only speaks REST, no
  KV); an existing key fails the load. A document that is not a JSON object
  fails the read. A whole float (`1.0`) comes back as an integer.
- **CouchDB.** Design documents are skipped; `_id` is always text; a conflict
  (existing `_id`) fails the load.
- **Elasticsearch, OpenSearch, Open Distro.** A nested field that is moved to
  a dotted key (`metrics.mem`) loses nothing but changes shape. Fields that
  the mapping does not list and dotted keys travel together in the `_source`
  column; with no columns requested, the rest come from the mapping. Open
  Distro, not tested live (no container).
- **Solr.** Insert only (`_version_: -1`). It rejects fields the schema does
  not define, values that do not fit exactly (Solr truncates fractions and
  overflows integers without warning), empty or null-containing lists, and a
  `copyField` target whose value is not the one Solr would copy.
- **Redis, Valkey, Dragonfly.** No transactions that roll back: if a command
  in an `EXEC` fails (`WRONGTYPE`), the rest of the window stays written, is
  counted and the load fails naming the rows. The filter is not supported
  (key or pattern only). Module types are read with the default read.
- **etcd.** The filter is not supported. The load never overwrites an existing
  key. A null key or value is rejected (etcd has neither).
- **Neo4j, Memgraph.** See "Bulk load" for relationships, leap seconds, dates
  and Memgraph bytes. Decimals and UUIDs go as text and maps or mixed lists
  as JSON text; only labels are loaded.
- **Amazon Neptune.** Not tested live. Read-only (HTTP JSON): see "Bulk
  load".
- **OrientDB.** Each class is read and written separately (not its
  subclasses).
- **InfluxDB.** No compression in loads. A row becomes a point: it requires a
  `time`/`_time` column. They fail with `Unsupported`, before losing anything:
  a point without fields, `NaN` or infinity, an unsigned integer greater than
  `i64` in 1.x, tag names or values that line protocol cannot carry, and in
  2.x the names Flux reserves.
- **TDengine.** It does not use line protocol (it decides the types itself). A
  timestamp finer than the database's precision, `NaN` or an infinity are
  rejected. A statement that spans several subtables is not atomic.
- **IoTDB and TimechoDB.** Tree model only. The REST API returns a `BLOB` as
  UTF-8 text, so a binary that is not UTF-8 is altered on **read** (writing it
  goes through SQL with `X'…'`). A timestamp finer than the server's precision
  is rejected (it is the key: two rows would collapse). Before 1.3.3 there is
  no `TIMESTAMP`, `DATE` or `BLOB`. Version 2.x was tested live
  (`dbine-test-iotdb2`, port 27150), except the transfer with 2.x's new
  types: that test leaves the container out of memory.
- **ksqlDB.** There is no rollback: rows already inserted stay, a retry
  duplicates, and if the server rejects a request unexpectedly, the rows of
  that request are uncertain. `NaN` is rejected. An ordinary `CREATE TABLE` is
  not read to the end (it only answers push queries) and gives `Unsupported`.
  A `TIME` loses milliseconds.

**Shared by all**

- The source is always read-only, also in direct copy.
- Loads never replace existing rows: a key that is already there fails the
  load in the engines that detect it (Cosmos, DynamoDB, Couchbase, CouchDB,
  etcd, Solr, Phoenix).
- When the load fails or is cancelled, requests that were already on their
  way are awaited before returning control, so nothing is committed
  afterwards. Where the engine has no transactions (Cassandra, Redis,
  MongoDB, ksqlDB, TDengine…), what was already committed stays and the error
  says so.

## SSH tunnel

All engines that connect over the network can use an SSH tunnel
([`ssh-tunnels.md`](ssh-tunnels.md)): the tunnel is a local port, so it does
not depend on the driver.

| Engine | Reason |
|---|---|
| SQLite, DuckDB and other local-file engines | There is no server: a file on the machine is opened. |
| ODBC with DSN | The connection is built by the system's ODBC driver from the DSN; DBine does not see the server to redirect it. A tunnel can be used with a DSN that points to `127.0.0.1`. |

## Locks

The Monitor's locks panel ([`locks.md`](locks.md)). These have locks and
allow terminating sessions: SQL Server, Azure SQL, PostgreSQL, TimescaleDB,
AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu, KingbaseES, openGauss,
Greenplum, Cloudberry, Greengage, YugabyteDB, CockroachDB, H2, Redshift,
Yellowbrick, MySQL, MariaDB, Aurora MySQL, TiDB, Oracle, Oracle Autonomous,
SAP HANA, MongoDB, Amazon DocumentDB, Neo4j, Snowflake and, through ODBC,
Db2 LUW, Sybase ASE and SQL Anywhere.

Redshift, Yellowbrick, KingbaseES, Fujitsu, SAP HANA, DocumentDB, Snowflake,
Db2, Sybase ASE and SQL Anywhere were implemented following the vendor's
documentation, without a test server.

| Engine | What is missing | Reason |
|---|---|---|
| Informix, GBase 8s (ODBC) | Terminating sessions | Terminating a session requires `task('onmode','z',…)` from the sysadmin database, which only runs when connected to that database. |
| Babelfish | Everything | Babelfish does not implement over TDS the SQL Server views that report locks (`sys.dm_exec_requests.blocking_session_id`). |
| Microsoft Fabric Data Warehouse | Everything | The warehouse does not expose the lock views or `KILL`. |
| Firebird | Everything | The MON$ tables do not report which transaction or connection blocks another (only that a statement is active); without that there is no lock chain. |
| Materialize | Everything | Materialize has no locks between sessions: it orders reads and writes by timestamps, without locks. |
| RisingWave | Everything | It is a streaming database without interactive write transactions. |
| CrateDB | Everything | It has no transactions or row locks: no session waits for another. |
| Denodo | Everything | It is a virtualization layer: locks happen in the data sources, not in Denodo. |
| OceanBase | Everything | There is no documented, stable lock-wait view accessible through SQL in MySQL mode. |
| SingleStore | Everything | It does not reliably expose through SQL who blocks whom. |
| StarRocks, Doris, VeloDB, Databend, Manticore, GreptimeDB | Everything | Analytical or search engines without row locks between sessions. |
| Db2 for i, Db2 for z/OS (ODBC) | Everything | Their lock-wait views differ from Db2 LUW and could not be validated. |
| Other engines through ODBC | Everything | There is no reliable lock-wait view for that engine through ODBC. |
| FerretDB | Everything | It does not report lock waits or allow terminating operations (it has no `killOp`). |
| Memgraph | Everything | It does not make a transaction wait for another: the second write fails right away with a serialization error. |
| Amazon Neptune | Everything | It does not report locks between transactions. |
| Azure Cosmos DB | Everything | It uses optimistic concurrency with ETags: there are no locks between sessions. |
| Cloud Spanner | Everything | Locks are only seen as historical statistics (`SPANNER_SYS.LOCK_STATS_*`), without who blocks whom live, and another client's transaction cannot be terminated. |
| Databricks | Everything | Delta uses optimistic concurrency: a conflicting write fails, it does not wait. |
| ClickHouse | Everything | There are no transactions or row locks; table lock waits do not say who holds them. |
| Trino, Presto, Starburst | Everything | It is a query engine without locks between queries; the waits are resource group queues. |
| Redis, Valkey, Dragonfly, etcd | Everything | There are no transactions that wait for others: each command runs on its own. |
| Cassandra, ScyllaDB, Amazon Keyspaces | Everything | There are no locks between sessions (lightweight transactions use Paxos, with no visible waits). |
| Elasticsearch, OpenSearch, Solr | Everything | There are no transactions or locks between clients. |
| InfluxDB, IoTDB, TDengine, ksqlDB, BigQuery, Athena, Dremio, Drill, Phoenix, Couchbase, CouchDB, DynamoDB, Flight SQL, libSQL, Aurora DSQL | Everything | They do not report lock waits between sessions that can be queried. |
| SQLite, DuckDB | Everything | They are file databases without a server: there are no other sessions to see. |

## Users and permissions

The **Users and permissions** tab ([`users-and-permissions.md`](users-and-permissions.md)).
These have it:

- **Relational and compatibles:**
  - SQL Server family: SQL Server, Azure SQL, Microsoft Fabric Data Warehouse and Babelfish.
  - PostgreSQL family: PostgreSQL, TimescaleDB, YugabyteDB, openGauss, Cloudberry, Greengage, Greenplum, KingbaseES, EDB, Fujitsu, Yellowbrick, AlloyDB, Cloud SQL, Aurora PostgreSQL, Aurora DSQL, CockroachDB, Materialize, Redshift, CrateDB, H2 and RisingWave.
  - MySQL family: MySQL, Aurora MySQL, Cloud SQL para MySQL, MariaDB, TiDB, OceanBase, SingleStore, StarRocks, Apache Doris, VeloDB and Databend.
  - Others: Oracle, SAP HANA and Firebird.
- **Analytical and cloud:** ClickHouse, Timeplus Proton, Snowflake, Databricks, BigQuery, Cloud Spanner, Trino, Presto, Starburst and Dremio.
- **Documents, graphs and key-value:** MongoDB, FerretDB (users only), Amazon DocumentDB, Couchbase, CouchDB, Azure Cosmos DB, OrientDB, Neo4j, Memgraph, Cassandra, ScyllaDB, Redis, Valkey, Dragonfly and etcd.
- **Search and time series:** Elasticsearch, OpenSearch, Solr, InfluxDB 1.x, TDengine, IoTDB and TimechoDB.
- **Through ODBC:** Db2 LUW, Db2 for i, Db2 for z/OS, Hive/Cloudera, Impala, Vertica, Exasol, Teradata, Sybase ASE, SQL Anywhere, Informix, GBase 8s, Netezza, Altibase, Dameng, CUBRID, Zen, Mimer, MonetDB, IRIS/Caché, MaxDB, NuoDB, HeavyDB, SQream, Ingres, Virtuoso, OpenEdge, Machbase, Ignite and Ocient.

Tested against real servers:

- SQL Server, Babelfish, PostgreSQL, TimescaleDB, YugabyteDB, openGauss, Cloudberry, Greengage, CockroachDB, Materialize, H2 and RisingWave.
- MySQL, MariaDB, TiDB, StarRocks, Apache Doris, Databend, ClickHouse and Oracle.
- MongoDB, Couchbase, Neo4j (Enterprise and Community), Memgraph Community, Cassandra, ScyllaDB, Redis and etcd.
- Elasticsearch, OpenSearch, Solr, InfluxDB, TDengine, IoTDB, OrientDB and Trino.
- Dremio: only the OSS edition, where permissions are an Enterprise feature.
- Cosmos DB and Aurora DSQL: against the Cosmos emulator and, for DSQL, against a test PostgreSQL.
- Spanner and BigQuery: their emulators run the scripts but do not enforce permissions.

The rest was implemented following the vendor's documentation, with unit tests: HANA, Snowflake, Databricks, Fabric, SingleStore, OceanBase, Firebird, CouchDB and the ODBC presets. In the ODBC presets there are catalog details still to be confirmed against a server: Netezza's privilege bits, Teradata's codes and SQream's columns.

| Engine | What is missing | Reason |
|---|---|---|
| Neo4j Community, Memgraph Community | Roles and permissions | They are Enterprise edition features; users are seen and managed. |
| Neo4j, Memgraph, Cassandra, ScyllaDB | "Can grant them to others" | Those engines have no `WITH GRANT OPTION`. |
| Neo4j | Permissions for a user | Neo4j grants permissions to roles only. |
| CockroachDB, Redshift | Permissions on the whole database | The script cannot point to the current database (without a dynamic `DO`); the `GRANT` is written by hand. |
| Redis, Valkey, Dragonfly, etcd, InfluxDB 1.x | Roles (Redis, InfluxDB) / disable (etcd, InfluxDB) | The engine does not have them: permissions go per user (ACL rules in Redis). |
| Elasticsearch, OpenSearch | Adding or removing a single role or privilege | The API rewrites the whole role or user; the script explains it and it is done from the console. |
| MongoDB, CouchDB, Memgraph, OpenSearch (internal users) | Disabling login | The engine has no such option: the password is changed or the user is dropped. |
| FerretDB | Roles and permissions | It does not have them: every user who logs in has full access. |
| InfluxDB 2 and 3 | Everything | They authorize with API tokens, not with users and permissions that can be changed through queries. |
| Amazon Neptune, Amazon Keyspaces | Everything | Access is controlled with AWS IAM, not from the engine. |
| Denodo | Everything | It manages users and roles from its server (VQL/console), not through PostgreSQL SQL. |
| Manticore | Everything | It has no users or permissions. |
| GreptimeDB | Everything | Users come from the server configuration (static provider), not from SQL. |
| SQLite, DuckDB | Everything | They are file databases without users. |
| libSQL | Everything | It authorizes with JWT tokens, not with SQL users. |
| Drill, ksqlDB | Everything | Users come from the server configuration (PAM, JAAS), they are not managed through queries. |
| Flight SQL | Everything | It is a generic protocol: permissions depend on the server behind it. |
| Phoenix | Everything | Its `GRANT`/`REVOKE` (only with `phoenix.acls.enabled`) write HBase ACLs (`hbase:acl`) that cannot be read through SQL: there is no `SHOW GRANTS`, no `SYSTEM` table, and no Avatica metadata with the permissions. Users and groups belong to HBase, Kerberos or LDAP, and Phoenix neither lists them nor reports the current user. |
| Snowflake | Granting permissions to a user | Snowflake grants permissions to roles only: the script does it on a role and explains how to add the user. |
| Databricks | Creating users and groups, passwords, memberships | They are managed in the account or workspace console (SCIM), not with SQL. Unity Catalog has no `WITH GRANT OPTION`. |
| Trino, Presto, Starburst | Users, passwords | Trino has no users of its own (they come from the authenticator); roles and permissions depend on the connector and the configured access control (for example, `hive.security=sql-standard`). |
| Aurora DSQL | Passwords | Login uses IAM tokens; the link to an IAM role is left commented in the script to fill in the ARN. |
| Cloud Spanner | Users, permissions on the whole database, `WITH GRANT OPTION` | Users are IAM principals; Spanner does not have those permissions. |
| BigQuery | Users, groups, project roles | They belong to Google Cloud IAM; project roles need the project name and another API. It is seen per dataset. |
| Athena | Everything | Permissions are managed in IAM and Lake Formation; there is no SQL for that. |
| OrientDB | Server users, roles within roles, `WITH GRANT OPTION` | Server users are in its configuration; OrientDB does not have the rest. |
| Solr | Granting and revoking permissions, roles, disable | `set-permission` and `set-user-role` replace the whole list, so they are done from the console; roles exist only while something names them; basic authentication does not disable users. |
| TDengine | Roles | The engine has no roles. |
| IoTDB | Disable, roles within roles | The engine does not have them. |
| Couchbase | Group members, permissions on a scope, blocking users | SQL++ replaces the whole group list and does not have the rest. It needs Couchbase 8.0 or later; groups and most roles are Enterprise. |
| Azure Cosmos DB | Roles, passwords, disable | They are data-plane users and permissions (resource tokens); Entra ID access control belongs to Azure's control plane. |
| Dremio | Everything in the OSS edition; permissions on a whole space or source | Permissions are Enterprise and Cloud; from the name it is not known whether it goes `ON SPACE` or `ON SOURCE`. |
| Microsoft Fabric Data Warehouse | Passwords, disable, dropping users | Users are Entra ID identities and access is given with workspace roles; `DROP USER` is not documented. |
| Babelfish | `DENY`, permissions on the whole database, `ALTER`/`CONTROL`/`VIEW DEFINITION`, names with `]` | Babelfish rejects them. |
| H2 | Disable, `WITH GRANT OPTION` | H2 does not have them. It lowercases user names. |
| RisingWave | Roles, memberships, permissions on the current database without a name | RisingWave has no roles; permissions on a database are given by choosing it by name. |
| StarRocks, Apache Doris, VeloDB, Databend | Disabling login; in Doris and Databend, `WITH GRANT OPTION`; in Doris, roles within roles | The engines do not have them. In Doris, table permissions are not yet offered from the tab: they are only seen and revoked. |
| SingleStore | A role granted directly to a user | Its model is user → group → role. |
| DynamoDB | Everything | Permissions belong to IAM, a separate service. |
| ODBC: Informix, GBase 8s, Db2 (LUW, i, z/OS), Hive, Impala | Creating users and passwords | Users belong to the operating system, LDAP, Kerberos or RACF. |
| ODBC: Spark, Kyuubi, Access, dBase, Ignite 3, NetSuite, generic ODBC | Everything | Spark and Kyuubi authorize from the catalog or Ranger; Access and dBase have no users; Ignite 3 is configured in the cluster; NetSuite is read-only; with generic ODBC it is not known which engine is behind. |

### Map login

**Map login…** creates a database user for a server login that already
exists ([`users-and-permissions.md`](users-and-permissions.md#map-a-login)).
It only applies where a database user and a server login are separate
principals.

| Engine | Script | Logins offered |
|---|---|---|
| SQL Server | `CREATE USER … FOR LOGIN … [WITH DEFAULT_SCHEMA = …]` | `sys.server_principals` (SQL, Windows and Entra ID logins and groups) with no user of the same SID in the database; not `sa` nor the `##…##` certificate logins. Disabled logins are listed. |
| Babelfish | Same as SQL Server; names with `]` are rejected | Same query, which Babelfish answers. Tested on `dbine-test-babelfish`. |
| Azure SQL Database | Same as SQL Server | From `master` only. From a user database `sys.server_principals` only shows the caller's own login, so the login is typed. |
| Sybase ASE (ODBC) | `exec sp_adduser 'login', 'user'` | `master..syslogins` with no user (`sysusers`) and no alias (`sysalternates`) in the current database. ASE has no default schema. Unit tests only. |

Tested on `dbine-test-sqlserver` and `dbine-test-babelfish`: the login shows
up as unmapped, the script creates the user with that login's SID and the
default schema, and the login leaves the list.

| Engine | Reason it doesn't have it |
|---|---|
| Microsoft Fabric Data Warehouse | It has no logins: users are Microsoft Entra ID identities. |
| PostgreSQL family, MySQL family, Oracle, SAP HANA, Firebird, ClickHouse, Snowflake, the other ODBC presets (SQL Anywhere, Db2, Informix, Teradata…) and the remaining engines with users | The user is the login: a single principal signs in and holds the permissions, so there is no separate server login to map. |
| MongoDB, Amazon DocumentDB | Users are created in each database with their own credentials; there are no server logins. |
| Engines whose users come from IAM, the operating system or the server configuration (Spanner, BigQuery, Databricks, Trino, Hive, Impala…) | The users aren't created from the engine. |

## New schema and drop schema

Create and drop schemas from the explorer ([`schemas.md`](schemas.md)).
Right click on a database → "New schema…" (with owner and permissions on
creation, in a single script that is shown before running it) and on a schema
→ "Drop schema…". Only in engines whose explorer shows schemas.

These have it:

- **SQL Server family:** SQL Server, Azure SQL, Microsoft Fabric Data Warehouse and Babelfish.
- **PostgreSQL family:** PostgreSQL, TimescaleDB, YugabyteDB, openGauss, Cloudberry, Greengage, Greenplum, KingbaseES, EDB, Fujitsu, Yellowbrick, AlloyDB, Cloud SQL, Aurora PostgreSQL, Aurora DSQL, CockroachDB, Materialize, Redshift, RisingWave and H2.
- **Analytical and cloud:** Snowflake, Databricks, Trino, Presto, Starburst, Dremio (folders) and Cloud Spanner.
- **Others:** DuckDB, Arrow Flight SQL, Couchbase (scopes) and Apache Phoenix.
- **Through ODBC:** Db2 LUW, Db2 for i, Hive/Cloudera, Impala, Spark, Kyuubi, Vertica, Exasol, Netezza, Dameng, MonetDB, Mimer, MaxDB, NuoDB, Ignite 3, SQream and Ocient.

**Empty schemas in the explorer.** A newly created schema, with no objects,
shows up in the tree in all engines with "New schema", because they list
their schemas:

- **SQL Server family:** `sys.schemas`. In Babelfish it only returns `dbo`,
  `guest` and the user schemas.
- **PostgreSQL family:** `pg_namespace`; H2 and CrateDB,
  `information_schema.schemata`. In openGauss, each user's personal schema is
  listed as a user schema. Materialize lists only the current database and
  the `mz_*` ones.
- **Analytical and cloud:** Snowflake, Databricks
  (`<catalog>.information_schema.schemata`), Trino, Presto, Starburst,
  Dremio (folders), Cloud Spanner and Aurora DSQL.
- **Others:** DuckDB (without the `system` catalog), Flight SQL
  (`GetDbSchemas`; if the server does not answer it, schemas come from the
  tables again), Couchbase (scopes) and Phoenix.
- **Through ODBC:** Db2 LUW and Db2 for i, Vertica, Exasol, Dameng, MonetDB,
  Mimer, MaxDB, NuoDB, SQream and Hive/Cloudera/Impala/Spark/Kyuubi, with
  each one's catalog query; Netezza, Ocient and Ignite 3, with the ODBC
  driver's `SQLTables`.

System schemas are marked and the tree hides them while they have no
objects: `sys`, `INFORMATION_SCHEMA`, `guest` and those of a fixed `db_*`
role in SQL Server (so a user schema owned by a `db_*` role also counts as
system), plus `queryinsights` in Fabric; `pg_catalog`, `information_schema`
and each engine's own in the PostgreSQL family (`_timescaledb_*`,
`timescaledb_information` and `timescaledb_experimental` in TimescaleDB;
`crdb_internal` and `pg_extension` in CockroachDB; `mz_*` in Materialize;
`rw_catalog` in RisingWave); `information_schema`, `pg_catalog` and `sys` in
Flight SQL; `_system` in Couchbase. In the ODBC presets, Db2, Vertica and
MonetDB use the catalog's own mark, and Netezza, Ocient and Ignite 3, the
preset's list.

In SAP HANA and Oracle, schemas are the explorer's databases and are listed
even when empty (`SYS.SCHEMAS` in HANA; `ALL_USERS` in Oracle, except in 11g,
where only users with objects appear). Denodo has no schemas. In engines
without "New schema" that show schemas, a schema appears when it has its
first object, because it comes from the object list.

**Permission to create.** "New schema…" is disabled when the server says the
user cannot create schemas: SQL Server and Azure SQL
(`HAS_PERMS_BY_NAME(…, 'CREATE SCHEMA')`), the PostgreSQL family (`CREATE` on
the database), Cloud Spanner (`databases.updateDdl`), Aurora DSQL, Db2 LUW
(`DBADM`) and SAP HANA. It stays enabled without a check in Fabric,
Babelfish, RisingWave, H2 (except administrators: `ALTER ANY SCHEMA` is not
read), Snowflake (except with the system roles), Databricks, Trino and
Dremio.

### Engines without "New schema"

| Engine | Reason |
|---|---|
| Oracle | A schema is a user with a password: it is created and dropped from "Users and permissions". |
| MySQL family (MySQL, Aurora MySQL, Cloud SQL para MySQL, MariaDB, TiDB, OceanBase, SingleStore, StarRocks, Apache Doris, VeloDB, Databend) | The schema is the database: it is created with "New database". |
| SAP HANA | HANA schemas are the explorer's databases: they are created and dropped with "New database" and "Delete database". Pending: choosing the owner (`OWNED BY`) and permissions from "New database". |
| Firebird | It has no schemas before version 6.0. Pending: showing and creating them in Firebird 6.0. |
| Generic ODBC | It is not known which engine is behind. |
| ODBC: Sybase ASE, SQL Anywhere, Informix, GBase 8s, Altibase, Ingres, OpenEdge, Machbase | The schema is the user who owns the objects: it is created with the user, from "Users and permissions". |
| ODBC: Teradata | A schema is a database with its own space (`CREATE DATABASE … PERM`), not a simple object. |
| ODBC: Db2 for z/OS, IRIS/Caché, Virtuoso, Ignite 2 | The schema is an implicit qualifier: it appears when the first object that names it is created. |
| ODBC: CUBRID, Zen, Access, dBase, HeavyDB | They have no schemas. |
| ODBC: NetSuite (SuiteAnalytics Connect) | It is read-only. |
| Amazon Athena | The explorer has no schema level: Glue databases are the explorer's databases (Athena calls the database a schema). |
| Google BigQuery | The explorer has no schema level: datasets (what BigQuery calls a schema) are the explorer's databases. |
| SQLite, libSQL | They have no schemas: attached databases (`ATTACH`) are separate files, not schemas created with SQL. |
| ClickHouse | It only has databases, no schemas: they are created with "New database". |
| DuckDB: file queries | There is no database to store a schema in: files are read in an in-memory database. |
| Calcite Avatica (generic Phoenix) | The DDL is that of the engine behind the Avatica server, and it is not known which one it is. |
| CrateDB | It has no `CREATE SCHEMA` or `DROP SCHEMA`: a schema exists while it has some table. |
| Denodo | It has no schemas: a virtual database contains its views directly. |
| The other document, key-value, search and time series engines; Cassandra, ScyllaDB (keyspaces), Apache Drill (workspaces) | Their explorer has no schema level. |

### Differences by engine

| Engine | What is missing | Reason |
|---|---|---|
| SQL Server, Azure SQL, Microsoft Fabric, Babelfish | Dropping with its contents | T-SQL's `DROP SCHEMA` has no `CASCADE` and refuses while the schema has objects: they have to be dropped or moved first. |
| SQL Server, Azure SQL | The `UNMASK` permission on a schema | Only SQL Server 2022 and later accept it; it is not offered. |
| SQL Server, Azure SQL | Changing the owner after granting | The owner goes in `CREATE SCHEMA … AUTHORIZATION`: `ALTER AUTHORIZATION ON SCHEMA` deletes all the permissions already granted on the schema (verified live). Whoever creates without being `db_owner` needs `CREATE SCHEMA`, `IMPERSONATE` on the owner user (or `ALTER` on the owner role) and to be a member of `db_securityadmin` to grant on the schema being handed over. |
| Microsoft Fabric | Choosing the owner | `AUTHORIZATION` could not be verified without a warehouse; the schema stays in the creator's name. |
| Babelfish | "With grant option", names with `]` and permissions other than SELECT, INSERT, UPDATE, DELETE, REFERENCES and EXECUTE | Babelfish rejects `GRANT … ON SCHEMA … WITH GRANT OPTION` and does not read `]]` inside brackets. It also has no `ALTER AUTHORIZATION` on schemas and rejects `GRANT CREATE SCHEMA`: whoever creates needs `db_ddladmin` and `db_securityadmin`. |
| Materialize | "With grant option" | Materialize does not have it. It does not accept `AUTHORIZATION` either: the owner is assigned afterwards with `ALTER SCHEMA … OWNER TO`. |
| H2 | "With grant option" | H2 does not have it. Permissions on a schema are SELECT, INSERT, UPDATE and DELETE. Only an administrator creates schemas. |
| RisingWave, Redshift, H2 | A role or group as owner | The owner has to be a user and the owner list shows only users (RisingWave has no roles; Redshift rejects a group with a message; in H2 2.1, a role as owner leaves the database unable to open). |
| ODBC: Db2 LUW | Dropping with its contents | Db2's `DROP SCHEMA` only accepts `RESTRICT` (empty schema). |
| ODBC: SQream, Ocient | Dropping with its contents | `DROP SCHEMA` only drops an empty schema. |
| ODBC: Db2 for i, Mimer, MaxDB, NuoDB, Ignite 3, SQream, Ocient, Spark, Kyuubi | Choosing the owner | The schema stays in the creator's name (Spark and Kyuubi have no owner). |
| ODBC: Db2 LUW | Permissions on creation, without `ACCESSCTRL` | The owner goes in `CREATE SCHEMA … AUTHORIZATION` and the following `GRANT … ON SCHEMA` need `ACCESSCTRL` or `SECADM`: a `DBADM` without `ACCESSCTRL` creates the schema but fails when granting. Pending: handing over the owner at the end (`TRANSFER OWNERSHIP`). Untested: there is no Db2 container. |
| ODBC: Db2 LUW, Vertica, Netezza, Dameng | A role as owner | Only a user can own a schema (`AUTHORIZATION` names a user): the owner list shows only users. |
| ODBC: Hive, Cloudera, Netezza, Dameng, MonetDB, Mimer, NuoDB, Ignite 3, Ocient, Spark, Kyuubi, Db2 for i | Permissions on creation | Pending: the permissions script does not yet write `GRANT` on a schema in these engines (MonetDB has no per-schema permissions: the owner is given). |
| ODBC: MaxDB, SQream, Exasol | "With grant option" | Their permissions on a schema do not have it (Exasol does not grant object permissions with the option to grant them). The form shows the option anyway and the script rejects it in the preview; the same happens in H2, Materialize, Babelfish and Cloud Spanner. Pending: have `SchemaSpec` say whether the engine has it, in order to hide it. |
| ODBC: Vertica, Impala | `ALL` together with other permissions | `ALL` already includes them; it is chosen alone. Impala writes one `GRANT` per permission. |
| ODBC: Hive, Cloudera, Impala | Names with spaces or symbols | Database names only accept letters, numbers and `_`. |
| Presto | Choosing the owner, permissions on creation and dropping with its contents | Presto does not accept `AUTHORIZATION` or `ON SCHEMA`, and `DROP SCHEMA … CASCADE` is "not yet supported": it only drops an empty schema. |
| Trino, Starburst | Nothing, but it depends on the catalog | Owner, permissions and `CASCADE` are accepted or rejected by the connector. The owner is changed at the end of the script (`ALTER SCHEMA … SET AUTHORIZATION`); the `memory` catalog rejects that change, roles and permissions ("does not support permission management"). |
| Snowflake | Dropping only if empty | Snowflake's `DROP SCHEMA` always drops the contents (`RESTRICT` only stops for foreign keys from other schemas); the script warns about it. |
| Snowflake | A user as owner | The owner is always a role (`GRANT OWNERSHIP … TO ROLE … COPY CURRENT GRANTS`, at the end of the script to keep the permissions just granted): the owner list shows only roles. |
| Databricks | `WITH GRANT OPTION` | Unity Catalog does not have it: "with grant option" also grants `MANAGE` on the schema. |
| Dremio | Choosing the owner and dropping with its contents | A schema is a folder: it has no owner clause (it is the `OWNERSHIP` permission) or `CASCADE`. Folders are only created in catalog sources (Nessie, Iceberg REST, Arctic), not in spaces. |
| Dremio | Dropping a folder with a dot in its name, inside a source | The space or source comes from the database where the menu was opened, whole even if it has dots (`@ana.b`). The following folders are given by INFORMATION_SCHEMA with unquoted dots (`source.a.b`), the same for folder `a.b` as for `a` › `b`, so they are taken as nested folders. Spaces do not allow dots in their folders; in a source, folder `a.b` is dropped by writing the script with quotes (`source."a.b"`). |
| Dremio | "With grant option" | Dremio has no `WITH GRANT OPTION`: `MANAGE GRANTS` on the folder is also granted. Permissions on folders belong to Dremio Enterprise and Cloud; in the OSS edition they fail when run. |
| Google Cloud Spanner | Choosing the owner and dropping with its contents | Named schemas have no owner and `DROP SCHEMA` only drops an empty one. |
| Google Cloud Spanner | Permissions on creation, in the emulator | `USAGE` is offered (`GRANT USAGE ON SCHEMA … TO ROLE`, from fine-grained access control); the emulator rejects it when run. The live test verifies that rejection; against real Spanner it runs with `DBINE_TEST_SPANNER_SCHEMA_GRANTS=1` (untested: there is no instance). No "with grant option": Spanner does not have it. |
| DuckDB | Choosing the owner and permissions on creation | DuckDB has no users or permissions. |
| Arrow Flight SQL | Choosing the owner and permissions on creation | The engine's users cannot be listed over Flight SQL. |
| Arrow Flight SQL | Catalog where the menu was opened, outside DuckDB | The script writes the schema with its catalog (`"catalog"."schema"`). It is only verified that a DuckDB server (GizmoSQL) accepts it; with other engines (Doris, DataFusion, Dremio) it was not tested. With DuckDB behind, the session also sets the catalog with `USE` on connect, so if the catalog no longer exists the connection fails ("couldn't open the catalog"). |
| Couchbase | Choosing the owner and dropping only if empty | A schema is a scope (`bucket.scope`; the name comes completed with the bucket where the menu was opened). It has no owner and `DROP SCOPE` always drops its collections: "with its contents" has to be checked. |
| Couchbase | Permissions on creation, in Community Edition | Roles on a scope (`` GRANT … ON default:`bucket`.`scope` ``) belong to Enterprise Edition: Community rejects them when run ("Role … is not valid"). Untested on Enterprise (there is no container): the syntax is covered by unit tests. |
| Apache Phoenix | Choosing the owner and dropping with its contents | No owner and no `CASCADE`: `DROP SCHEMA` only drops an empty schema. It needs `phoenix.schema.isNamespaceMappingEnabled`. |
| Apache Phoenix | Names with spaces, symbols or non-ASCII letters | A schema is an HBase namespace: only ASCII letters, numbers and `_` (Phoenix does not accept `"` in a name either). Pending: DBine still accepts accented letters or `ñ`, which HBase rejects on run. |
| Apache Phoenix | Permissions on lowercase schemas | Permissions are HBase ACLs (R, W, X, C, A; they need `phoenix.acls.enabled`) and `GRANT … ON SCHEMA` uppercases the name: they are only granted on schemas with an uppercase name. "With grant option" adds the A (admin) permission, which is the one that allows granting in HBase. |
| Aurora DSQL | Dropping with its contents | DSQL runs one DDL statement per transaction and does not drop objects in cascade; the empty schema is dropped. Untested on real DSQL (there is no emulator). |

Tested against real servers (create with owner and permissions, check them in
the catalog, reject dropping a schema with objects, and drop):

- SQL Server 2022 (with a creator who is neither `db_owner` nor `sysadmin`)
  and Babelfish 5.4 (with a creator who is only in `db_ddladmin` and
  `db_securityadmin`): user and role owner, permissions and empty schemas in
  the list, with system ones marked.
- PostgreSQL 16, TimescaleDB, YugabyteDB, CockroachDB, openGauss, Greengage,
  Materialize, RisingWave and H2, with a creator who is not a superuser (in
  H2, an administrator), and the empty schema in the list.
- Trino, Presto, Cloud Spanner (emulator) and Aurora DSQL (against a test
  PostgreSQL, with a creator who is not a superuser).
- Dremio OSS: the rejection in `$scratch`, the syntax and an empty folder of
  a space, listed and dropped; there is no catalog source in the container.
- Oracle and SAP HANA (the explorer's databases): Oracle lists empty
  schemas; HANA, with unit tests only.
- DuckDB, Arrow Flight SQL (GizmoSQL) and Couchbase Community.

Not tested against a server, with unit tests following the vendor's
documentation:

- Azure SQL, Fabric and Redshift.
- Snowflake, Databricks, Couchbase Enterprise and real Spanner.
- Arrow Flight SQL with engines other than DuckDB.
- Apache Phoenix: the live test exists, but it still has to be confirmed
  against the test container.
- The ODBC presets: there are no containers or ODBC drivers for those
  engines.

## Create databases: options

"New database" ([`create-databases.md`](create-databases.md)) offers advanced
options, with "Show script" and suggestions from the server, in: SQL Server
and Azure SQL; the PostgreSQL family (PostgreSQL, TimescaleDB, EDB, Fujitsu,
AlloyDB, Cloud SQL, Aurora, KingbaseES, Greenplum, Cloudberry, Greengage,
YugabyteDB, openGauss, CockroachDB, Redshift, RisingWave and Yellowbrick); the
MySQL family (MySQL, MariaDB, TiDB, OceanBase, SingleStore, StarRocks, Doris,
VeloDB and GreptimeDB); Oracle, SAP HANA, Firebird, Sybase ASE and Netezza
(through ODBC); ClickHouse, Snowflake, BigQuery, Databricks, Athena and Cloud
Spanner; Cassandra, ScyllaDB, Amazon Keyspaces, Couchbase, CouchDB, OrientDB,
InfluxDB 1, 2 and 3, IoTDB, TDengine, Neo4j and Cosmos DB.

**Tested against real servers** (each driver's `tests/create_database.rs`):
SQL Server; PostgreSQL, TimescaleDB, openGauss, YugabyteDB, Greengage,
CockroachDB and RisingWave; MySQL, MariaDB, TiDB and GreptimeDB; Oracle,
Firebird, ClickHouse, Cassandra, Couchbase (Community), CouchDB, OrientDB,
InfluxDB 1, 2 and 3, IoTDB 1 and 2, TDengine and Neo4j Enterprise. BigQuery
and Spanner, against their emulators. Materialize and StarRocks, only
creation without options.

**Not verified against a real server** (they follow the vendor's
documentation): Azure SQL, Snowflake, Databricks, Athena, Redshift,
Yellowbrick, KingbaseES, Greenplum, Cloudberry, OceanBase, SingleStore, Doris,
SAP HANA, Sybase ASE, Netezza, Cosmos DB, ScyllaDB, Amazon Keyspaces and
Timeplus. EDB, Fujitsu, AlloyDB, Cloud SQL and Aurora use the same code as
PostgreSQL.

| Engine | What is missing | Reason |
|---|---|---|
| Materialize | options | `CREATE DATABASE` only takes the name. |
| Databend | options | `CREATE DATABASE` takes an `ENGINE` with a single useful value. |
| Memgraph | options | `CREATE DATABASE` only takes the name. |
| Timeplus Proton | options | `CREATE DATABASE` only takes the name. |
| Dremio | options | `CREATE` only takes the name. |
| Microsoft Fabric | options | Databases are created from its portal. |
| Babelfish | options | Babelfish's T-SQL has no `CREATE DATABASE` options. |
| Neptune | create databases | A single database per cluster: there is nothing to create. |
| MongoDB | create databases | Databases are created on their own when the first document is written. |
| DuckDB | options | The database is a file; showing its path in "Show script" requires changing the contract. See pending items. |
| Generic ODBC | options | It is not known which engine is behind, so a `CREATE DATABASE` with options cannot be built. |
| Trino | create databases | It does not create databases from DBine. See pending items (schemas). |
| Apache Drill, Apache Phoenix | create databases | They do not create databases from DBine. |
| Aurora DSQL | create databases | It does not create databases from DBine. |
| Arrow Flight SQL | create databases | It does not create databases from DBine. |
| Elasticsearch, Solr | create databases | They do not create databases from DBine. |
| DynamoDB, ksqlDB, etcd, Redis | create databases | They do not create databases from DBine. |
| SQLite, libSQL | create databases | They do not create databases from DBine. |
| H2 | create databases | It creates the database when connecting to a new name (with `-ifNotExists`). |
| CrateDB | create databases | A single database per cluster: it is organized in schemas. |
| Denodo | create databases | Databases are created from Denodo. |
| Manticore | create databases | It has no databases. |
| Db2, Informix, Teradata | create databases | They do not create databases from DBine. See pending items. |

**Explicit pending items:**

- **Db2, Informix and Teradata:** database creation has to be enabled first
  in their drivers; only then does it make sense to offer them options.
- **DuckDB:** the contract (`create_database_script`) would need to be able
  to show the path of the file that is going to be created.
- **Spanner with PostgreSQL dialect:** the driver speaks GoogleSQL, so it
  could not use the database it creates. Dialect support is needed in the
  driver.
- **Trino schemas:** their options need a schema options contract, which does
  not exist yet.
- **IoTDB 2:** orphaned TTL rules are left behind when dropping a database.
  It is a known bug, unresolved.

## Backups

The **Backups** tab ([`backups.md`](backups.md)).

**DBine copies** work in all engines. They are a script with the structure
and data in a local file, and are restored by running it.

**Server backups** are the engine's own. The engines in the table have them;
those not in it only have DBine copies.

| Engine | Backup | Restore | Delete | History | Tested against a server |
|---|---|---|---|---|---|
| SQL Server (and Managed Instance) | `BACKUP DATABASE/LOG`: full, differential or log, to disk or to a URL | Yes, under another name (moves the files) or over the existing one, in single-user mode | No | `msdb` | Yes (2022) |
| SAP HANA | `BACKUP DATA`: full, differential or incremental; file or Backint | Yes, from SYSTEMDB (`RECOVER DATA … CLEAR LOG`) | Yes (`BACKUP CATALOG DELETE`) | `M_BACKUP_CATALOG` | No |
| Oracle, Oracle Autonomous | Data Pump (`DBMS_DATAPUMP`) of a schema | Yes, with schema remapping | Yes (`UTL_FILE.FREMOVE`) | Data Pump jobs; in Autonomous, the `.dmp` files too | Yes (23 Free) |
| CockroachDB | `BACKUP … INTO`: full or incremental, of the database or the cluster | Yes, under another name | No | Backup jobs | Yes |
| CrateDB | `CREATE SNAPSHOT` | Yes | Yes | `sys.snapshots` | Yes |
| H2 | `SCRIPT TO` / `BACKUP TO` | Yes, of the `SCRIPT` format (`RUNSCRIPT`) | No | No | Yes |
| MySQL | `CLONE LOCAL DATA DIRECTORY` | No | No | The last clone | Yes (8.4) |
| TiDB | `BACKUP DATABASE` | Yes, under the same name | No | `SHOW BACKUPS` | Yes |
| SingleStore | `BACKUP DATABASE`: full or differential; local, S3, GCS or Azure | Yes, under another name | No | `MV_BACKUP_HISTORY` | No |
| OceanBase | `ALTER SYSTEM BACKUP` (of the tenant) | No | No | Backup jobs | No |
| StarRocks, Apache Doris | `BACKUP SNAPSHOT` to a repository | Yes | No | `SHOW SNAPSHOT` | StarRocks yes; Doris no |
| Manticore | `BACKUP TABLE` | No | No | No | Yes |
| GreptimeDB | `COPY DATABASE TO` | Yes, into tables that already exist | No | No | Yes |
| ClickHouse | `BACKUP DATABASE` to a disk, a path or S3; incremental | Yes, under another name | No | `system.backups` | Yes |
| DuckDB | `EXPORT DATABASE` (Parquet or CSV) | Yes (`IMPORT DATABASE`) | No | No | Yes |
| SQLite | `VACUUM INTO` | No | No | No | Yes |
| Snowflake | Backup sets or instant clone (`CLONE`) | Yes, into a new database or replacing the current one (`SWAP`) | Yes (in backup sets, only the oldest) | Backup sets and clones | No |
| Databricks | `DEEP CLONE` of each Delta table in a schema, in an SQL scripting block | Yes | Yes | Backup schemas | No |
| BigQuery | Snapshots of all the dataset's tables (`CREATE SNAPSHOT TABLE`) | Yes (`CLONE`) | Yes | `TABLE_SNAPSHOTS` | No |
| Cloud Spanner | `CREATE BACKUP` (DBine statement; uses the admin API) | Yes, into a new database | Yes | Backup list | No (the emulator has no backups) |
| DynamoDB | `CREATE BACKUP` (DBine statement; uses the DynamoDB API) | Yes, into a new table | Yes | Backup list | No (DynamoDB Local has no backups) |
| Amazon Keyspaces | Turns on point-in-time recovery for the tables | Yes, into a new table | Yes (turns it off) | Tables with recovery on | No |
| Memgraph | `CREATE SNAPSHOT` | Yes (`RECOVER SNAPSHOT`; replaces everything) | No | `SHOW SNAPSHOTS` | Yes |
| Redis, Valkey, Dragonfly | `BGSAVE` / `SAVE` | No | No | The last save | Yes |
| etcd | `snapshot save`: the snapshot stays on this machine | No | No | No | Yes |
| Elasticsearch, OpenSearch | Snapshots | Yes, under another name | Yes | Snapshots of each repository | Yes (Open Distro no) |
| Solr | Collections (SolrCloud) or cores (standalone) | Yes | Yes | In standalone, only the last one | Yes |
| Db2 LUW (ODBC) | `ADMIN_CMD('BACKUP DATABASE …')` | No | No | `DB_HISTORY` | No |
| Sybase ASE (ODBC) | `DUMP DATABASE` | Yes (`LOAD` + `ONLINE`) | No | No | No |
| SQL Anywhere, MonetDB, Virtuoso, Machbase (ODBC) | Each engine's backup command | No | No | No | No |
| Informix, GBase 8s (ODBC) | `task('ontape archive' / 'onbar')` | No | No | ON-Bar and level-0 files | No |
| Db2 for i (ODBC) | `SAVLIB` to a save file | Yes (`RSTLIB`) | Yes | `SAVE_FILE_INFO` | No |
| Vertica (ODBC) | `SAVE RESTORE POINT` (Eon 24.1+) | No | Yes | Restore points | No |
| Mimer, Dameng (ODBC) | Each engine's backup command | No | No | Yes | No |

**What is missing and why:**

| Engine | What is missing | Reason |
|---|---|---|
| PostgreSQL, TimescaleDB, EDB, Fujitsu, KingbaseES, openGauss | Server backups | They are done with client tools (`pg_dump`, `pg_basebackup`, `gs_basebackup`), not with SQL. |
| Redshift, Aurora PostgreSQL, Aurora MySQL, Aurora DSQL | Server backups | Snapshots are handled from the AWS API or console. |
| Cloud SQL, AlloyDB | Server backups | They are handled from the Google Cloud API or console. |
| Azure SQL Database | Server backups | The service makes its own backups; point-in-time restore is done from the portal or the API. |
| Microsoft Fabric, Babelfish | Server backups | They have no `BACKUP` or `RESTORE`. |
| YugabyteDB, Greenplum, Cloudberry, Greengage, Yellowbrick, RisingWave | Server backups | They are done with their own tools (`yb-admin`, `gpbackup`, `ybbackup`, `risectl`). |
| Materialize | Server backups | It does not store data of its own: its state is rebuilt from the sources. |
| Denodo | Server backups | It exports its metadata with its own tool; there is no server backup through SQL. |
| MariaDB, Databend | Server backups | They are done with tools (`mariadb-backup`, `bendsave`). MariaDB's `BACKUP STAGE` only locks the server for those tools. |
| Cloud SQL para MySQL, VeloDB | Server backups | They belong to the provider (console or API). |
| MySQL, Manticore, OceanBase | Restore | MySQL: the server is restarted with `--datadir` pointing to the copy. Manticore: `manticore-backup --restore` with the server stopped. OceanBase: restoring creates a new tenant from the sys tenant. |
| TiDB | Restoring under another name | TiDB restores under the original name and the tables must not exist. |
| MySQL, TiDB, SingleStore, OceanBase, StarRocks, Doris, Manticore, GreptimeDB, ClickHouse, CockroachDB | Deleting a backup | There is no statement for that: the folder or object is deleted in storage. |
| SQL Server | Deleting a backup | SQL cannot delete a `.bak` (`xp_delete_file` is not documented), and `sp_delete_database_backuphistory` deletes the database's whole history. |
| SQL Server | Restoring backups split across several files | They show in the history, but are not restored from DBine. |
| Timeplus Proton | Server backups | `BACKUP` writes empty metadata and `RESTORE` fails (tested on 3.0.31). |
| libSQL / Turso | Server backups | The server rejects `VACUUM INTO`; point-in-time restore belongs to the platform's API. |
| SQLite, Redis, etcd | Restore | There is no SQL or command for that: the file is replaced with the database or the server stopped. |
| Firebird | Server backups | `gbak` and `nbackup` only work through the Services API, which the driver's client does not implement. |
| MongoDB, FerretDB, Amazon DocumentDB | Server backups | There is no server command: `mongodump` is a client tool and Atlas and DocumentDB backups belong to the provider. |
| Neo4j | Server backups | `neo4j-admin database backup` (Enterprise), outside Cypher. |
| Amazon Neptune | Server backups | They are handled from the AWS API. |
| Cassandra, ScyllaDB | Server backups | Snapshots are made with `nodetool` (JMX) or ScyllaDB's REST API, not with CQL. |
| Memgraph | Deleting a snapshot | There is no command: old ones are discarded according to the configured retention. |
| InfluxDB 1, 2, 3 | Server backups | v1: `influxd backup` uses a separate RPC port. v2: the backup API is several HTTP transfers assembled by the `influx` client, and Cloud has no API. v3: the data are Parquet files in object storage. |
| CouchDB | Server backups | It has no backup API. `_replicate` asks for URLs with credentials inside the script, and DBine does not put passwords in scripts. |
| Couchbase | Server backups | The backup service is Enterprise edition only, on its own port; Community only has `cbbackupmgr`. |
| TDengine | Server backups | The open edition uses `taosdump`; Enterprise, taosX. Neither is SQL. |
| IoTDB | Server backups | It has no backup statement: the data directories are copied or external tools are used. |
| Dremio | Server backups | They are done with `dremio-admin backup`, a command-line tool on the server. |
| Azure Cosmos DB | Server backups | Backups and point-in-time restore belong to Azure's control plane, which cannot be reached with the account key. |
| OrientDB | Server backups | `BACKUP` and `EXPORT DATABASE` are console commands; SQL rejects them. |
| Athena | Server backups | It queries data in S3; protecting it is S3's job (versioning, replication) or AWS Backup. |
| Trino, Presto, Starburst, Drill, Flight SQL | Server backups | They are query engines without data of their own: backups belong to the storage behind them. |
| Phoenix | Server backups | They are HBase snapshots (shell or admin API), outside the scope of SQL. |
| ksqlDB | Server backups | Its state lives in Kafka topics. |
| Db2 LUW, SQL Anywhere, Informix, Vertica, MonetDB, Virtuoso, Dameng, Machbase, Mimer (ODBC) | Restore | Restoring is done with tools or with the server stopped (Db2: `RESTORE` is a CLP command that `ADMIN_CMD` does not run). |
| Db2 for z/OS, Teradata, Netezza, Exasol, IRIS, OpenEdge, Ingres, CUBRID, Zen, MaxDB, NuoDB, Ignite, Ocient, SQream, HeavyDB, Altibase (ODBC) | Server backups | They are done with their own tools or outside SQL. On z/OS, `DSNUTILU` returns the result in an output parameter that the script runner does not bind. |
| Hive, Impala, Spark, Kyuubi (ODBC) | Server backups | The data lives in HDFS or S3. Hive only has `EXPORT TABLE`, one table at a time. |
| Access, dBase, NetSuite, generic ODBC | Server backups | Access and dBase: the file is copied. NetSuite: it is read-only. Generic ODBC: it is not known which engine is behind. |

**Not tested against a server, to be confirmed:**
- **Databricks:** that the statement execution API accepts a `BEGIN … END` block.
- **ODBC:** the backup syntax of the presets in the table, written following the vendor's documentation.

### Progress and remaining time

A server backup or restore runs as a background task (**Tasks** panel), with the elapsed time in all engines. The script runs as a single statement, so progress comes from the engine's own views: DBine reads them every 2 s from a second session while the script runs. With a known total, the task also shows an **estimate of the remaining time**, calculated from the last minute's pace (it appears after 5 s; if progress stalls, the estimate grows). If the progress query fails (for example, without permission on the view), polling stops and the task goes on without a percentage.

| Engine | What it reports | Where it comes from | How the operation is recognized |
|---|---|---|---|
| SQL Server (and Managed Instance) | Backup and restore, in %; the post-backup check (`RESTORE VERIFYONLY`) appears as its own stage, with its estimate | `percent_complete` of `sys.dm_exec_requests` | By the `@@SPID` of the session that runs the script. The quick file reads before restoring (`FILELISTONLY`, `HEADERONLY`) are not counted. |
| Oracle, Oracle Autonomous | Data Pump export and import, in % | `percent_done` of the job (what `expdp ATTACH=` shows), with `DBMS_DATAPUMP.ATTACH` and `DETACH` right after | The user's running export or import job. The percentage advances as each object finishes: a schema that is one big table jumps from 0 to 99. `v$session_longops` receives no Data Pump rows in 23ai Free. In Autonomous Database the same query is used; untested against a server. |
| CockroachDB | Backup and restore, in % | `fraction_completed` of `SHOW JOBS` | The user's `BACKUP` or `RESTORE` job created since the script started. |
| MySQL | Backup (`CLONE LOCAL`), in bytes copied over the estimate | `performance_schema.clone_progress` | The clone in progress that started after the script. |
| TiDB | Backup and restore, in % | `SHOW BACKUPS` / `SHOW RESTORES` | By the `CONNECTION_ID()` of the session that runs the script. |
| SAP HANA | Backup (`BACKUP DATA`), in bytes transferred over the total | `M_BACKUP_PROGRESS` | The most recent backup in progress: if two run at once, the last one is shown. Untested against a server. |

**No progress, and why:**

| Engine | What is missing | Reason |
|---|---|---|
| SAP HANA | Restore progress | `RECOVER DATA` runs from SYSTEMDB with the database stopped; `M_BACKUP_PROGRESS` only reports backups. |
| OceanBase, StarRocks, Apache Doris, Redis, Valkey, Dragonfly | Backup progress | The statement (`ALTER SYSTEM BACKUP`, `BACKUP SNAPSHOT`, `BGSAVE`) returns right away and the backup continues on the server: its status is seen in the history. |
| CrateDB, H2, Manticore, GreptimeDB, DuckDB, SQLite, Memgraph, etcd, Solr, Snowflake, Databricks, BigQuery, Cloud Spanner, DynamoDB, Amazon Keyspaces, ODBC presets | Backup and restore progress | The engine does not publish the operation's progress while it runs: the task shows only the elapsed time. |
| SingleStore, ClickHouse, Elasticsearch, OpenSearch | Backup and restore progress | Explicit pending item: `MV_BACKUP_STATUS` (SingleStore), `system.backups` (ClickHouse) and `_snapshot/_status` (Elasticsearch, OpenSearch) could give the progress; what they report during the operation still has to be verified against a server. |

## Clone table

"Clone…" (explorer context menu) copies a table next to the original, under
another name, with its structure and its data. The clone is exact or it is not
made: what cannot be copied identically is rejected before anything is
written, with the reason, and the explorer does not offer the option where it
does not apply.

| Engine | What | Reason |
|---|---|---|
| Neo4j, Memgraph, Neptune | Not cloned | The nodes of a label are not copied without their relationships. |
| Redis, Valkey, Dragonfly, etcd | Not cloned | A key-value engine has no tables: each key is a standalone value. |
| ksqlDB | Not cloned | The data lives in Kafka topics: cloning the stream or table does not faithfully copy its messages. |
| InfluxDB 2 | Not cloned | Flux cannot drop a measurement (dropping is another API, `/api/v2/delete`): if the copy failed halfway, the clone could not be undone. Pending: a way to drop a measurement from the driver. |
| InfluxDB 3 | Not cloned | Its SQL is read-only and does not drop tables (dropping is another API, `/api/v3/configure/table`): same reason as InfluxDB 2. Pending, same as InfluxDB 2. |
| TDengine | Supertables and subtables are not cloned | The rows of a supertable live in subtables with their own name (`tbname`) and tags: the clone would need subtables with other names. A cloned subtable would be another subtable of the same supertable and its rows would appear twice in queries over it. |
| IoTDB | Series with aliases, tags or attributes, and series that are views | DBine does not read them yet in the device structure; the clone is rejected with the list of series. Pending: reading and recreating them. |
| Cassandra, ScyllaDB, Keyspaces | Tables with `counter` columns | A counter only changes by adding (`UPDATE … SET c = c + n`), not with `INSERT`, and a retry of the addition would duplicate the value. |
| CouchDB | Not cloned | Its documents (`_all_docs`) are the whole database, not a table inside it: the clone would be another database. To copy it, create another database and use "Migrate…". |

**Particulars:**
- **TDengine:** the clone of a normal table is created with its own `SHOW CREATE TABLE`
  (composite key, encoding, compression and level of each column, `TTL`,
  comment) and is compared against it before copying the rows.
- **IoTDB:** the new device's name is a single level (letters without
  accents, numbers and `_`); if there are already series of another device
  under that path (for example `px.inner` when cloning as `px`), it is
  rejected, because dropping the clone (`DELETE TIMESERIES px.**`) would also
  take the other's.
- **InfluxDB 1:** the measurement is created with its first points; tags remain
  tags and fields keep their type (it is compared after the copy). The default
  retention policy is copied; if the measurement has points in another one, it
  is rejected. Without copying the data there is no clone (a measurement
  without points does not exist).
- **Cassandra, ScyllaDB, Keyspaces:** the name only allows letters without
  accents, numbers and `_`. Each row's TTL and write time (`writetime`)
  cannot be copied: in the clone, rows expire according to the table's default
  TTL (or do not expire) and their write time is that of the copy. The result
  of the clone warns about it.
- **Document and search engines** (MongoDB, FerretDB, DocumentDB,
  Couchbase, Cosmos DB, Elasticsearch, OpenSearch, Solr): whole documents are
  copied, with all their fields (not only those the structure sample saw) and
  their metadata (`_id` and routing in Elasticsearch/OpenSearch).
- **MongoDB, FerretDB, DocumentDB:** the clone is built under a name of its
  own (`<name>__dbine_tmp_<hash>`) and renamed at the end: if someone else
  created the collection with that name in the meantime, the clone is
  discarded and the other one's is left intact. Time series cannot be renamed
  and are built under their final name. Indexes with simple collation on a
  collection with a default collation are recreated with
  `{locale: "simple"}` (text ones included).
- **Elasticsearch, OpenSearch:** the full mapping is copied
  (`dynamic_templates`, `_routing`, `runtime`, `_meta`…) and all the index
  settings except those the server carries by itself (uuid, creation date,
  version). Ingest pipelines (`default_pipeline`, `final_pipeline`) and write
  blocks (`index.blocks.*`) are applied after copying the documents, which
  already went through the pipeline once. The clone is not added to the
  original's aliases or to its lifecycle policy (ILM/ISM); the result of the
  clone warns about it.

## Actions according to the user's permissions

Before offering an action that needs privileges, DBine asks the server what
the connected user can do. If the permission is missing, the button or menu
option shows disabled, with a tooltip that names what is missing. It is
queried once per connection and database, and again on reconnect.

The actions that are checked are: making a native backup, restoring it, the
profiler, terminating sessions from the Monitor, creating and dropping
databases, managing users and permissions, and creating schemas (which
engines check it is in [New schema and drop schema](#new-schema-and-drop-schema)).

The rule is never to disable an action the user could actually perform. When
the engine does not allow knowing for certain (permissions that arrive
through roles that cannot be resolved, per-resource permissions, IAM
systems), the action stays enabled and the server answers. The check looks at
the server's permissions; DBine's read-only mode is applied separately
(ClickHouse and the document and search engines also report it as a missing
permission).

### What is checked in each engine

| Engine | What is checked | How |
|---|---|---|
| SQL Server | All | `HAS_PERMS_BY_NAME` and `IS_SRVROLEMEMBER` in a single query. |
| Azure SQL Database | Backup, profiler, terminate sessions, users | `VIEW DATABASE STATE`, `KILL DATABASE CONNECTION`, `ALTER ANY USER`. |
| Oracle | All | `SESSION_PRIVS`, `SESSION_ROLES` and the permissions on the V$ views. |
| SAP HANA | All | `EFFECTIVE_PRIVILEGES`, which includes what arrives through roles. |
| Snowflake | Only with the system roles (ACCOUNTADMIN, SYSADMIN, USERADMIN); dropping a database depends on the owner | `IS_ROLE_IN_SESSION`. |
| PostgreSQL and derivatives (TimescaleDB, YugabyteDB, AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu, KingbaseES, Greenplum, Cloudberry, Greengage) | Profiler, terminate sessions, create and drop databases, users | Role attributes and membership in `pg_read_all_stats` and `pg_signal_backend`. |
| openGauss | Profiler, terminate sessions, create and drop databases, users | SYSADMIN, MONADMIN and the role attributes. |
| CockroachDB | All | `admin` role, role options, system privileges and the database's grants. |
| Materialize | Create databases, users | `has_system_privilege`. |
| H2 | Backup, restore, profiler, terminate sessions, users | Administrator rights. |
| MySQL, MariaDB, TiDB (and Aurora MySQL, Cloud SQL) | Profiler, terminate sessions, create and drop databases, users; backup in MySQL and TiDB, restore in TiDB | `SHOW GRANTS`, which includes the active roles. |
| StarRocks | Only with the system roles | `CURRENT_ROLE()`. |
| ClickHouse | All the ones it offers | `CHECK GRANT`, which takes roles and `readonly` mode into account. |
| Firebird | Profiler, create and drop databases, users | SYSDBA, RDB$ADMIN role, database owner and system privileges (Firebird 4+). |
| SQLite | Backup | It is disabled with `PRAGMA query_only`. |
| DuckDB | Restore, create databases | They are disabled if the database was opened read-only. |
| BigQuery, Cloud Spanner | Backup, restore, create and drop databases, users; profiler only to enable | `testIamPermissions`. |
| Aurora DSQL | Users | Role attributes. |
| Athena | Profiler | If AWS denies `athena:ListQueryExecutions`. |
| Trino, Presto, Starburst | Profiler | If the server rejects the query list. |
| Dremio | Create and drop spaces, profiler, users | In OSS everyone is an administrator; in Enterprise, `sys.privileges` and roles. |
| Apache Drill | Profiler | Drill administrator when authentication is on. |
| etcd | Backup, users | `root` role when authentication is on. |
| Db2 LUW, SAP ASE (ODBC) | Backup, terminate sessions, users; in ASE also restore and create and drop databases | The user's authorities (Db2) and the login's roles (ASE). |
| Redis, Valkey, Dragonfly | Backup, profiler, users | `ACL DRYRUN`, without running the commands. |
| Cassandra, ScyllaDB | Profiler, create and drop keyspaces, users | Superuser and `LIST ALL PERMISSIONS`, with inherited roles. |
| Neo4j | Profiler, terminate sessions, create and drop databases, users | `SHOW USER PRIVILEGES` in Enterprise; Community has no roles. |
| Memgraph | Backup, restore, profiler, create and drop databases, users | The user's privileges in Enterprise. |
| InfluxDB | 1.x: profiler, create and drop databases, users; 2.x: create and drop buckets; 3.x: profiler, create and drop databases | Administrator user (1.x), authorizations (2.x), administrator token (3.x). |
| IoTDB | Profiler, create and drop databases, users | `LIST PRIVILEGES OF USER`, with roles. |
| TDengine | Only to enable | The superuser enables everything; it is never disabled (see below). |
| MongoDB, FerretDB, Amazon DocumentDB | Profiler, terminate sessions, create and drop databases, users | `connectionStatus` with `showPrivileges`, which includes roles. |
| Elasticsearch | Backup, restore, profiler, users | `_security/user/_has_privileges`. |
| OpenSearch | Profiler, users; everything with the `all_access` role | `authinfo` and test reads of `_tasks` and of the internal users. |
| Solr | Backup, restore, users | The user's roles and authorization rules. |
| CouchDB | Create and drop databases; users only to enable | `_admin` role and the database's administrators. |
| Couchbase | Profiler, create and drop buckets, users | `checkPermissions`, which includes all roles. |
| OrientDB | Create and drop databases, users; only to enable | Server user or role with all permissions. |

Flight SQL, ksqlDB, Phoenix and libSQL have none of these actions.

### What cannot be checked

| Engine | What stays enabled without a check | Reason |
|---|---|---|
| Babelfish, Microsoft Fabric | Everything | `HAS_PERMS_BY_NAME` is not reliable in those variants. |
| Azure SQL Database | Create and drop databases | It is decided in `master` (dbmanager role), which a connection to another database does not see. |
| Snowflake | What arrives through custom roles | Reading the account privileges of custom roles requires walking the whole hierarchy with `SHOW GRANTS` or `ACCOUNT_USAGE`, which lags by hours. |
| Oracle | Permission on the Data Pump DIRECTORY | The directory is chosen in the backup form. |
| Redshift, RisingWave | Users, profiler, terminate sessions and drop databases for those who are not superusers | RBAC system privileges are not read. |
| CrateDB | What arrives through roles | The AL privilege granted to a role is not followed. |
| Materialize | Drop databases and profiler for those who are not superusers | The database owner and membership in `mz_monitor` are not read. |
| CockroachDB | Profiler and terminate sessions without the system privilege | A non-admin user cannot read their own role options. |
| OceanBase, SingleStore, Doris, VeloDB | Everything | Their permission model could not be verified: there is no test server. |
| Databend, GreptimeDB, Manticore | Everything | Different permission model (Manticore has no users). |
| StarRocks | What arrives through custom roles | Only the system roles are read. |
| Firebird | Dropping a database other than the connected one; creating databases in Firebird 4+ when the privilege is not seen | A system privilege granted by a role in the security database is not seen from the connected database. |
| ClickHouse before 24.5, Timeplus | Everything | They do not have `CHECK GRANT`. |
| SQLite, DuckDB | The backup's target folder | It is chosen at backup time. |
| libSQL | Everything | A Turso read-only token cannot be detected without writing. |
| Athena | Create and drop databases | IAM has no cheap check for the caller, and Glue has no simulation. |
| Databricks | Everything except enabling the profiler for administrators | Unity Catalog does not tell the user their own privileges, and querying them through SQL would wake the warehouse. |
| BigQuery | Drop datasets, restore and users when granted per dataset | The project check does not see per-dataset permissions. |
| Cloud Spanner | Profiler with fine-grained access control | The `spanner_sys_reader` role is not seen from IAM. |
| Trino, Presto, Starburst | Users | They depend on the connector's access control. The profiler shows enabled even if the user only sees their own queries. |
| Dremio | Users in OSS; dropping a space in Enterprise without an owner grant | OSS has no users; the grants list may not show the owner. |
| Other engines through ODBC | Everything | Each engine has its own permission model; generic ODBC does not know which engine is behind. |
| Redis before 7, KeyDB, users without `ACL DRYRUN` | Everything | Without `ACL DRYRUN` there is no way to ask; that command is an administration one. |
| Amazon Keyspaces, Amazon Neptune | Everything | IAM decides. |
| Neo4j Community, Memgraph Community | Create and drop databases | Those editions have a single database. |
| InfluxDB 2 | Almost everything, except with the operator token | The server hides the tokens and it is not possible to know which one is the user's own. |
| Azure Cosmos DB | Everything | A read-only key cannot be told apart from a read-write one without writing, and Entra ID roles are in the control plane. |
| DynamoDB | Backup and restore | There is no simulation of CreateBackup or RestoreTableFromBackup, and simulating an IAM policy requires IAM permissions of its own. |
| OrientDB | It is never disabled | A rejection on `/server` does not tell a database user from a server user without `server.info`. |
| Solr | Everything, if the user cannot read the rules (`security-read`) | Requests that match no rule are allowed: without seeing the rules, a missing permission proves nothing. |
| OpenSearch | Snapshots without `all_access` | It has no API to query one's own privileges. |
| CouchDB | Users for those who are not administrators | An ordinary user can still register users. |
| MongoDB through a compatible API (Cosmos DB for MongoDB) | Everything | They do not answer `connectionStatus` with privileges. |
| TDengine | Everything for those who are not superusers | In the test, the server let a user without SUPER or CREATEDB create and drop databases and users: those flags do not prove a rejection. |

### Tested against real servers

With an administrator user and a limited one: SQL Server, Oracle,
PostgreSQL, CockroachDB, H2, openGauss, Materialize, MySQL, MariaDB, TiDB,
StarRocks, ClickHouse, Firebird, Trino, Drill, Dremio OSS, etcd, Redis,
Valkey, Dragonfly, Cassandra, ScyllaDB, Neo4j (Enterprise and Community),
Memgraph Community, InfluxDB 1, 2 and 3 Core, IoTDB, TDengine, MongoDB,
FerretDB, Elasticsearch, OpenSearch, Solr, CouchDB, Couchbase, OrientDB, the
Cosmos DB emulator and DynamoDB Local. SQLite and DuckDB were tested with real
files, including read-only files.

Implemented from the documentation, without a test server: SAP HANA,
Snowflake, BigQuery, Cloud Spanner, Aurora DSQL, Databricks, Athena, Dremio
Enterprise, Db2 LUW, SAP ASE, Redshift, RisingWave, CrateDB, YugabyteDB and
the other PostgreSQL derivatives, Aurora MySQL, Cloud SQL, Memgraph
Enterprise, InfluxDB 3 Enterprise, Amazon DocumentDB and Open Distro.

## Script execution

How it works: [`script-execution.md`](script-execution.md). All engines that
run text run the script statement by statement, with messages, errors with
code and line, and cancellation. The exceptions are these.

Tested against real servers: SQL Server, Babelfish, PostgreSQL, TimescaleDB,
YugabyteDB, CockroachDB, MySQL, MariaDB, TiDB, Oracle, SQLite, DuckDB,
Firebird, ClickHouse, Flight SQL (GizmoSQL), Drill, Phoenix, Trino, Dremio,
InfluxDB 1, MongoDB, Neo4j, Redis, Elasticsearch, ScyllaDB, Couchbase and the
Cloud Spanner emulator. Spanner and Phoenix are emulators or containers: their
error codes may differ from the real service.

Implemented from the documentation and tested only with unit and cut tests,
without a server: SAP HANA, ODBC (presets), libSQL, Aurora DSQL, Presto,
Snowflake, BigQuery, Databricks, Athena, InfluxDB 2 and 3, etcd, Cosmos DB,
CouchDB, OrientDB, ksqlDB and Manticore (its test container did not start
because of a busy port on the test machine; unit tests only).

### Send mode

| Engine | Mode | Reason |
|---|---|---|
| Snowflake, BigQuery, InfluxDB 2 (Flux) | All the text at once; "Continue on error" has no effect | The server runs the script as a single request. Snowflake Scripting and BigQuery blocks are split correctly. |
| Firebird | All the text at once | It accepts `SET TERM` and blocks without a terminator. `isql`'s own commands are skipped with a warning. |
| ODBC other than Teradata (generic ODBC, Informix and GBase SPL bodies, Exasol scripts ending in `/`, Netezza, NuoDB, IRIS…) | Split by `;` with the generic lexer | Those bodies need the "All the text at once" mode on the connection. |
| Db2 | A loose `BEGIN [ATOMIC] … END` block is cut at its first `;` | `CREATE TRIGGER` / `PROCEDURE … BEGIN … END` stay whole. For anonymous blocks you have to use `--#SET TERMINATOR`, which is what the Db2 client does without an alternate terminator. |
| Athena, InfluxDB 1 | One statement per request | Athena creates one execution per statement; InfluxDB 1.x loses the error reason of a statement that shares a request. |

### Manual transactions

These offer Auto/Manual, Commit and Roll back: SQL Server, Babelfish,
PostgreSQL and derivatives, CockroachDB, MySQL, MariaDB, TiDB, OceanBase (and
Aurora and Cloud SQL MySQL), Oracle, SQLite, DuckDB, Flight SQL, Phoenix,
Trino, Spanner, Neo4j and Couchbase.

| Engine | What is missing | Reason |
|---|---|---|
| StarRocks, Manticore, GreptimeDB and other analytical MySQL variants | Manual transactions | The engine has no multi-statement transactions. |
| ClickHouse | Manual transactions | The engine has no multi-statement transactions (only an experimental feature, off by default). |
| Snowflake | A transaction left open at the end of the script is lost | There is one API session per execution. DBine warns if the script ends with an open transaction. |
| Spanner | `DDL` inside a transaction | Spanner does not support it; DBine rejects it with a message. |
| Oracle | "Failed" state | Oracle does not have it: a failed statement rolls back only itself and the transaction stays open. In the editor the mode starts at Auto because the connection has autocommit on. |
| MySQL, TiDB (autocommit off) | A `SELECT` leaves the transaction "open" | That is what the server reports (`SERVER_STATUS_IN_TRANS`). |
| Trino | After an error the transaction is left "failed" | The server aborts it and the following statements give `TRANSACTION_ALREADY_ABORTED` until it is rolled back. |
| SQL Server | A transaction that cannot be committed is rolled back at the end of the batch | The server does it (error 3998). |

The other engines do not offer Auto/Manual; the selector does not appear.

### `USE` and the tab's database

The tab follows the database change in all engines that have the concept.
Particulars:

| Engine | Behavior | Reason |
|---|---|---|
| Oracle | `ALTER SESSION SET CURRENT_SCHEMA` (also inside `EXECUTE IMMEDIATE`) acts as `USE` | Oracle changes schema, not database. |
| Databricks | `USE CATALOG` moves the tab; `USE SCHEMA` only changes the context (`Context: cat.schema`) | The tab shows the catalog. |
| Athena | `USE` is checked against the catalog's databases; an unknown one gives `SCHEMA_NOT_FOUND` | The engine does not validate `USE`. |
| InfluxDB 1 | `USE db[.rp]` can end at its line without `;`; it is checked with `SHOW DATABASES` / `SHOW RETENTION POLICIES` | It is InfluxQL syntax. |
| Redis | `SELECT n` switches to `db{n}`; inside `MULTI` the change happens at `EXEC`, and not if the `EXEC` fails, there is a `DISCARD` or `EXECABORT` | The server does not switch database until the transaction runs. |
| ODBC | Only in presets that have a database list | Hive, Teradata and similar do not have the concept. |

### Cancel

They keep the session: all, except Oracle and Babelfish (see
[Cancel](script-execution.md#cancel)). In Elasticsearch, if a request ignores
the `_tasks` cancellation (for example, a cluster health wait), the
cancellation waits for that request to finish; the following ones do not run.

### What is missing

| Engine | What is missing | Reason |
|---|---|---|
| All engines | SQLCMD mode, variables (`&var`, `:var`, `$(var)`), the editor's maximum time, client commands beyond `PROMPT` and `SHOW ERRORS` | Explicit pending item: they are not done. |
| PostgreSQL and derivatives | `COPY … FROM stdin` | Explicit pending item: the editor does not send data through the COPY channel (see below). |
| PostgreSQL and derivatives | Running the statement under the cursor splits with the dialect's lexer, not with psql's split | Explicit pending item: `split_for_ui` with `statements` uses the core lexer instead of the driver's `split_script`. Metacommands with apostrophes and `COPY` rows are not split psql-style in that run. |
| PostgreSQL and derivatives | A metacommand in the middle of an unclosed statement cuts the statement at that line | Explicit pending item: psql keeps the buffer and continues the statement after the metacommand. |
| PostgreSQL and derivatives | `SHOW` inside an open transaction does not report the column type; `RETURNING` does not report `rows_affected` (only the tag) | Explicit pending item. |
| Oracle | The line `select …;` followed by `/` runs once | Deliberate difference: SQL*Plus runs it twice because `/` re-runs the buffer. |
| Oracle | Cancelling without the `ALTER SYSTEM` privilege | The client has no interrupt call; without the privilege the cancellation is only recorded. |
| Oracle, engines with a plugin | SQL*Plus splitting in published builds | It arrives when the plugin host is republished; an older host answers `Unsupported` and the app splits with the dialect. |
| Oracle | A "Consistent" backup taken 1 to 5 seconds after a commit may lose those rows | `FLASHBACK_TIME` uses a coarse time-to-SCN map; the fix is to use `FLASHBACK_SCN`. Explicit pending item. |
| MySQL and family | Errors that are not syntax errors (for example 1054) point to the statement's first line | The server does not give the position. |
| Presto | `TABLE_NOT_FOUND` is located at the beginning of the statement | Presto does not report line or column. |
| Dremio | Errors carry no code | The REST API does not return it. |
| Spanner (emulator) | Some errors arrive as `failed to marshal error message` (`INTERNAL`) and a duplicate key error appears in the following statement | Emulator behavior; it was not tested against a real instance. |
| Redis | The error text keeps client prefixes (`ResponseError:`, `"WRONGTYPE":`) | Explicit pending item. |
| Redis, Elasticsearch, MongoDB | A syntax error that prevents splitting rejects the whole script before running anything | Explicit pending item: the lines before the error are not run. |
| Phoenix | An uncommitted `UPSERT` is not seen in a `COUNT` inside the transaction | Phoenix behavior with non-transactional tables. |
| Timeplus, Proton | Untested | The `dbine-test-proton` container was not started. |
| Firebird | The live test suite must run with `--test-threads=1` | The tests share one `test.fdb` and collide on concurrent DDL; it is not the driver's fault. |

### psql scripts in PostgreSQL and compatibles

The editor splits the script like psql, statement by statement. What psql
resolves on the client side does not reach the server:

| What | What DBine does | Reason |
|---|---|---|
| `\echo`, `\qecho`, `\warn` | Shows the text in the messages. | — |
| `\restrict` / `\unrestrict` (pg_dump) | They are skipped without a warning. | They only protect psql. |
| Other metacommands (`\set`, `\pset`, `\i`, `\gexec`…) | They are ignored with a warning; the script goes on. | DBine runs SQL only; there are no client variables or files. |
| `\connect` / `\c` | Error that stops the script. | The session does not change database: the rest would run in the wrong one. That database has to be opened in another tab. |
| `COPY … FROM stdin` | Error in that statement; its rows, up to the `\.` line, are not sent and the script continues with what comes after. | The editor does not send data through the COPY channel. To load them: Import data or `COPY … FROM 'file'` on the server. |

A metacommand ends at the end of its line, even if it has quotes or `;`, and
COPY rows end at their `\.` even if they have apostrophes or `;`, as in psql.
A line that starts with `\` inside a quoted text, a `$$ … $$` body or a
comment is part of that text.

## Index usage

When a table is expanded, the explorer marks the primary key columns (key)
and the foreign key columns (link, with the table and column they point to)
and adds the **Indexes** folder: each index with its type (PK, UNIQUE,
CLUSTERED, NC, COLUMNSTORE…) and what share of the table's reads goes
through it, or **unused** in red when it is written to but nobody reads it.
Clicking an index or right click on the table › **Indexes…** opens the tab
with the detail (key and INCLUDE columns, filter, size, seeks, scans,
lookups, updates, percentage of reads, writes per read and latest
accesses). **Compare schemas** shows each index's usage on each side, read
on its own connection. The contract is in
`crates/dbine-driver/src/index_usage.rs` (`Driver::supports_index_usage` and
`Session::index_usage`); the derived numbers are computed there, in
`IndexUsageReport::derive`.

- **Reads** = seeks + scans + lookups.
- **% reads** = the index's reads over those of all the table's indexes
  (empty if the table had no reads).
- **Seek health** (`seek_scan_split`): only where the engine separates point
  lookups from scans. Seeks over seeks + scans: green from 0.8, yellow from
  0.5, red below (a columnstore is never red). Where the engine has a single
  counter ("used N times"), it goes in seeks, the color is neutral and the
  small-table warning is not shown.
- **Writes** (`writes_counted`): where the engine does not count them per
  index, the tab shows a dash (unknown, not 0), there are no writes per read
  and no index comes out "unused": one without reads stays at 0 %.
- **Unused** = no reads and some writes, only with counters and counted
  writes.
- **Since when** (`since`): the tab says "Statistics since …" with the date
  the engine gives, or "since the last server restart" if it does not give
  one.
- Without counters (`stats_available` false: the engine does not have them
  or the user cannot read them) the indexes are listed anyway, with a
  warning that says why.

In the table, "—" in health and writes means the engine has no usage
counters. "Live" is against a real server or emulator (`dbine-test-*`
containers or local files); the detail of each test is in "Tested against
real servers", at the end of this section.

| Engine | Indexes | Counters | Since when | Seek health | Writes | Permissions | Live | Notes |
|---|---|---|---|---|---|---|---|---|
| SQL Server | `sys.indexes`, `sys.index_columns` | `sys.dm_db_index_usage_stats` (LEFT JOIN: never-used indexes stay at zero): `user_seeks`, `user_scans`, `user_lookups`, `user_updates` and the latest accesses | `sqlserver_start_time` | Yes | Yes | VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE since 2022); without that, no counters and the warning says so | Yes (2022) | Size from `sys.dm_db_partition_stats`. Heaps (`index_id` 0) are not listed: they are the table, not an index, and their reads do not count in the percentage. |
| Azure SQL Database | Same | Same | `sqlserver_start_time` if the database allows reading `sys.dm_os_sys_info`; otherwise "since the last restart" | Yes | Yes | VIEW DATABASE STATE | No | Counters also reset on a failover. |
| Babelfish | `sys.indexes`, `sys.index_columns` | It has no `sys.dm_db_index_usage_stats`: no counters, with a warning | — | — | — | — | Yes | — |
| PostgreSQL (and AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu, KingbaseES) | `pg_index` (PK included, DESC from `indoption`, INCLUDE, `indpred` filter); type = access method (BTREE, GIN, BRIN…); foreign keys from `pg_constraint` | `pg_stat_all_indexes.idx_scan` to seeks; `last_idx_scan` (16+) to the last read; writes = the table's `n_tup_ins + n_tup_upd - n_tup_hot_upd`, equal for all its indexes (every insert and every non-HOT update writes to each index; PostgreSQL does not count them per index). A partial index does not receive the table's writes (it is not known how many rows meet its filter): it stays without writes, never "unused", and the warning says so | `stats_reset` of `pg_stat_database`; if they were never reset, the warning says they run since the database was created | No | Yes (the table's) | None: statistics are readable by any user; if denied, no counters and the warning suggests `pg_read_all_stats` | Yes (16) | Partitioned tables add up the counters and size of their partitions (`pg_partition_tree`, 12+). The table's `seq_scan`s are not an index's reads: the warning says how many there were. Size from `pg_relation_size`. |
| TimescaleDB | Same | Same; in a hypertable each index adds up those of its chunks (the chunk's index with the same definition) | Same | No | Yes (the table's) | Same | Yes | — |
| YugabyteDB | Same | `idx_scan` of the node the session is connected to, not of the cluster | — (counters live in memory) | No | No | Same | Yes | No size. The warning says the numbers are from the node. |
| CockroachDB | `pg_index` (the `prefix` and `inverted` methods of 26.x are shown as BTREE and GIN, as in PostgreSQL, in the explorer, the comparison and index usage) | `crdb_internal.index_usage_statistics`: `total_reads` to seeks and `last_read`, for the whole cluster, joined with `crdb_internal.table_indexes` | — | No | No | Turns on `allow_unsafe_internals` only for that read; if the server denies it, no counters with a warning | Yes | No size. |
| Greenplum, Apache Cloudberry, Greengage | `pg_index` | `gp_stat_all_indexes_summary` (Greenplum 7) or `pg_stat_all_indexes`, which in Cloudberry already adds up the segments (each query counts one scan per segment serving it); writes from `gp_stat_all_tables_summary` or `pg_stat_all_tables`, like PostgreSQL | The coordinator's `stats_reset` | No | Yes (the table's) | Like PostgreSQL | Cloudberry yes; Greenplum 7 and Greengage no | Greenplum 6 only has the coordinator's statistics, which do not see the segments' reads: no counters, with a warning. |
| openGauss | `pg_index` | Like PostgreSQL (without `last_idx_scan` or `pg_partition_tree` partitions) | `stats_reset` | No | Yes (the table's) | Like PostgreSQL | Yes | — |
| Materialize, Yellowbrick | `pg_index` | They do not count usage: no counters, with a warning | — | — | — | — | Materialize yes; Yellowbrick no | Materialize has no primary or foreign keys; Yellowbrick has no secondary indexes. |
| RisingWave, CrateDB, H2 | From the table structure (`information_schema`, `pg_indexes`, SHOW CREATE TABLE in CrateDB): the PK and indexes | No counters, with a warning | — | — | — | — | Yes | CrateDB indexes every column on its own: the PK and full-text indexes are shown. H2 shows its foreign keys. |
| Amazon Redshift | It has no indexes (it sorts and distributes with SORTKEY and DISTKEY): the folder stays empty, with a warning | — | — | — | — | — | No | Foreign keys (informational) mark their columns. |
| Amazon Aurora DSQL | `pg_index` (PK included, DESC, INCLUDE, filter) | It has no usage statistics: no counters, with a warning | — | — | — | — | Against PostgreSQL (`dbine-test-dsqlpg`), not against DSQL | It has no foreign keys. |
| Oracle (and Autonomous Database) | `ALL_INDEXES`, `ALL_IND_COLUMNS`, `ALL_IND_EXPRESSIONS` (function columns and DESC), PK from `ALL_CONSTRAINTS`; without LOB indexes; invisible ones carry `INVISIBLE` in the type | `DBA_INDEX_USAGE` (12.2+): `TOTAL_ACCESS_COUNT` to seeks, `LAST_USED` to the last read (the time of the flush that recorded the access, not of the access); writes = "db block changes" of the index segments in `V$SEGSTAT` (blocks, not rows, since the instance started). Reads are kept across restarts and writes start from zero at each startup; the warning says so | — (Oracle does not say since when it counts); with MONITORING USAGE, the oldest `START_MONITORING` | No | Yes (without access to `V$SEGSTAT`, no) | SELECT_CATALOG_ROLE or SELECT ANY DICTIONARY; without that (or before 12.2), if all the indexes of an own table have `MONITORING USAGE`, only whether each one was used (`USER_OBJECT_USAGE`); otherwise no counters and the warning names the privilege | Yes (23ai Free) | Oracle counts by sampling and flushes to `DBA_INDEX_USAGE` every 15 minutes: the warning shows the last flush and warns that, until the next one, a newly created or newly used index has writes and 0 reads (it looks unused), and that one that is used little may not enter the sampling. Size from `DBA_SEGMENTS`, or from `USER_SEGMENTS` when the indexes belong to the user (an index with no segment yet shows as 0 KB); without `DBA_SEGMENTS`, those of another schema have no size (unknown, not 0 KB). There is no INCLUDE or filtered indexes. |
| Google Cloud Spanner | `INFORMATION_SCHEMA.INDEXES` / `INDEX_COLUMNS`: the primary key (the table is stored in its order), secondary ones (UNIQUE, NULL_FILTERED, `STORING` as included, `WHERE` filter), search and vector ones; without those Spanner manages for foreign keys | `SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR` (one row per table and per index, 30 days): reads (seeks) = sum of `READ_QUERY_COUNT`; writes = `WRITE_COUNT + DELETE_COUNT`. The primary key takes the table's; an index with no rows stays at zero | The oldest `INTERVAL_END` minus one hour (UTC) | No | Yes | `spanner.databases.select` (with fine-grained access control, the `spanner_sys_reader` role); without that, in the emulator or while the hourly table has no data, no counters and the warning names the permission | Emulator (without `SPANNER_SYS`: only indexes, keys and the warning) | Size: `USED_BYTES` of the last hour in `SPANNER_SYS.TABLE_SIZES_STATS_1HOUR`. Foreign keys from `REFERENTIAL_CONSTRAINTS`. **Drop index** generates `DROP INDEX` (or `DROP SEARCH/VECTOR INDEX`). |
| BigQuery | Search indexes (one per table) and vector indexes (one per column), from `SEARCH_INDEXES` / `VECTOR_INDEXES` (columns, `STORING`, status if not active) | Reads (seeks) = queries of the last 180 days that used the index according to the region's `INFORMATION_SCHEMA.JOBS` (`FULLY_USED` or `PARTIALLY_USED`, without those that say they did not use this table's index). Usage is recorded per query, not per table: the number is a maximum and the warning says so | The start of the 180-day window | No | No | `bigquery.jobs.listAll`; without it, `JOBS_BY_USER` (only your own queries) with a warning; with neither, no counters | No (unit tests only; the emulator does not have these views) | Reading `JOBS` is a billed query and runs every time the **Indexes** folder of a table with indexes is opened; the warning says so. With more than one vector index the job does not say which one it used: no counters. Size `total_storage_bytes`; last write, the last refresh. The primary key (not enforced) is not listed; foreign keys come from `tables.get`. |
| Snowflake | Hybrid tables: `SHOW INDEXES IN TABLE` (the primary key's, those of unique and foreign keys and secondary ones with `INCLUDE`). Standard tables have no indexes: the folder stays empty, with a warning | There are no per-index counters (`ACCESS_HISTORY` is per column) | — | — | — | — | No (unit tests only) | Foreign keys from `SHOW IMPORTED KEYS IN TABLE`. Hybrid table indexes do not yet enter the schema comparison, so **Drop index** does not find them: they are dropped with `DROP INDEX table.index`. |
| Databricks (and Azure Databricks) | It has no secondary indexes (per-file statistics, clustering, Z-order): the folder stays empty, with a warning | — | — | — | — | — | No (unit tests only) | Only Unity Catalog foreign keys (`information_schema`, informational) for the icons; without Unity Catalog there are none. |
| Dremio | It has no indexes: the table's **reflections** are listed (`sys.reflections`): RAW or AGGREGATION type (with the status if it cannot accelerate), columns or dimensions as key and measures as included | Reads (seeks) = `accelerated_count`, the queries the reflection accelerated | — (Dremio does not say since when it counts) | No | No (refreshes are not counted) | In Dremio Enterprise, VIEW REFLECTION to read `sys.reflections` | Yes (OSS) | Size `current_footprint_bytes`; last write, the last refresh. Reflections enter the schema comparison as table indexes (in a conversion to another engine they are omitted with a warning): **Drop index** generates `ALTER TABLE … DROP REFLECTION` and the sync creates or rebuilds them. No keys. |
| MongoDB (and Amazon DocumentDB) | `listIndexes`: `_id_` (or a clustered collection's index) as primary key, keys with DESC, type (BTREE, TEXT, 2DSPHERE, HASHED, WILDCARD; TTL, SPARSE, HIDDEN), `partialFilterExpression` as filter | `$indexStats`: `accesses.ops` to seeks; in a sharded collection the shards are added up. MongoDB does not count writes per index (only collection operations, which would include those from before the index was created and those of documents a partial index does not cover): they are not shown and the warning says so | The oldest `accesses.since` (server start or index creation) | No | No | The indexStats action (clusterMonitor role, or dbAdmin on the database) and collStats; without it, no counters and the warning names the role | MongoDB yes; DocumentDB no | Size from `storageStats.indexSizes`. No foreign keys. **Drop index** generates `dropIndex`. |
| FerretDB | Same | It answers `$indexStats` with everything at zero: no counters, with a warning | — | — | — | — | Yes | Size from `collStats`. **Drop index** works (FerretDB answers `ok: true` instead of `1`, and the driver accepts it). |
| Neo4j | `SHOW INDEXES` of the label or relationship type; those backing a constraint carry the constraint's name (KEY = primary key, UNIQUENESS = unique); without the LOOKUP ones, which are for all labels | Neo4j 5: `readCount` to seeks and `lastRead` to the last read (it flushes them every few seconds and counts each insert's uniqueness check as a read). Neo4j 4 has no `readCount`: no counters | The oldest `trackedSince` | No | No | SHOW INDEX (Enterprise with RBAC); without it, only the constraints' indexes (`SHOW CONSTRAINTS`), no counters, and the warning names the privilege | Yes (Community and Enterprise) | No size or foreign keys. **Drop index** generates `DROP INDEX` or `DROP CONSTRAINT`. |
| Memgraph | `SHOW INDEX INFO` and unique constraints | It does not count usage: no counters, with a warning | — | — | — | — | Yes | **Drop index** generates `DROP INDEX ON :Label(property)`. |
| Couchbase | `system:indexes` (GSI): `#primary` as primary key (on `meta().id`), keys, the `WHERE` condition as filter, partitioning in the type | Index service statistics from the cluster manager (`/pools/default/stats/range`, sampled every few seconds): `index_num_requests` to seeks and `index_disk_size` to size; the last read, from `last_known_scan_time` when the indexer published it. Its only write counter (`index_num_docs_indexed`) includes the documents of the initial build: it is not shown and the warning says so | The start of the index node that started last (its `uptime`) | No | No | External Stats Reader (or an administration role); without it, no counters and with a warning | Yes | No foreign keys or unique indexes; search (FTS) indexes are not listed. **Drop index** generates `DROP INDEX`. |
| Apache Cassandra, ScyllaDB | The primary key (partition, then clustering, with DESC) and `system_schema.indexes` (secondary, SAI, SASI, with their target: the column, `keys(…)`, `values(…)`, `full(…)`) | They do not count usage per index: no counters, with a warning | — | — | — | — | Yes | No foreign keys. **Drop index** generates `DROP INDEX`. |
| Amazon Keyspaces | Only the primary key | Same | — | — | — | — | No | It has no secondary indexes. |
| Azure Cosmos DB | The key (`id` with the partition key), the included paths of the indexing policy (range, with excluded ones as filter), unique keys, composite indexes and spatial, full-text and vector ones | There are no per-index counters (index metrics are per query): no counters, with a warning | — | — | — | — | No (unit tests only) | No size or foreign keys. **Drop index** is not possible: the indexing policy cannot be changed from a script (the sync warns about it); it is changed in the Azure portal or with the CLI. |
| CouchDB | In `_all_docs`: the `_id` index as primary key and Mango indexes (`GET _index`, json or text, DESC, `partial_filter_selector` as filter) | It does not count usage per index: no counters, with a warning | — | — | — | — | Yes | Size from `_design/…/_info` when the design document has only that index. **Drop index** generates `DELETE _index/<name>`, a driver extension that looks up the design document when run. |
| OrientDB | The class's indexes, from the database metadata (UNIQUE, NOTUNIQUE, FULLTEXT, hash, Lucene…) | It does not count usage per index: no counters, with a warning | — | — | — | — | Yes | Records are located by `@rid`, which is not an index: there is no primary key entry. LINK properties with a linked class are the foreign keys. **Drop index** generates `DROP INDEX`. |
| Amazon DynamoDB | The primary key (partition and sort, with the table's size), GSIs and LSIs with their projection (`INCLUDE` as included columns, `KEYS_ONLY` in the type) and their size | It does not count reads per index (the capacity consumed per GSI is in CloudWatch): no counters, with a warning | — | — | — | — | DynamoDB Local | No foreign keys. **Drop index** drops a GSI; an LSI is not dropped without recreating the table (the sync warns about it). |
| SQLite, libSQL / Turso | The same catalog read as **Compare schemas** (`pragma_index_list`, `pragma_index_xinfo`): the primary key (`ROWID` if it is an `INTEGER PRIMARY KEY`, `CLUSTERED` in a `WITHOUT ROWID` table, otherwise its automatic index), indexes with DESC, expressions and partial filter, and those of UNIQUE constraints with the name the sync uses | SQLite does not count index usage: no counters, with a warning | — | — | — | — | Yes (local file and libSQL server) | Size from `dbstat` (pages × page size) where the build has it. Foreign keys from `pragma_foreign_key_list`. **Drop index** generates `DROP INDEX` (a table UNIQUE is rebuilt). |
| DuckDB | `duckdb_constraints()` and `duckdb_indexes()`: the primary key, UNIQUEs and `CREATE INDEX`es, all ART | It neither counts index usage nor reports its size: no counters, with a warning (the plan shows `INDEX_SCAN` when one is used) | — | — | — | — | Yes (local file) | Foreign keys from `duckdb_constraints()`. **Drop index** generates `DROP INDEX`. |
| Firebird | `RDB$INDICES`, `RDB$INDEX_SEGMENTS`, `RDB$RELATION_CONSTRAINTS`: type `ASC`/`DESC`, `COMPUTED` for expression ones and `INACTIVE` for deactivated ones; filter of partial ones (Firebird 5) | It does not count usage per index (`MON$RECORD_STATS.MON$RECORD_IDX_READS` is per table): no counters, with a warning | — | — | — | — | Yes (Firebird 5) | No size (only `gstat` reports it). Named foreign keys. **Drop index** generates `DROP INDEX`. |
| SAP HANA | The **Compare schemas** read reduced to the table (`SYS.INDEXES`, `SYS.INDEX_COLUMNS`, `SYS.FULLTEXT_INDEXES`, `SYS.CONSTRAINTS`); type = `INDEX_TYPE` (CPBTREE, BTREE, INVERTED VALUE/HASH/INDIVIDUAL) or FULLTEXT | There are no per-index usage counters: no counters, with a warning | — | — | — | — | No (there is no container) | Size from `M_RS_INDEXES` (row store table indexes; in column store they live in the columns' dictionaries). Foreign keys from `SYS.REFERENTIAL_CONSTRAINTS`. **Drop index** generates `DROP INDEX` (or `DROP FULLTEXT INDEX`). |
| ClickHouse | The primary key (MergeTree's sparse index, type SPARSE, not unique), skip indexes (minmax, set, bloom_filter…, with their GRANULARITY) and projections | It does not count usage per index (how many granules each one discards comes from `EXPLAIN indexes = 1`, per query): no counters, with a warning | — | — | — | — | Yes | Size: `primary_key_bytes_in_memory` from `system.parts`, `data_compressed_bytes` from `system.data_skipping_indices` and `bytes_on_disk` from `system.projection_parts` (active parts). No foreign keys. **Drop index** generates `ALTER TABLE … DROP INDEX` (or `DROP PROJECTION`). |
| Timeplus Proton | Same (the same system tables; in an append-type stream the sort key is not a primary key and is not listed) | Same; the warning names Timeplus Proton | — | — | — | — | Yes (`dbine-test-proton`) | Same. |
| Apache Phoenix | `SYSTEM.CATALOG`: the row key (ROW KEY) and GLOBAL and LOCAL indexes, with DESC and covered columns (`INCLUDE`) | It does not count usage per index: no counters, with a warning | — | — | — | — | Yes | No size (it lives in HBase) or foreign keys. **Drop index** generates `DROP INDEX`, and the sync recreates it with its DESC. |
| Flight SQL | With DuckDB behind (GizmoSQL): `duckdb_constraints()` and `duckdb_indexes()` through SQL. With another engine: the primary key and foreign keys from `GetPrimaryKeys` and `GetImportedKeys`, if the server answers them | No counters, with a warning | — | — | — | — | GizmoSQL yes; other servers no | Flight SQL has no index commands: Dremio and DataFusion (InfluxDB 3) are left with the key, if they report it. It has no schema sync, so **Drop index** does not appear: it is dropped with `DROP INDEX` in a query. |
| Db2 (LUW, through ODBC) | `SQLStatistics`, `SQLPrimaryKeys` and `SQLForeignKeys` of the table, with the INCLUDE columns from `SYSCAT.INDEXCOLUSE` | `MON_GET_INDEX` (LEFT JOIN from `SYSCAT.INDEXES`: unused ones stay at zero): `INDEX_SCANS` to seeks, writes = `KEY_UPDATES` + `INCLUDE_COL_UPDATES` (inserts do not count), last read = `SYSCAT.INDEXES.LASTUSED` (date) | `DB_CONN_TIME` of `MON_GET_DATABASE` (database activation) | No | Yes | EXECUTE on `MON_GET_INDEX` (or SQLADM, DBADM, DATAACCESS); without that, no counters and the warning names the privilege | No (unit tests only) | No size. |
| Db2 for i (ODBC) | Same | `QSYS2.SYSINDEXSTAT`: `QUERY_USE_COUNT` to seeks and `LAST_QUERY_USE` | — | No | No | Read of `QSYS2.SYSINDEXSTAT`; if denied, no counters with a warning | No (unit tests only) | — |
| Sybase ASE (ODBC) | Same | `master..monOpenObjectActivity`: `UsedCount` to seeks, rows inserted + deleted + updated to writes, `LastUsedDate` | — (it counts while the object descriptor is open) | No | Yes | mon_role and the "enable monitoring" and "per object statistics active" options; without that, no counters and the warning says so | No (unit tests only) | — |
| Db2 for z/OS, Informix, GBase 8s, Teradata, SQL Anywhere, Altibase, CUBRID, Dameng, Mimer, Ingres, IRIS and Caché, OpenEdge, MonetDB, Virtuoso, MaxDB, Zen, NuoDB, Ocient, Ignite, Machbase, Access, dBase and generic ODBC | From the table's ODBC catalog | They do not expose per-index counters through SQL: no counters, with a warning | — | — | — | — | The generic preset yes (through the SQL Server ODBC driver); the others no | Candidates: Teradata with object DBQL (`DBC.DBQLObjTbl`, if enabled), Informix with `sysmaster:sysptprof` (per partition), Db2 for z/OS with `SYSINDEXSPACESTATS.LASTUSED` (the date only). **Drop index** with each engine's sync script. |
| Vertica, Exasol, Netezza (ODBC) | Without user-defined indexes (projections, automatic indexes, zone maps): the primary key as a constraint (CONSTRAINT) | — | — | — | — | — | No | Foreign keys (informational) mark their columns. |
| MySQL (and Aurora MySQL, Cloud SQL para MySQL) | `SHOW INDEX` (PK included, `col(n)` prefixes, expressions, DESC); foreign keys from `KEY_COLUMN_USAGE` | `performance_schema.table_io_waits_summary_by_index_usage`: `COUNT_FETCH` (rows read through the index) to seeks; rows read without an index (table scans) to the primary key's scans in InnoDB, where the table is its clustered index; writes = the table's (`COUNT_INSERT + COUNT_UPDATE + COUNT_DELETE`: inserts are not attributed to any index) | Server start (`Uptime`); the warning clarifies that it holds unless the statistics were reset or enabled later | No | Yes (the table's) | SELECT on performance_schema; with `performance_schema = OFF`, the `wait/io/table/sql/handler` instrument off or without the permission, no counters and the warning says why | Yes (8.4) | "Unused" is equivalent to `sys.schema_unused_indexes`. Size from `mysql.innodb_index_stats` (pages × `innodb_page_size`, partitions added up, according to the last ANALYZE) if the user can read it. A TRUNCATE of the performance_schema table resets the counters. |
| MariaDB | Same | With `userstat = 1`: `information_schema.INDEX_STATISTICS` (`ROWS_READ` to seeks) and `TABLE_STATISTICS` (`ROWS_CHANGED` to writes; its `ROWS_READ` minus what was read through indexes, to the primary key's scans in InnoDB). Otherwise, performance_schema, like MySQL | Same | No | Yes (the table's) | The `userstat` views in information_schema; performance_schema like MySQL | Yes (11.8) | performance_schema comes off by default: without `userstat` or performance_schema the warning says how to enable them (`SET GLOBAL userstat = 1` does not require a restart). `FLUSH INDEX_STATISTICS` resets the counters. |
| TiDB | `SHOW INDEX` | 8.0+: `CLUSTER_TIDB_INDEX_USAGE` (summed across instances; `TIDB_INDEX_USAGE` if it fails): queries that read less than 10 % of the table's rows to seeks, the rest to scans; `LAST_ACCESS_TIME` to the last read; writes = `mysql.stats_meta.modify_count` (rows modified since the last ANALYZE) | Instance start (`Uptime`) | Yes | Yes (without SELECT on `mysql.stats_meta`, no, and the warning says so) | SELECT on `mysql.stats_meta` for writes | Yes (7.5 and 8.5) | The clustered primary key is the row identifier: TiDB does not count it and it stays at zero (never "unused"). It only counts on tables with statistics. Before 8.0, no counters with a warning. It does not keep `DESC`. No size. |
| OceanBase (MySQL mode) | `SHOW INDEX` | `oceanbase.DBA_INDEX_USAGE` (4.x; joined with `DBA_OBJECTS` by the internal name `__idx_<table id>_<index>`): `TOTAL_ACCESS_COUNT` to seeks, `LAST_USED` to the last read; writes from `DBA_TAB_MODIFICATIONS` (since the last statistics) | — (counters persist across restarts) | No | Yes (if `DBA_TAB_MODIFICATIONS` is denied, no) | SELECT on the `oceanbase` database; with `_iut_enable` off or without the permission, a warning | Yes (4.4.2) | It counts by sampling except with `_iut_stat_collection_type = 'ALL'` and flushes to the view in the background: in 4.4.2 no access appeared in 40 minutes, so the name join is verified against the view's definition, not with data. `DBA_TAB_MODIFICATIONS` arrives late and in 4.4 does not count UPDATEs. The primary key is not counted: it stays at zero, never "unused". |
| SingleStore, StarRocks, Apache Doris, VeloDB, Databend, GreptimeDB | `SHOW INDEX` (Databend: `system.indexes`); GreptimeDB lists its PRIMARY and TIME INDEX | They do not count index usage: no counters, with a warning | — | — | — | — | GreptimeDB yes; the rest no | In StarRocks and Doris the sort key is not an index: bitmap, N-gram and inverted ones are listed. |

### Engines without index usage

The **Indexes** folder and tab do not appear (`supports_index_usage` is
false) where there are no indexes to show:

- **Fabric Warehouse**: it has no indexes.
- **Denodo**: it has no indexes.
- **Amazon Neptune**: it has no user-defined indexes.
- **Elasticsearch, OpenSearch**: an Elasticsearch index is the table; each
  field is indexed on its own (inverted index, doc values, points) and there
  are no secondary indexes to list or drop. Elasticsearch 7.15+ counts
  accesses per field (`_field_usage_stats`), a candidate for later;
  OpenSearch does not have it.
- **Redis (Valkey, Dragonfly), etcd, ksqlDB**: they have no secondary indexes
  per table.
- **Manticore Search**: the table is the index.
- **Hive, Impala, Spark, Kyuubi, Cloudera, SQream, HeavyDB, NetSuite (ODBC)**:
  no indexes or foreign keys in their catalog.
- **DuckDB, files preset**: it queries files, which have no indexes.
- **Apache Calcite Avatica (generic preset)**: its protocol has no index
  metadata.
- **Trino, Presto, Starburst and Amazon Athena**: no indexes or keys in their
  catalog; each connector's are not exposed.
- **Apache Drill**: no indexes or keys in its catalog.
- **InfluxDB**: it indexes all tags by itself, with no per-table indexes or
  counters.
- **Solr**: like Elasticsearch, the collection is the index; there are no
  secondary indexes.
- **Apache IoTDB**: time series by path, no secondary indexes.
- **TDengine**: pending. TDengine 3 has indexes on the tags of a supertable
  (without usage counters), which are not listed yet.

### Tested against real servers

`index_usage_live` (SQL Server 2022, `dbine-test-sqlserver`): a table with a
primary key and two indexes; five seeks on one give 5 seeks, the other stays
at zero reads with writes (unused), and `since` comes with the start time.
`index_usage_babelfish_live` (`dbine-test-babelfish`): lists the indexes and
warns that there are no counters.

`crates/drivers/mysql/tests/index_usage.rs` (`dbine-test-mysql` 8.4,
`dbine-test-mariadb` 11.8, `dbine-test-tidb` 7.5, `dbine-test-tidb8` 8.5,
`dbine-test-oceanbase` 4.4.2, `dbine-test-greptimedb`): a table with a
primary key, a foreign key and two indexes; five lookups on one give 5 seeks,
the other stays without reads with writes (unused), a table scan adds scans
to the primary key (MySQL, MariaDB), the foreign key points to its table and
the unused index is dropped with the **Compare schemas** script
(`ALTER TABLE … DROP INDEX`). MySQL and MariaDB with counters: the warning
says they hold since startup, barring a later reset or activation. MySQL with
a user without SELECT on performance_schema: indexes and foreign keys
without counters, with the permission warning. MariaDB without `userstat` or
performance_schema: the warning on how to enable them; with `userstat`, the
counters. TiDB 7.5: no counters, with the version warning; 8.5: seeks with
health color, last read and counted writes; a user without SELECT on
`mysql.stats_meta` sees the reads without writes (nothing "unused") and the
warning that names the permission. OceanBase: indexes, foreign key, readable
counters (empty, see the table) and the drop. GreptimeDB: PRIMARY and TIME
INDEX without counters.

Oracle (`crates/drivers/oracle/tests/index_usage.rs`, Oracle 23ai Free,
`dbine-test-oracle`): `index_usage_live` creates a table with a primary key,
foreign key and two indexes (one with DESC and a function), does five lookups
on one and waits for the `DBA_INDEX_USAGE` flush: the used index gives 5
accesses with its last use, the other stays at zero with writes (unused), the
warning talks about the flush and the sampling, and **Drop index…** (the sync
script without that index, `DROP INDEX`) drops it.
`index_usage_without_privileges_live`: a user without SELECT_CATALOG_ROLE
sees the indexes, their size and the foreign keys without counters and with
the privilege warning; with MONITORING USAGE on all indexes they see which
one was used. A reader from another schema, with only SELECT on that table,
sees the indexes and foreign keys without size (unknown, not 0 KB).

`crates/drivers/postgres/tests/index_usage.rs` (`dbine-test-postgres`,
`-timescale`, `-yugabyte`, `-cockroach`): a table with a primary key, a
foreign key and three indexes (one partial); six lookups on one give 6 seeks,
the others stay without reads. In PostgreSQL and TimescaleDB the ordinary
index without reads comes out "unused" with the table's writes and the
partial one is left without writes (never "unused") with the partial index
warning; in YugabyteDB and CockroachDB, which do not count writes, they stay
at 0 %. In CockroachDB 26.x the indexes show as BTREE. The foreign key points
to its table and the partial index is dropped with the **Compare schemas**
script (`DROP INDEX`). In a TimescaleDB hypertable the index adds up its
chunks' scans. Against openGauss and Cloudberry, indexes and keys are listed
with counters; against Materialize, RisingWave, CrateDB and H2, without
counters. In `crates/drivers/dsql/tests/index_usage.rs` (DSQL against
`dbine-test-dsqlpg`) the indexes are listed without counters and one is
dropped with the sync script.

Spanner's `index_usage_live` (emulator, `dbine-test-spanner`): a table with a
primary key, a foreign key and two indexes (one NULL_FILTERED, the other with
`STORING`); it lists the primary key and the two indexes without the
foreign key's, with columns, `DESC` and stored ones, the foreign key, and the
warning that names the permission (the emulator has no `SPANNER_SYS`); it
drops an index with the sync script (`DROP INDEX`).

Dremio's `index_usage_live` (OSS, `dbine-test-dremio`): two reflections on a
`$scratch` table; the queries one accelerates give its reads (100 %), the
other stays at 0 % without writes; the sync drops it (`DROP REFLECTION`) and
creates it again.

BigQuery, Snowflake and Databricks only have unit tests (cloud services).
Still untested against a real service: the columns of `SHOW INDEXES` for
Snowflake hybrid tables and the `SYS_INDEX_*_PRIMARY` naming convention,
BigQuery's `JOBS` and `JOBS_BY_USER` error paths (and the filter by
`index_unused_reasons.base_table`, which if it fails falls back to the
simple count), the Unity Catalog `information_schema` joins in Databricks and
the read of `SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR` in a real Spanner (the
emulator does not have it).

Document and graph databases (`crates/drivers/<engine>/tests/index_usage.rs`):

- `mongodb` (`dbine-test-mongodb`): a collection with two indexes; five
  lookups on one give 5 reads, the other stays at 0 %, without writes
  (MongoDB does not count them per index) and with the warning that explains
  it, with size and `since`. In FerretDB (`dbine-test-ferretdb`) they are
  listed without counters.
- `neo4j` (`dbine-test-neo4j`): a constraint and two indexes; five lookups on
  one give 5 reads and its last read, the other stays at 0 %. Against Neo4j
  Enterprise (`dbine-test-neo4j-ee`), a user without SHOW INDEX sees the
  constraints' indexes with the privilege warning. In Memgraph
  (`dbine-test-memgraph`) they are listed without counters.
- `couchbase` (`dbine-test-couchbase`): `#primary` and two indexes; five
  queries on one give 5 reads, the other stays at 0 % with its size, without
  writes and with the initial build warning (it does not come out "unused").
- `cassandra` (`dbine-test-scylladb` and `dbine-test-cassandra`), `couchdb`,
  `orientdb` (with its LINK as foreign key) and `dynamodb` (DynamoDB Local):
  they list the key and the indexes without counters.

In all of them, **Drop index…** (the sync script without that index) drops
it. Cosmos DB only has unit tests.

Embedded and analytical (`crates/drivers/<engine>/tests/index_usage.rs`): a
table with a primary key, a foreign key and two indexes, five lookups on one.

- `sqlite` and `duckdb` (local files, without `#[ignore]`), `libsql`
  (`dbine-test-libsql`), `firebird` (`dbine-test-firebird`, Firebird 5) and
  `flightsql` (GizmoSQL, `dbine-test-flightsql`): they list the key, the two
  indexes and the foreign key, without counters and with the warning; SQLite
  and libSQL with the size from `dbstat`.
- `clickhouse` (`dbine-test-clickhouse`): the sparse key and two skip indexes
  with their size, without foreign keys, and the warning that names
  ClickHouse. `timeplus_index_usage_live` (`dbine-test-proton`): a stream with
  a minmax index lists it without counters, with the warning that names
  Timeplus Proton.
- `phoenix` (`dbine-test-phoenix`): the row key, two global indexes (one with
  its covered column) and a local one with DESC, which the sync drops and
  recreates identically.

In all but Flight SQL (which has no sync and drops it with `DROP INDEX`),
**Drop index…** drops it with the sync script. SAP HANA and the ODBC variants
with counters (Db2, Db2 for i, ASE) only have unit tests (there are no
containers); `crates/drivers/odbc/tests/index_usage.rs` tests the generic
preset through a real ODBC driver.

## Dependencies

Right click on a table, a view, a routine or a column › **View
dependencies…** opens a tab with what depends on that object: foreign keys,
indexes and primary keys, CHECK constraints, and views, routines and
triggers whose code uses it, with the lines where it appears. Each result
says how certain it is: **Confirmed** (the engine's catalog records it),
**Probable** (the code names the object; for a column, together with its
table) or **Review** (the name only appears inside a text, as in dynamic
SQL). Comments do not count, nor does a name qualified with another schema,
nor an alias. External applications and reports are not seen.

The contract is in `crates/dbine-driver/src/dependencies.rs`
(`Driver::supports_dependencies` and `Session::dependents`). The default
version works for all engines: it takes the foreign keys, indexes and checks
from `database_schema` and reads one by one the definitions of views,
routines, triggers, packages, synonyms, aliases, streams, tasks and sinks
(`CODE_KINDS`). Drivers that have a dependency registry use it instead:

- **SQL Server and Azure SQL Database**: a single query on
  `sys.sql_modules` brings only the bodies that name the object or that
  `sys.sql_expression_dependencies` records as users. Columns are only
  confirmed in objects with SCHEMABINDING (the catalog does not record
  columns in the others). Encrypted modules are listed as unreadable.
  **Babelfish** and **Fabric Warehouse** use the default version if their
  catalog does not have that view.
- **The other engines**: the default version. In a database with thousands
  of routines it is slow, because it reads each definition separately. The
  versions with the PostgreSQL catalog (`pg_depend`), Oracle
  (`ALL_DEPENDENCIES`) and MySQL (`VIEW_TABLE_USAGE`) are pending.

Drivers that run in their own process (those downloaded on demand) answer
`Dependents` through the protocol; a process published earlier answers that
it does not know it and the app runs the default version through its other
calls.

### Engines without dependencies

The option does not appear (`supports_dependencies` is false) where there are
no foreign keys or objects with code that could depend on others:

- **Redis, Valkey, Dragonfly, etcd**: keys only.
- **Amazon DynamoDB, Azure Cosmos DB, Apache Solr, Manticore Search**: tables,
  collections and indexes without views, routines or foreign keys (Cosmos DB's
  procedures are JavaScript that the driver does not list).
- **Amazon Keyspaces**: no materialized views or functions (Cassandra and
  ScyllaDB do have them and go through the default version).
- **Amazon Neptune**: labels and relationships without saved queries.
- **InfluxDB 1 (InfluxQL), InfluxDB 2 (Flux), InfluxDB 3 (SQL), Apache IoTDB,
  TimechoDB**: measurements and series without objects that depend on others
  (InfluxDB's continuous queries and tasks are not listed).

### Tested against real servers

`crates/drivers/sqlserver/tests/dependencies.rs` (SQL Server 2022,
`dbine-test-sqlserver`): a table with a primary key, an index and a CHECK on
a column, another table with a foreign key to it, a view with SCHEMABINDING,
one without it, a procedure that uses the column, one with dynamic SQL and
one that uses a column of the same name from another table. For the table:
the foreign key, the views and the procedure come out confirmed, the dynamic
SQL for review, and the other table's procedure does not appear. For the
column: the view with SCHEMABINDING is confirmed, the procedure as probable
(with the line `SELECT Pepe`), the index and the CHECK confirmed, and neither
the view that does not use it nor the other table's procedure appear. The
default version, run on the same database, finds the same code.

## Disable and enable indexes

Right click on an index (in the explorer or in the **Indexes** tab) ›
**Disable index…**; if it is already disabled, **Enable index…**. The dialog
shows the engine's statement and what is worth knowing beforehand, and runs
as a task (like **Drop index…**). A disabled index stays defined but the
optimizer does not use it: the explorer and the tab mark it **disabled** and
never as "unused". The contract is in `crates/dbine-driver/src/lib.rs`
(`Driver::supports_index_toggle` and `Driver::index_toggle_script`) and the
state in `IndexUsage::disabled`. It is not part of the structure
(`IndexDef`): **Compare schemas** sees no difference between an enabled and
a disabled index.

| Engine | Disable | Enable | Notes |
|---|---|---|---|
| SQL Server, Azure SQL Database | `ALTER INDEX … DISABLE` | `ALTER INDEX … REBUILD` | It stops being maintained and frees its space; enabling it rebuilds it in full. The clustered index leaves the table inaccessible, and the primary key's or a UNIQUE disables the foreign keys that point to it (they do not come back by themselves). |
| MySQL 8, Aurora MySQL, Cloud SQL para MySQL, TiDB, OceanBase (MySQL) | `ALTER TABLE … ALTER INDEX … INVISIBLE` | `… VISIBLE` | It is still maintained. The primary key (the implicit one too) cannot. OceanBase untested against a server. MySQL 5.7 rejects the statement. |
| MariaDB | `… IGNORED` | `… NOT IGNORED` | Since 10.6. A MariaDB database connected as "MySQL" receives MySQL's syntax and rejects it. |
| Oracle, Oracle Autonomous Database | `ALTER INDEX … INVISIBLE` | `ALTER INDEX … VISIBLE` | Not UNUSABLE: it stays maintained and still guarantees uniqueness (the primary key's too). An UNUSABLE index is marked disabled and enabling it rebuilds it, by partitions if needed. Those of IOT and cluster tables cannot. |
| Firebird | `ALTER INDEX … INACTIVE` | `ALTER INDEX … ACTIVE` | Activating it rebuilds it. Constraint indexes (PRIMARY KEY, FOREIGN KEY, UNIQUE) cannot. |
| CockroachDB | `ALTER INDEX t@ix NOT VISIBLE` | `… VISIBLE` | Since 22.2. The primary key cannot. |
| MongoDB | `hideIndex` | `unhideIndex` | Since 4.4 (`collMod`). The `_id_` index cannot. |
| IBM Informix, GBase 8s (ODBC) | `SET INDEXES … DISABLED` | `… ENABLED` | Untested against a server. It is not maintained while disabled; enabling it rebuilds it. |
| SAP MaxDB (ODBC) | `ALTER INDEX … DISABLE` | `… ENABLE` | Untested against a server. |

### Engines without disabling indexes

The option does not appear (`supports_index_toggle` is false) where the
engine has no native way to turn an index off without dropping it:

- **PostgreSQL and its family** (PostgreSQL, TimescaleDB, YugabyteDB,
  KingbaseES, AlloyDB para PostgreSQL, Amazon Aurora PostgreSQL, Cloud SQL para PostgreSQL, EDB Postgres Advanced Server, Fujitsu Enterprise Postgres,
  openGauss, Greenplum, Apache Cloudberry, Greengage DB, Amazon Redshift,
  Amazon Aurora DSQL, H2 (servidor PostgreSQL), Materialize, RisingWave,
  Yellowbrick): there is no supported way; marking the index as invalid by
  touching `pg_index` by hand is not reasonable to offer. **Babelfish for
  PostgreSQL** does not accept `ALTER INDEX … DISABLE` either.
- **SQLite, libSQL / Turso, DuckDB, Archivos dBase (DBF), Microsoft Access**:
  no index state; they are only created and dropped.
- **IBM Db2 (LUW), IBM Db2 for i (AS/400), IBM Db2 for z/OS, SAP ASE
  (Sybase), SAP SQL Anywhere, SAP HANA, Teradata, Actian Ingres, Actian Zen
  (Pervasive PSQL), Mimer SQL, CUBRID, Altibase, InterSystems IRIS,
  InterSystems Caché, Progress OpenEdge, NuoDB, MonetDB, OpenLink Virtuoso,
  Machbase, Ocient, Exasol, IBM Netezza, Vertica, ODBC (genérico)**: no
  statement to disable an index (or no user indexes). CUBRID 10 might have
  `INVISIBLE`: pending verification.
- **Dameng (DM)**: pending; it probably accepts `ALTER INDEX … INVISIBLE`,
  with neither the syntax nor where the state is read verified.
- **SingleStore, StarRocks, Apache Doris, VeloDB, Databend, GreptimeDB,
  ClickHouse, Timeplus Proton, Apache Phoenix, CrateDB, Apache Ignite 2,
  Apache Ignite 3, Arrow Flight SQL, Dremio, Snowflake, Google BigQuery,
  Google Cloud Spanner, Databricks SQL, Azure Databricks**: indexes (or sort
  keys, skip indexes) are only created and dropped.
- **FerretDB**: rejects `hidden` (tested against `dbine-test-ferretdb`).
  **Amazon DocumentDB**: it has no hidden indexes (according to AWS
  documentation, untested).
- **Apache Cassandra, ScyllaDB, Amazon Keyspaces, Couchbase, CouchDB, Azure
  Cosmos DB, Amazon DynamoDB, Neo4j, Memgraph, OrientDB**: no way to turn a
  secondary index off without dropping it.

### Tested against real servers

Each test creates a table with an index, disables it with the driver's
script, checks that `index_usage` marks it disabled and that the table can
still be queried, enables it and checks that it comes back:

- `crates/drivers/sqlserver/tests/index_toggle.rs` (SQL Server 2022,
  `dbine-test-sqlserver`).
- `crates/drivers/mysql/tests/index_toggle.rs` (MySQL 8.4, MariaDB 11.8,
  TiDB 7.5 and 8.5): in addition, the primary key is rejected.
- `crates/drivers/oracle/tests/index_toggle.rs` (Oracle 23ai Free): in
  addition, the invisible primary key index still rejects duplicates
  (ORA-00001), an UNUSABLE index is rebuilt on enabling (by partitions too)
  and that of an IOT table is rejected.
- `crates/drivers/firebird/tests/index_toggle.rs` (Firebird 5): the primary
  key is rejected and a named UNIQUE constraint is rejected by the server.
- `crates/drivers/postgres/tests/index_toggle.rs` (CockroachDB 26.3): the
  primary key is rejected.
- `crates/drivers/mongodb/tests/index_toggle.rs` (MongoDB 7 and FerretDB 2):
  `_id_` is rejected; FerretDB does not offer the option and the server
  rejects `hideIndex`.

## Run on several databases

In the query editor, **Run on several databases…** runs the code (or the
selection) on several databases of the same connection and gathers the
results. The dialog lists the connection's databases with a filter that
accepts `*` (for example `*tenant-n*`), **All** / **None** over what the
filter shows, and remembers the last choice per connection; the first time,
the tab's database comes selected. Each database runs in its own session
(never the explorer's), 4 at a time, split like the engine's normal run
(statement by statement, batch by batch or whole) and with the same maximum
rows per database. It runs as a task: it continues in the background and is
cancelled from the dialog or the **Tasks** panel (no further database
starts and those running are interrupted, like the editor's Cancel).

- **Safety**: a read-only connection still rejects anything that is not a
  read. If the code is not read-only (the same classification as the
  read-only guard), or the engine is not SQL and it cannot be verified, it
  asks for confirmation saying how many databases it will run on. It never
  runs by itself.
- **Results**: if the first result of each database that finished well has
  the same columns (case-insensitive), a single grid is shown with a first
  column `database`; it is exported and copied like any grid (the loaded
  rows). Otherwise, one result tab per database. **Messages** has the
  summary per database: rows and time, or the error.
- The command is `run_multi_db` / `cancel_multi_db`
  (`src-tauri/src/commands/multi_db.rs`) and uses only `Session::execute`:
  it works in all engines with several databases.

### Engines without running on several databases

The action does not appear where the engine has a single database
(`DriverInfo::databases_label` empty: the explorer shows the objects
directly under the connection). Reason in all of them: a single database.

- **Files**: SQLite, libSQL / Turso, Archivos CSV / Parquet / JSON, Archivos dBase (DBF), Microsoft Access.
- **SQL**: Firebird, Amazon Aurora DSQL, H2 (servidor PostgreSQL), CrateDB,
  Apache Phoenix, Apache Ignite 2, Apache Ignite 3, Apache Calcite Avatica,
  HeavyDB (OmniSciDB), Manticore Search, Dameng (DM).
- **ODBC**: IBM Db2 (LUW), IBM Db2 for i (AS/400), IBM Db2 for z/OS,
  Teradata, Vertica, Exasol, SAP MaxDB, SAP SQL Anywhere, Actian Ingres,
  Actian Zen (Pervasive PSQL), Altibase, CUBRID, InterSystems IRIS,
  InterSystems Caché, Mimer SQL, MonetDB, NuoDB, Ocient, Machbase, Progress
  OpenEdge, SQream DB, Apache Hive, Apache Impala, Apache Kyuubi, Spark
  Thrift Server, Cloudera CDP (Hive por HTTP), Oracle NetSuite
  (SuiteAnalytics Connect).
- **NoSQL and search**: Amazon DynamoDB, Amazon Neptune, Elasticsearch,
  OpenSearch, Open Distro for Elasticsearch, Apache Solr, etcd, ksqlDB.

### Tested against real servers

SQL Server (`dbine-test-sqlserver`) and PostgreSQL (`dbine-test-postgres`):
three databases with the same table are gathered into one grid with
`database`; with a fourth with different columns, one result per database
remains; a database without the table reports its error and the others are
still gathered; on a read-only connection a `DELETE` is rejected (tested on
SQL Server)
(`cargo test -p dbine --lib multi_db -- --include-ignored`).

## Integrated authentication (Windows / Kerberos)

Sign in with the domain account instead of a database user
([`integrated-authentication.md`](integrated-authentication.md)).

| Engine | What there is |
|---|---|
| SQL Server | **Windows: current user**: SSPI on Windows (NTLM), Kerberos with the session's ticket on macOS and Linux. **Windows: domain username and password** (NTLMv2): on Windows, macOS and Linux. |
| MongoDB | **Kerberos (GSSAPI)** with the session's ticket (SSPI on Windows). Requires MongoDB Enterprise. |
| Engines through ODBC (Db2, Teradata, Hive, Impala, Spark, Vertica…) and generic ODBC | With the ODBC driver's attributes in **Additional attributes** (`Authentication=KERBEROS`, `AuthMech=1`, `Trusted_Connection=yes`…), which replace those of the same name. |

### Engines without integrated authentication

| Engine | Reason |
|---|---|
| SQL Server from macOS and Linux, with domain username and password | Pending. The client (tiberius) does NTLM outside Windows with `sspi-rs`, whose version pins a preliminary version of `crypto-bigint` incompatible with the SSH client's (russh). It is solved with a patch to DBine's copy of tiberius (`vendor/tiberius`) that uses its own NTLMv2 client, in Rust, on all platforms. In the meantime: **Windows: current user** with `kinit`. |
| Azure SQL Database, Microsoft Fabric | They have no Windows logins (local Active Directory): they use Microsoft Entra ID, including its integrated method (see [`integrated-authentication.md`](integrated-authentication.md#microsoft-entra-id-sql-server-azure-sql-database-microsoft-fabric)). |
| Babelfish for PostgreSQL | It has no Windows logins. |
| Oracle, Oracle Autonomous | The client is Oracle's thin client in Rust (`oracledb`), which only does the password login (O5LOGON). External authentication (`/`, wallet with credentials, Kerberos, operating system user) belongs to the client with Instant Client, which DBine does not use. The operating system user over the network (`REMOTE_OS_AUTHENT`) no longer exists since Oracle 21c anyway. |
| PostgreSQL and compatibles (TimescaleDB, AlloyDB, Cloud SQL, Aurora, EDB, YugabyteDB, CockroachDB, Greenplum…), Amazon Aurora DSQL | The client (tokio-postgres) rejects the server's GSSAPI and SSPI methods; there is no way to add them without rewriting its login. |
| MySQL, MariaDB, TiDB and compatibles | The client (mysql_async) does not have the `authentication_kerberos_client`, `authentication_windows_client` (MySQL Enterprise) or `auth_gssapi_client` (MariaDB) plugins. |
| SAP HANA | The client (hdbconnect) only does the username and password login. |
| Firebird | The Rust client only does SRP; Windows integrated security (`Win_Sspi`) belongs to the native fbclient library. |
| Cassandra, ScyllaDB | The Kerberos authenticator belongs to DataStax Enterprise; the client (scylla) does not bring it. |
| Neo4j | Bolt's `kerberos` scheme needs a server plugin and a ticket that DBine does not obtain yet. |
| ClickHouse, Trino, Elasticsearch, OpenSearch, Solr, Apache Phoenix, Apache Drill, Dremio, CouchDB, InfluxDB, ksqlDB | The server supports Kerberos over HTTP (SPNEGO) in some cases, but DBine's HTTP client does not negotiate SPNEGO. Pending. |
| Redis, etcd, Couchbase, OrientDB, Apache IoTDB, TDengine | The engine has no Windows or Kerberos authentication. |
| Arrow Flight SQL | The protocol only defines username and password or a token. |
| BigQuery, Spanner, Snowflake, Databricks, Athena, DynamoDB, Cosmos DB and other cloud services | They use the cloud identity (service accounts, IAM, tokens), not the domain's. |
| SQLite, DuckDB, libSQL and other local-file engines | There is no server to authenticate to. |

### Tested

- Unit: each mode builds the correct tiberius authentication (SQL, current
  user, NTLM), which fields the form shows in each mode, that Azure SQL,
  Fabric and Babelfish do not offer Windows, the Kerberos messages, MongoDB's
  GSSAPI login and the ODBC attributes that replace the preset's.
- Against real servers: the SQL Server username login stays the same
  (`dbine-test-sqlserver`) and so does MongoDB's with username and password
  (`dbine-test-mongodb`).
- **Not tested end to end**: there is no test Active Directory domain, so the
  current user (SSPI and Kerberos), NTLM and MongoDB's Kerberos were not
  tested against a real server.

## Processes

The Monitor's process list ([`processes.md`](processes.md)). Each engine
fills it with what it reports; there are three separate capabilities:
listing, cancelling another session's query (the session stays open) and
ending the session (its transaction is rolled back).

**List processes:** PostgreSQL and the engines that keep `pg_stat_activity`
(TimescaleDB, AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu,
KingbaseES, openGauss, Greenplum, Cloudberry, Greengage, YugabyteDB),
CockroachDB, Redshift, Yellowbrick, Materialize, RisingWave, CrateDB, H2,
Denodo, Aurora DSQL; MySQL, MariaDB, Aurora MySQL, TiDB, OceanBase,
SingleStore, StarRocks, Doris, VeloDB, Databend, Manticore, GreptimeDB; SQL
Server, Azure SQL, Fabric and Babelfish; Oracle, SAP HANA, Firebird;
MongoDB, FerretDB, DocumentDB, Elasticsearch, OpenSearch, Couchbase,
CouchDB, Cassandra, ScyllaDB, OrientDB; Redis, Valkey, Dragonfly, Neo4j,
Memgraph, Amazon Neptune, InfluxDB 1 and 3, IoTDB, TDengine, ksqlDB;
ClickHouse, Trino, Presto, Starburst, Drill, Dremio, Databricks, Snowflake,
BigQuery, Athena and Spanner; and, through ODBC, Db2 LUW, Db2 for i, Sybase
ASE, SQL Anywhere, Teradata, Vertica, Exasol, Netezza, Dameng and Altibase.

**Cancel another session's query:** all of the above except those in the
table below (SQL Server and its family, Cassandra and ScyllaDB, InfluxDB 3,
Dragonfly, Denodo, FerretDB, Sybase ASE, SQL Anywhere, Teradata, Netezza and
Altibase). Each engine uses its native form: `pg_cancel_backend`, `KILL
QUERY`, `ALTER SYSTEM CANCEL SQL`, `killOp`, `CLIENT UNBLOCK`,
`TERMINATE TRANSACTION`, `WLM_CANCEL_ACTIVITY`, `INTERRUPT_STATEMENT`, etc.
In Neo4j, Memgraph, ksqlDB and CouchDB "cancel" has a different scope (it
rolls back the transaction, pauses the persistent query, only stops
transient replications): it is in [`processes.md`](processes.md#by-engine).

**End the session:** those that appear in [Locks](#locks) with "Terminating
sessions", SQL Server and its family (also Fabric and Babelfish), Netezza,
Redis, Valkey and Dragonfly (`CLIENT KILL`), Firebird, OrientDB and
Snowflake (the chosen query's session). TDengine, ClickHouse, Trino, Presto,
Starburst, Drill, Dremio, Databricks, BigQuery, Athena, Spanner, Couchbase,
Elasticsearch, OpenSearch, CouchDB, InfluxDB, IoTDB, ksqlDB, Memgraph,
Neptune, Db2 for i, Teradata, Altibase, Manticore and GreptimeDB do not end
sessions (or have no sessions to end).

**Tests:** there are tests against a real server (each driver's
`tests/processes.rs`, run with the `DBINE_TEST_<ENGINE>_URL` variable and
skipped without it) for each engine's own drivers. The ODBC variants (Db2
LUW, Db2 for i, Sybase ASE, SQL Anywhere, Teradata, Vertica, Exasol,
Netezza, Dameng and Altibase) have no test of this kind: they were written
following the vendor's documentation and are verified with simulated
responses.

| Engine | What is missing | Reason |
|---|---|---|
| SQL Server, Azure SQL, Fabric | Cancel the query | `KILL` is the only thing that exists: it closes the session and rolls back its transaction; there is no way to stop the statement and keep the session. |
| Babelfish | Cancel the query | Babelfish does not allow stopping another session's query from T-SQL: only terminating it (`KILL`), which closes the session. |
| Fabric | Statement text | The warehouse views cannot be joined with `dm_exec_sql_text`. |
| Cassandra, ScyllaDB | Cancel and end | CQL cannot stop another client's request or close its connection. The list is the coordinator node's (local virtual tables); Cassandra 4.0 or later (`system_views`), and running queries since 4.1. ScyllaDB only lists connections. |
| Amazon Keyspaces | Everything | Amazon Keyspaces does not expose its connections or running queries. |
| InfluxDB 3 | Cancel | It has no `KILL QUERY` or an API to stop another client's query. |
| InfluxDB 2 (Flux) | Everything | No API lists or stops other clients' queries. |
| Dragonfly | Cancel | It has no `CLIENT UNBLOCK`: another client's blocking command cannot be interrupted without closing its connection. It does not report command or user either. |
| Denodo | Cancel and end | VQL has no way to cancel a query: they are cancelled from Diagnostic & Monitoring Tool or through JMX. |
| FerretDB | Cancel and end | It lists its sessions (PostgreSQL backends) with little detail and has no `killOp`. |
| Amazon Neptune | End | It only has running queries (`/openCypher/status`) and cancels them with `cancelQuery`; there are no sessions. |
| Sybase ASE | Cancel | ASE only ends a whole session (`KILL`). The batch text comes from the MDA tables (`monProcessSQLText`) and only if monitoring is on. |
| SQL Anywhere | Cancel | There is no cancellation of another connection's request, only `DROP CONNECTION`. |
| Teradata | Cancel, end and text | Aborting a request needs the PM/API host id, which SQL does not give. The text requires a `MonitorSQLText` call per session, which is why it is not shown. |
| Netezza | Cancel | A session can only be ended (`DROP SESSION`). |
| Altibase | Cancel | There is no cancellation through SQL. **Pending**: ending sessions (not implemented; the statement Altibase offers still has to be confirmed). |
| Db2 for i | End | **Pending**: it cancels with `QSYS2.CANCEL_SQL`, but ending the job is not implemented (its lock views differ from Db2 LUW and could not be validated). |
| Manticore, GreptimeDB | End | They only report running queries: `KILL` ends the query, there is no session to close. |
| Materialize, RisingWave, CrateDB | End | They cancel in their native way; ending sessions is not in these engines' lock views (see [Locks](#locks)). |
| TDengine | End | Its connections are taosAdapter's shared pool: closing one would cut other clients. |
| Spanner (emulator) | Everything | The emulator implements neither `SPANNER_SYS` nor `cancel_query`: there are no running queries to list. Real Spanner does. |
| Spanner, ClickHouse, Trino, Presto, Starburst, Drill, Dremio, Databricks, BigQuery, Athena, Couchbase, Elasticsearch, OpenSearch | End | Their protocol is HTTP/REST with no sessions the operator can close (Spanner's are a client pool): they list running queries or tasks, which can be cancelled. |
| Solr | Everything | It keeps no list of queries or sessions: `/tasks/list` only sees, core by core, queries sent with `canCancel=true`. |
| Azure Cosmos DB, DynamoDB | Everything | They are stateless HTTP services: they expose neither sessions nor running queries. |
| etcd | Everything | It has no server sessions or a view of running requests: each request is independent (clients' "sessions" are leases) and another client's cannot be cancelled. |
| libSQL / Turso | Everything | Each Hrana request is independent and the server has no view or API to list or cancel them. |
| SQLite, DuckDB | Everything | They are embedded databases: there is no server with other clients' sessions to list or foreign queries to cancel. |
| Flight SQL | Everything | The protocol does not define how to list or cancel other clients' queries: it depends on the server. For Dremio or Apache Doris, the dedicated driver does list them. |
| Phoenix | Everything | Neither Phoenix nor HBase keeps a list that can be read or stopped, and the Query Server (Avatica) only knows its own connections. |
| Db2 for z/OS (ODBC) | Everything | It does not expose its threads through SQL: they are seen with `-DISPLAY THREAD`, IFI or OMEGAMON. |
| Spark Thrift Server, Kyuubi (ODBC) | Everything | They do not expose their sessions through SQL: they are seen in Spark's web interface. |
| Hive (ODBC) | Everything | HiveServer2 does not list its sessions through SQL: they are seen in its web interface (port 10002). |
| Actian Zen (ODBC) | Everything | It does not expose its sessions through SQL: they are seen in Zen Monitor. |
| Mimer SQL (ODBC) | Everything | It does not expose its sessions through SQL: they are seen with `sqlmonitor`. |
| NetSuite (ODBC) | Everything | SuiteAnalytics Connect is a read-only service: it does not report sessions. |
| Access, dBase (ODBC) | Everything | They are file databases without a server: they have no sessions to list. |
| Generic ODBC | Everything | The generic preset does not know the engine's session views: the engine's preset has to be used. |
| Informix, Cubrid, MonetDB, IRIS, OpenEdge, MaxDB, NuoDB, HeavyDB, Machbase, Ignite, Ignite3, Ocient, SQream, Ingres, Virtuoso, Impala (ODBC) | Everything | **Pending**, not impossible: DBine does not list these engines' processes yet. For each one, the query to its session views still has to be written (in the code the preset answers "this engine's processes aren't listed in DBine yet"). |

## Scheduled tasks

Scheduled tasks ([`scheduled-tasks.md`](scheduled-tasks.md)) work in **all engines**, with the six step types: run a script, export to file, compare schemas, backup, document the database and send an email. They add nothing to the driver: each step uses what the engine already offers for that function.

- **Run a script and Export:** as far as each engine's script execution and export go. Export always runs read-only.
- **Compare schemas:** as far as each engine's comparison goes, which does not change by being a task (see [Compare schemas](#compare-schemas)). The sync script is saved to a file and never run.
- **Backup:** the **Engine backup** only exists in engines that have their own backups (see [Backups](#backups)). The others offer only the **DBine copy**, so all engines can be backed up.
- **Document the database:** as far as the feature goes (see [Document the database](#document-the-database)); it runs in a read-only session.
- **Send an email:** it does not use the driver, so it does not depend on the engine: it sends over SMTP with the server from **Settings › Mail**.

Tested: the task steps have automated tests with SQLite databases (change approval, export, comparison, DBine copy, stop or continue on error). For the other engines there are no tests of the tasks themselves: each step uses the same code as the app's equivalent feature, with the support and the tests against servers listed in their sections.

Registration in the system scheduler (LaunchAgent, Task Scheduler, systemd or cron) and the notification were tested with the texts they generate in the automated tests; according to the code, the real execution against each operating system has no automated tests.

| Engine | What is missing | Reason |
|---|---|---|
| Those in the "What is missing and why" list of [Backups](#backups) | Engine backup | Without their own backups through SQL or the protocol, the step offers only the **DBine copy**. The reasons for each engine are in that section. |
| Those in the [Compare schemas](#compare-schemas) section with limits | What the comparison does not cover | The task compares the same as the feature; the limits and their reasons are in that section. |
| Connections that do not save their password | Unattended runs | The task reads the password from the system keychain; without a saved password there is no one to type it. |
| Any | **Only if…** in steps other than **Send an email** | The task engine evaluates the condition in any step, but the interface only offers it in **Send an email**. Explicit pending item: offering it in the other step types. |
| Linux without systemd | Keychain access from cron | A task registered with `crontab` may not see the user's keychain; a user systemd timer does. |

## Code quality

Code quality ([`code-quality.md`](code-quality.md)) flags problems in the editor of **all engines** except InfluxDB 2, which uses Flux. It is text analysis: it neither queries the server nor adds anything to the driver. Each engine receives the lexical analyzer and the rules of its family, which come from `DriverInfo` (`language`, `dialect`, `id`) in `src-tauri/src/lint/mod.rs`:

| Rule family | Engines |
|---|---|
| SQL (common) | All SQL-language ones |
| T-SQL (in addition to the common ones) | SQL Server, Azure SQL, Fabric, Babelfish and, through ODBC, Sybase ASE and SQL Anywhere (`mssql` or `sybase` dialect) |
| PostgreSQL | Those with the `postgres` dialect: PostgreSQL and its family, and Aurora DSQL |
| MySQL | Those with the `mysql` dialect: MySQL, MariaDB, TiDB, OceanBase and the family |
| Oracle | Oracle and Oracle Autonomous |
| InfluxQL | InfluxDB 1 and InfluxDB 3 |
| CQL (the common SQL ones that apply, plus the CQL ones) | Cassandra, ScyllaDB and Amazon Keyspaces |
| MongoDB | MongoDB, FerretDB and Amazon DocumentDB |
| CouchDB | CouchDB |
| Search consoles | Elasticsearch, OpenSearch and Solr |
| Redis | Redis, Valkey and Dragonfly |
| etcd | etcd |
| Cypher | Neo4j, Memgraph and Amazon Neptune |

**Tested:** the rules of all families and the assignment of engines to families have automated tests over text (`src-tauri/src/lint/tests.rs` and `src-tauri/src/commands/lint.rs`). Since the analysis does not use the server, there are no tests against real servers and none are needed.

| Engine | What is missing | Reason |
|---|---|---|
| InfluxDB 2 (Flux) | All rules | Explicit pending item: a Flux lexical analyzer and rules for it are missing. The Flux profile has no rules. |
| SQL engines without a family of their own (SQLite, libSQL, DuckDB, ClickHouse, Snowflake, BigQuery, Databricks, Spanner, Trino, Athena, SAP HANA, Firebird, Db2, Teradata, Vertica and the rest of the ODBC presets) | The engine's own rules | Explicit pending item: they only receive the common SQL rules. Each family's rules are missing. |
| Cosmos DB, DynamoDB (PartiQL), Couchbase (N1QL), OrientDB, ksqlDB, IoTDB, TDengine | The engine's own rules | Explicit pending item: their languages look like SQL and receive the common SQL rules, but there are no rules of their own for each dialect. |
| Elasticsearch, OpenSearch, Solr | Rules other than `leading-wildcard` and `write-all` | Explicit pending item: rules for their consoles are missing (for example, reads without a filter). |

## Document the database

**Document the database…** ([`database-docs.md`](database-docs.md)) and its scheduled task step work in **all engines**: the document is built from what the engine reports through the contract (`list_objects`, `database_schema`, `columns`, `definition`), so each engine comes out with what it has. It adds no methods to the driver. It runs in a read-only session.

**Tested:** a file-based SQLite database, with keys, an index, `CHECK`, a view and a trigger, documented in HTML and in Markdown and as a task step (automated test); and PostgreSQL against the `dbine-test-postgres` container (a test marked as ignored, run by hand). In the other engines there are no tests of the document itself: each part uses the same calls as the explorer, with the support and the tests of their sections.

| Engine | What is missing | Reason |
|---|---|---|
| Those without foreign keys (`capabilities().foreign_keys` false: for example Cassandra and ScyllaDB, MongoDB, CouchDB, Couchbase, Cosmos DB, Redis, Neo4j, ClickHouse, Athena, Dremio, InfluxDB, IoTDB and Aurora DSQL) | Foreign keys and the diagram lines | The engine has no foreign keys; the diagram's tables are left without lines. |
| Those in [Engines without dependencies](#engines-without-dependencies) | **Used by** | The engine has no foreign keys or objects with code that depend on others; the option shows disabled. |
| Those that do not return the code of an object type (`has_definition` false for that type) | Source code of that type | The engine has no text to return; the document lists it without code. |
| Any | Diagram with more than 150 tables per schema | Diagram limit (`DIAGRAM_MAX`): with more tables one diagram per schema is built and the one that exceeds the maximum is left without a diagram, with a warning. Explicit pending item: a diagram that can be trimmed or paginated. |
| Any | Diagram in Markdown | The diagram is an SVG inside the HTML; Markdown does not carry it. |

## Query builder

**Design query…** ([`query-builder.md`](query-builder.md)) is in all **SQL and CQL** language engines. The other languages (documents, key-value, graphs, Flux) do not have it, because there is no `SELECT` to build. Table names, quotes and the row limit come from each driver's **View data** query (`Session::browse_query`); what that text does not say (joins, grouping, `HAVING`, operators) is per dialect in `features()` of `src-tauri/src/commands/query_builder.rs`. It adds no methods to the driver.

**Tested:** the queries generated for SQL Server, PostgreSQL, Oracle, MySQL, MS Access, Cosmos DB, CQL and ksqlDB have automated tests over the SQL text, and a join runs end to end against SQLite. There are no tests against real servers of the other engines.

What each engine offers (what is not in the table it offers in full: `INNER`, `LEFT`, `RIGHT` and `FULL`, `GROUP BY`, `HAVING`, the six aggregates, `DISTINCT`, `ORDER BY`, limit, `OR` groups and the thirteen operators):

| Engine | What is missing | Reason |
|---|---|---|
| MySQL and its family, MS Access | `FULL OUTER JOIN` join | The MySQL dialect (MySQL, MariaDB and the family) and Access have no `FULL OUTER JOIN`. Access also nests each join in parentheses. |
| SQLite and libSQL before 3.39 | `RIGHT` and `FULL` | `RIGHT` and `FULL JOIN` arrived in SQLite in 3.39; the builder reads the server version. |
| Couchbase (N1QL), HeavyDB | `RIGHT` and `FULL` | Explicit pending item: the builder only generates `INNER` and `LEFT`, but the code does not record the engine's reason; it still has to be confirmed with the vendor's documentation. |
| Sybase ASE, CUBRID, Ignite, NuoDB, OpenEdge, Zen, Machbase, NetSuite (ODBC) | `FULL OUTER JOIN` | Explicit pending item: the builder only generates `INNER`, `LEFT` and `RIGHT`, but the code does not record the engine's reason; it still has to be confirmed with the vendor's documentation. |
| Cosmos DB, DynamoDB (PartiQL), InfluxDB 1, IoTDB, TDengine, ksqlDB, OrientDB | Joins | They query one container, measurement, device, stream or class at a time; the builder leaves a single table on the canvas. |
| Cassandra, ScyllaDB, Amazon Keyspaces | Joins, `GROUP BY`, `HAVING`, `DISTINCT`, `OR` groups, `COUNT DISTINCT` and operators other than `=`, `<`, `<=`, `>`, `>=` and `IN` | The builder generates single-table CQL. A filter outside the primary key adds `ALLOW FILTERING`, with a warning that it scans the table. |
| Cosmos DB | `HAVING`, `COUNT DISTINCT`, `IS NULL` and `IS NOT NULL` | Explicit pending item: the code does not record the engine's reason; it still has to be confirmed with the vendor's documentation. |
| DynamoDB (PartiQL) | `GROUP BY`, `HAVING`, aggregates, `DISTINCT`, `ORDER BY`, `LIKE` and `NOT LIKE`; the row limit | Explicit pending item: the code does not record the engine's reason. DynamoDB's **View data** query carries no row limit. |
| InfluxDB 1 | `HAVING`, `DISTINCT` and operators other than the six comparisons; ordering only by `time` | Explicit pending item: the code does not record the engine's reason. |
| IoTDB | `GROUP BY`, `HAVING`, `DISTINCT` and `COUNT DISTINCT` | Explicit pending item: the code does not record the engine's reason. |
| ksqlDB | `DISTINCT`, `ORDER BY` and `COUNT DISTINCT` | Explicit pending item: the code does not record the engine's reason. |
| OrientDB | `HAVING` and `COUNT DISTINCT` | Explicit pending item: the code does not record the engine's reason. |
| TDengine, MS Access | `COUNT DISTINCT` | Explicit pending item: the code does not record the engine's reason. |

## Copy a subset

**Copy a subset…** ([`data-subset.md`](data-subset.md)) is in the table menu of all engines (with objects that have columns and can be browsed). The source and target can be different engines: the structure of the missing tables is converted with `dbine_schema`. It adds no methods to the driver: it uses `database_schema`, `Driver::filtered_browse` (the `IN` key filter), `table_ddl`, `insert_script` and `update_script`. The source is read in a read-only session.

**Tested:** SQLite to SQLite (automated tests): a table with children, parents and masking, a foreign key cycle with a composite key, and the rejections (same source and target, read-only target, production without confirmation). PostgreSQL to PostgreSQL against the `dbine-test-postgres` container (a test marked as ignored, run by hand). The other engines have no tests of their own.

| Engine | What is missing | Reason |
|---|---|---|
| Those without foreign keys (documents, key-value, time series: see the list in [Document the database](#document-the-database)) | Parents and children | There are no foreign keys to follow: the chosen table or collection is copied, with its filter and masking. |
| Those that do not filter on the server (see [Per-column filters](#per-column-filters-on-table-data)) | Filter on the starting table, and finding parents and children by key | The copy asks the driver for the filters (`filtered_browse`); where the driver does not apply them, the copy may fail with that error. Redis and etcd are the cases that section declares without a filter on the server. |
| Engines without a condition language (those that are neither SQL nor CQL) | **Condition** | Only the **By column** filter, that of the data grid, is offered. |
| Any, with a target of another engine | Tables that cannot be created | If converting the structure to the target engine fails, the table is marked **cannot be copied** with the error and blocks the copy: it has to be created in the target beforehand. |
| Any | Copying more than 2,000,000 rows, or more than 1,000,000 from the starting table; undoing a partial copy | Limits of the feature: rows are gathered in memory before being written, and there is no transaction around the copy. Explicit pending item: copying in batches without gathering everything in memory. |

## Optimize query

**Optimize query** ([`query-optimizer.md`](query-optimizer.md)) is in the editor of **all engines**. It has four parts, which depend on the engine in different ways: the rewrite **rules** (per language and dialect), the **suggested indexes** (from the estimated plan), the **AI alternatives** (any engine: the query, the structure and the plan are sent, never rows) and **Compare** (it runs with `Session::execute` and the export's row sink, in a read-only session). It adds no methods to the driver: it uses `explain`, `supports_explain`, `database_schema` and `table_ddl`.

**Tested:** SQLite, with automated tests, the rules, the comparison (also the one that detects a non-equivalent version) and the suggested indexes. Against the `dbine-test-*` containers there are tests marked as ignored, run by hand: PostgreSQL (suggested indexes and comparison), SQL Server (the missing index the engine reports) and MongoDB (`COLLSCAN` and `$where`). The other engines have no tests of their own.

| Engine | What is missing | Reason |
|---|---|---|
| Cosmos DB, DynamoDB (PartiQL), ksqlDB, IoTDB, TDengine, InfluxDB 1 (InfluxQL), OrientDB, Couchbase (N1QL) | Rewrite rules | Their language lacks the constructs the rules rewrite (subqueries, `UNION`, joins in their general form) or treats `NULL` in its own way. The AI alternatives and their own remain. |
| Non-SQL languages, except MongoDB (CQL, Cypher, Redis, etcd, Flux, Elasticsearch, OpenSearch, Solr, CouchDB) | Rewrite rules | Explicit pending item: there are no rules for those languages. The AI alternatives and their own remain. |
| MongoDB, FerretDB, Amazon DocumentDB | All rules except `mongo_where` | Explicit pending item: only `$where` is rewritten, and only if it is comparisons of fields with constants joined by `&&`. |
| All SQL engines except PostgreSQL (and its family), MySQL (and its family), SQL Server and Oracle | `function_to_range` rule | Explicit pending item: each dialect's date literal (`date_sql`) still has to be written. |
| Those that give no plan: Redis, Valkey, Dragonfly, etcd, IoTDB, FerretDB, InfluxDB 2 (Flux), and through ODBC Exasol, CUBRID, Informix, GBase 8s, Altibase, Db2 for i, Ingres, Mimer, Caché, Zen, Access, dBase, NetSuite and OpenEdge | Suggested indexes, **Plan warnings** and the cost in **Compare** | The engine has no execution plans or does not deliver them through the available protocol. The reason for each is in [Execution plans](#execution-plans). **Compare** still measures times and result. |
| All except SQL Server | Index suggested by the engine itself | Only SQL Server reports missing indexes in its plan. In the others, the suggestion comes from a full scan of a table that the query filters or joins by columns that no index starts with (in MongoDB, a `COLLSCAN`). |
| Those that write data (`INSERT`, `UPDATE`, `DELETE`, and everything non-SQL that writes) | Execution in **Compare** | A query that writes is never run: only its estimated plan is compared. In non-SQL languages the read-only session rejects the write. |

## Search

**Search in database…** ([`search.md`](search.md)) works in **all engines**. Names come from `list_objects` and columns from `database_schema` (if the engine does not give it, the search continues with names and code). Code is searched in two ways with the same result: through the engine's catalog in a few queries (`Session::search_code`) or, where the driver does not implement it, by reading each object's definition with progress and partial results. It runs in a read-only session.

Fast path through the catalog, according to each `search.rs`:

| Engine | What is read from the catalog | What is left for object-by-object reading |
|---|---|---|
| SQL Server, Azure SQL | `sys.sql_modules` (views, routines, triggers), filtered on the server with `LIKE` | Sequences, synonyms, types and full-text catalogs, one at a time |
| PostgreSQL and its family | `pg_get_viewdef`, `pg_get_functiondef` and `pg_get_triggerdef`, one query per type, filtered with `LIKE`/`ILIKE` | Sequences, types and synonyms (they are built from several catalogs). CockroachDB, the streaming engines, CrateDB, H2, Redshift and Denodo: everything, because their sources only come out one at a time (`SHOW CREATE`) |
| Oracle | `DBMS_METADATA.GET_DDL` of views, routines, packages and triggers in one query, and of tables in another, filtered with `DBMS_LOB.INSTR`; types, sequences and synonyms from `ALL_SOURCE`, `ALL_SEQUENCES` and `ALL_SYNONYMS` | Everything, if a bulk read fails (for example, without privilege on `DBMS_METADATA`) |
| SAP HANA | `DEFINITION` of views, procedures, functions and triggers, unfiltered on the server (they are NCLOB) | Tables and sequences (`GET_OBJECT_DEFINITION`, one call per object), synonyms and table types |
| Firebird | `RDB$RELATIONS`, `RDB$PROCEDURES`, `RDB$FUNCTIONS`, `RDB$PACKAGES`, `RDB$TRIGGERS`, `RDB$GENERATORS` and `RDB$FIELDS`, unfiltered on the server | — |
| ClickHouse | `system.tables` (tables, views, dictionaries, streams) and `system.functions`, filtered on the server | — |
| BigQuery | `INFORMATION_SCHEMA.TABLES` and `ROUTINES` (the `ddl`) | Objects without `ddl` in `INFORMATION_SCHEMA`; in the emulator, everything |
| Snowflake | `FUNCTIONS`, `PROCEDURES` and `SEQUENCES` of `INFORMATION_SCHEMA` | Tables, views, streams and tasks (`GET_DDL`, one call per object) and overloaded routines |
| Databricks | `routine_definition` of the functions | Tables, views and materialized views (`SHOW CREATE TABLE`, one statement per object) |
| ODBC presets | Each preset's definition queries, run once for all objects | Types whose source is not a query (`SHOW …` in Hive, Impala, Spark and Teradata; `GET_DDL(?)`; Netezza) and the generic preset, which guesses `INFORMATION_SCHEMA` |
| All the others (MySQL and family, SQLite, libSQL, DuckDB, Trino, Athena, Spanner, Cassandra, MongoDB, etc.) | — | Everything, object by object. Explicit pending item: a catalog path; drivers that do not have it read each definition separately and take longer on databases with thousands of routines. |

Without definitions to read, only names and columns are searched: engines whose object types have no code (key-value, time series, most document ones).

**Tested:** the code of the catalog paths asserts, in each `search.rs`, that it gives the same results as reading object by object; I have no record of tests of search against real servers.

## Health check

The **Health check** ([`health-check.md`](health-check.md)) works in **all engines** with the common checks, which use what DBine already reads (Monitor, processes and backups). Each driver can add its own with `Session::health_checks`; engines without checks of their own show only the common ones. It runs in a read-only session and the fix scripts are only opened in a query.

| Common check | Engines that do not have it | Reason |
|---|---|---|
| Connections and cache hits | Those without Monitor (`capabilities().monitor` false) | Without Monitor there are no connection or cache metrics. See [Server monitor](#server-monitor). |
| Long queries, locks and open transactions | Those without a process list (`capabilities().processes` false) | Without processes there is nothing to measure. See [Processes](#processes). |
| Last backup | Those without their own backups (`Driver::backup()` empty) | Without engine backups there is no history to query. See [Backups](#backups). |

Own checks (one `health.rs` per driver):

| Engine | What it checks | What it does not |
|---|---|---|
| SQL Server, Azure SQL | Configuration, statistics, unused indexes, untrusted constraints, foreign keys without an index, heaps, disabled indexes | What requires `VIEW SERVER STATE` or does not exist in Azure is skipped |
| PostgreSQL and family | Autovacuum, dead tuples, *wraparound*, unused, invalid and duplicate indexes, foreign keys without an index, tables without a primary key, sequences | Vacuum and dead tuples, in CockroachDB and YugabyteDB (their storage has no `VACUUM`) and in the MPP variants (the coordinator's counters do not see the segments); *wraparound* in openGauss (its XIDs are 64-bit). Specific: CockroachDB (automatic statistics), Redshift (stale statistics and unsorted rows) |
| MySQL, MariaDB, TiDB, OceanBase | Tables without a primary key, MyISAM, unused and redundant indexes, fragmentation, foreign keys without an index, mixed collations | Unused indexes: without counters (`performance_schema` or `userstat` off) it is skipped, and in OceanBase it is skipped because it keeps counts without a start date. Fragmentation: not in Aurora (its storage does not report that space). Foreign keys without an index: not in OceanBase, InnoDB and TiDB create one by themselves |
| Oracle | Invalid objects, unusable indexes, tablespaces, statistics, foreign keys without an index, tables without a primary key, sequences, recycle bin | What needs `DBA_` views without access is skipped |
| SAP HANA | Invalid objects, *delta* merge, tables without a primary key, virtual tables without statistics | What needs monitoring views without access is skipped |
| Firebird | Distance between transactions, forced writes, index statistics, inactive indexes, tables without a primary key | — |
| ClickHouse | Partitions with too many parts, detached parts, replicas, mutations, tables without TTL | Old versions, Timeplus or without access to `system`: each check that fails is skipped |
| Snowflake | Time Travel, *clustering*, retained dropped tables, *warehouses* that do not suspend | Cost and maintenance only, with `SHOW`: no *warehouse* is woken and no data is read |
| BigQuery | Large unpartitioned tables, partition filter, expiration, billing model, *time travel* | Cost and maintenance only, with the REST API (free metadata, no jobs) |
| Databricks | Auto-stop, predictive optimization, deleted file retention, non-Delta tables | Large tables without *clustering*: the size requires `DESCRIBE DETAIL` on a *warehouse*, and this check uses none |
| ODBC: Db2 LUW, Sybase ASE, Informix and GBase 8s | See [`health-check.md`](health-check.md) | Db2 for i and z/OS, Teradata, Vertica and the other presets: their catalogs are not read yet (explicit pending item); the generic preset does not know the engine |
| All the others (Cassandra, MongoDB, Redis, Neo4j, DuckDB, SQLite, etc.) | Only the common ones | Explicit pending item: there are no checks of their own; what is worth checking in each one still has to be defined |

Each own check is a separate query: if it fails (old version, permissions), it is skipped and listed under **Could not be checked**.

## Test data

**Generate test data…** ([`test-data.md`](test-data.md)) works in the engines that **insert from DBine with `insert_script`**. The generators live in DBine; the driver only provides the insert script, the columns and, where they exist, the foreign keys. It appears in the tables of connections that are not read-only.

These have their own `insert_script`: Athena, BigQuery, Cassandra, ClickHouse, Cosmos DB, Couchbase, CouchDB, Databricks, Dremio, DynamoDB, Elasticsearch, etcd, Firebird, SAP HANA, ksqlDB, MongoDB, MySQL and its family, Neo4j, ODBC, Oracle, OrientDB, Phoenix, PostgreSQL and its family, Redis, Solr, Snowflake, Spanner, SQL Server, TDengine and Trino. The rest use the standard `INSERT` of SQL engines (SQLite, libSQL, DuckDB, Flight SQL, Aurora DSQL…).

**Tested:** end to end, only SQL Server (`DBINE_TEST_SQLSERVER_URL`, a test marked as ignored that is run by hand). The other engines have no tests of their own for this feature.

| Engine | What is missing | Reason |
|---|---|---|
| Apache Drill | Everything | Drill has no `INSERT`: tables are created with `CREATE TABLE AS SELECT`. |
| InfluxDB 1, 2 and 3 | Everything | Flux has no insert language, InfluxQL has no `INSERT` over the HTTP API and InfluxDB 3's SQL is read-only: points are written with *line protocol*. Explicit pending item: generating and sending *line protocol*. |
| ksqlDB | Inserting into topics | ksqlDB does not insert into topics: it has to be done into a stream. |
| IoTDB | Tables without a `Time` column | IoTDB needs the timestamp to insert rows. |
| CouchDB | Views | Documents are not inserted into a view. |
| Those without foreign keys | **From the referenced table** and automatic foreign keys | The engine has no foreign keys to read; columns are filled with the generator by name or type. |
| Any | Uniqueness against existing rows and unique keys other than the primary | The generator only verifies a column's primary key, and only against what it generated itself. Explicit pending item: reading `UNIQUE` constraints and existing rows. |
| Any | Undoing an interrupted generation | Rows are inserted 500 at a time without a global transaction: the batches before the error remain. |

## Database properties

**Properties…** ([`database-properties.md`](database-properties.md)) appears in engines with the `database_properties` capability. Each driver (`properties.rs`) reports its own and only offers what the server reports, so an option from a newer version appears only if it exists. Changing a property generates the engine's script, which is reviewed and confirmed with the warnings before being applied; on read-only connections they are only seen. Fields with suggestions from the server (collations, locations…) are the same as in **New database** (see [Create databases: options](#create-databases-options)).

Data only, with nothing to change:

| Engine | Reason |
|---|---|
| DuckDB | It does not store per-database settings: `SET` and `PRAGMA` belong to the instance or the session, and how a database is attached is fixed by `ATTACH`. |
| Redis, Valkey, Dragonfly | They do not store per-database settings: `CONFIG SET` changes the whole server. |
| SAP HANA | The "database" is a schema and HANA has no `ALTER SCHEMA`: the owner is set on creation and the other settings live in tables, partitions and columns. |
| Informix and GBase 8s (ODBC) | The logging mode is changed with `ondblog`/`ontape` and a level-0 backup, not with SQL. |
| Memgraph | The storage mode and isolation have their own statements outside this dialog. Explicit pending item: offering them. |
| Neo4j Community | It has no `ALTER DATABASE`. |
| libSQL | Only `user_version`: the server manages the journal (always WAL), page size and vacuum, and rejects those `PRAGMA`s and `VACUUM`. |
| FerretDB, Amazon DocumentDB | FerretDB has no `profile` command; in DocumentDB the *profiler* is defined in the cluster parameter group and writes to CloudWatch Logs. |
| Dremio (spaces and homes) | They only have a name; sources do have refresh policies. |
| Spanner with PostgreSQL dialect | They are shown but not changed: the driver speaks GoogleSQL. |
| Cosmos DB without its own throughput | With per-container or *serverless* throughput there is no database offer to change. |

With properties that are not offered even though they are shown: Firebird (read-only, forced writes and *sweep* interval go through the services API, which the client used does not have; encryption needs a plugin and a key that a connection does not see), Athena (its DDL does not change the description or the location, nor remove properties), Databricks (storage root, isolation and catalog type do not change through SQL), TDengine (`MAXROWS` and `KEEP_TIME_OFFSET`: 3.3 rejects the first and takes the second without applying it) and Couchbase (type, storage engine and conflict resolution are not edited after creating the *bucket*; auto-compaction and encryption at rest are left out).

Without the feature:

| Engine | What is missing | Reason |
|---|---|---|
| Amazon Neptune | Everything | Settings live in the cluster parameter group (AWS API), not behind openCypher. |
| Databend | Everything | Its `ALTER DATABASE` only renames. |
| Manticore | Everything | It has no databases. |
| Denodo, CrateDB, H2 | Everything | They have no databases that DBine manages. |
| Apache Drill, DynamoDB, Aurora DSQL, Elasticsearch, OpenSearch, etcd, Flight SQL, ksqlDB, Phoenix, Solr, Trino | Everything | Explicit pending item: they have no `properties.rs`, and the code does not record whether the engine has per-database properties. |

**Tested:** I found no tests of this feature against real servers in the `properties.rs` files reviewed, except for the unit ones that some drivers have; what was implemented from the vendor's documentation cannot be distinguished in the code.

## Rename with impact

**Rename…** ([`rename.md`](rename.md)) changes an object's name
and recreates the code that names it, in a single script. The contract is
in `crates/dbine-driver/src/rename.rs` (`Driver::rename_spec` and
`Driver::rename_script`); the search and rewrite of dependents are
common to all engines. Each driver fills in its row when it implements the
feature and tests it against a real server; until then the explorer does
not offer **Rename…** in that engine.

| Engine | Status | What it renames | Dependents the engine updates by itself | How the rewritten ones are put back | Limits |
|---|---|---|---|---|---|
| PostgreSQL (and Aurora, AlloyDB, Cloud SQL, Timescale, Yugabyte, Greenplum, Cloudberry, Greengage, EDB, KingbaseES, Fujitsu, openGauss) | yes | table, view, materialized view, sequence, type, domain, function, procedure, trigger, column, index, constraint, schema (`ALTER … RENAME TO`, `RENAME COLUMN`, `RENAME CONSTRAINT`, `ALTER FUNCTION f(args) RENAME TO` once per overload, `ALTER TRIGGER t ON table RENAME TO`) | views, materialized views, triggers, foreign keys, indexes, `BEGIN ATOMIC` functions | plpgsql/sql functions and procedures: `CREATE OR REPLACE`; in a transaction (YugabyteDB: no transaction) | dynamic `EXECUTE` stays manual; the table's own indexes, constraints and sequences keep their name; `search_path`s that name the schema are not updated |
| CockroachDB | yes | same as PostgreSQL (index: `ALTER INDEX table@index`) | foreign keys and indexes | views and functions that use the object: they are dropped before and created after (they lose permissions) | DDL is committed statement by statement (`autocommit_before_ddl`): no transaction; a trigger function that names the table prevents renaming it |
| Amazon Redshift, Yellowbrick | yes (no live test) | table, view (Redshift: with `ALTER TABLE`), column, schema | foreign keys | views: `CREATE OR REPLACE` | no indexes, constraints or routines; no transaction |
| Materialize, RisingWave | yes | table, view, materialized view, source, sink, index, schema | views, materialized views and sinks | — | no columns; no transaction |
| CrateDB | yes | table, view (`ALTER TABLE … RENAME TO`), column (5.5+) | — | views: `CREATE OR REPLACE` | no schemas, indexes or constraints; no transaction |
| H2 (servidor PostgreSQL) | yes | table, view, column, index, constraint, schema | — | views: `CREATE OR REPLACE` | no transaction |
| Denodo | no | — | — | — | it does not modify objects through SQL: views are defined in Denodo |
| Amazon Aurora DSQL | yes (no live test) | tables, views and sequences (`ALTER TABLE\|VIEW\|SEQUENCE … RENAME TO`), functions (`ALTER FUNCTION f(args) RENAME TO`, each overload), table and view columns (`RENAME COLUMN`) and constraints (`RENAME CONSTRAINT`) | views | `CREATE OR REPLACE` (SQL functions) | it does not rename indexes, schemas or domains (it has no `ALTER INDEX`, `ALTER SCHEMA` or `ALTER DOMAIN`); each DDL statement goes in its own transaction: it is not atomic |
| SQL Server, Azure SQL | yes | table, view, procedure, function, trigger, column, index, constraint (`sp_rename`; a module is renamed with `sp_rename` and then `CREATE OR ALTER` with the new header, because `sp_rename` does not change the stored text) | foreign keys, indexes and constraints of the renamed object | `CREATE OR ALTER` (keeps permissions); views with SCHEMABINDING are dropped before and created after; in a transaction | no schemas or synonyms; a column used by CHECK constraints or filtered indexes is renamed by dropping and recreating them in the same batch; if a computed column uses it, it is not renamed |
| Microsoft Fabric Data Warehouse | yes (no live test) | table, column (`sp_rename`) | — | `CREATE OR ALTER`; no transaction | no views, routines, indexes or constraints |
| Babelfish for PostgreSQL | yes | table, view, procedure, function, column (`sp_rename`; procedures and functions are dropped and created with the new name) | CHECK constraints and computed columns when renaming a column | dropped before and created after (they lose permissions); in a transaction | no triggers, constraints, indexes, schemas or synonyms |
| MySQL, Aurora MySQL, Cloud SQL para MySQL | yes | table and view (`RENAME TABLE`), column (`CHANGE COLUMN` with the full definition: it works in all versions), index (`RENAME INDEX`, MySQL 5.7+) | foreign keys and indexes; checks on the column are dropped and added again with the new name in the same `ALTER TABLE` | views, routines and triggers: they are dropped and created again (they lose permissions) | no constraints (databases: see [Renaming a database](#renaming-a-database)); DDL without a transaction; a foreign `DEFINER` requires `SET_USER_ID` (`SET_ANY_DEFINER` since 8.2) or `SUPER`; a text column's own collation has to be added by hand |
| MariaDB | yes | table and view (`RENAME TABLE`), column (`CHANGE COLUMN` with the full definition), index (`RENAME INDEX`, 10.5+) | foreign keys, indexes and checks | views, routines and triggers: `CREATE OR REPLACE` | no constraints (databases: see [Renaming a database](#renaming-a-database)); DDL without a transaction; a foreign `DEFINER` requires `SET USER` or `SUPER`; a text column's own collation has to be added by hand |
| TiDB | yes | table and view (`RENAME TABLE`), column (`CHANGE COLUMN`), index (`RENAME INDEX`) | foreign keys and indexes; checks on the column are dropped and added again | views: `CREATE OR REPLACE` | no databases or constraints; DDL without a transaction |
| StarRocks, Apache Doris, VeloDB, GreptimeDB | yes | table (`ALTER TABLE … RENAME`) | — | views: dropped and created again | no views, columns or indexes |
| SingleStore, Databend, OceanBase (MySQL) | yes (no live test) | table (SingleStore: `ALTER TABLE … RENAME TO`; Databend and OceanBase: `RENAME TABLE`) | — | views and routines: dropped and created again | no views, columns or indexes |
| Manticore Search | no | — | — | — | the engine does not rename tables from SQL |
| Oracle, Oracle Autonomous Database | yes | table, view, column, index, constraint, sequence, private synonym and trigger with the engine's `RENAME` (views, sequences and synonyms: only when connected as the schema's owner); procedures, functions and packages by recreation (`CREATE` with the new name and `DROP` of the old one) | foreign keys, indexes and checks; views and code are left `INVALID` | `CREATE OR REPLACE [FORCE]`: they are left `VALID` without recompiling; what depends on them recompiles by itself when used | DDL commits by itself (no transaction); it does not rename users (schemas), public synonyms, materialized views or types; recreating a routine loses its permissions; hints in comments are not touched |
| SQLite | yes | tables and virtual tables (`ALTER TABLE … RENAME TO`), columns (`ALTER TABLE … RENAME COLUMN`, 3.25+), views, triggers and indexes (created with the new name and the old one dropped) | when renaming a table or a column: views, triggers, indexes, CHECK and foreign keys (with `legacy_alter_table` OFF; the script turns it off beforehand) | drop and create, in a transaction; when renaming a view, the views and triggers that use it are rewritten | it fails if there is already a broken view in the database; it does not rename constraints, attached databases or the automatic PRIMARY KEY/UNIQUE indexes |
| libSQL / Turso | yes | same as SQLite (`ALTER TABLE … RENAME TO / RENAME COLUMN`; views, triggers and indexes by recreation) | when renaming a table or a column: views, triggers, indexes, CHECK and foreign keys | drop and create, in a transaction (Hrana 3 servers); when renaming a view, the views and triggers that use it are rewritten | as SQLite; the server rejects `PRAGMA legacy_alter_table` and the script does not send it |
| DuckDB | yes | tables (`ALTER TABLE … RENAME TO`), views (`ALTER VIEW … RENAME TO`), columns (`ALTER TABLE … RENAME COLUMN`) | indexes and CHECK; views and macros do not | `CREATE OR REPLACE` after the change, in a transaction | it rejects renaming a table with indexes or referenced by a foreign key, and a column that is indexed or in a foreign key; it does not rename indexes, sequences, macros, types or schemas |
| Archivos CSV / Parquet / JSON | no | — | — | — | views are regenerated from the folder's files on each connection: the file is renamed |
| MongoDB | yes | collections (`renameCollection`), views (dropped and created with `db.createView`), fields (`updateMany` with `$rename`) | — | views that depend: `viewOn`, `$lookup.from`, `$graphLookup.from`, `$unionWith.coll`, `$out` and `$merge` are rewritten, and dropped and created (they store no data or lose permissions: roles grant by name); when renaming a field, the indexes that use it are dropped and created with the new field and the validator is changed with `collMod` | it is not transactional; renaming a field rewrites every document that has it; `_id`, time series collections and their time or metadata field are not renamed; fields used by views are listed and not rewritten; roles with privileges on the old name are not updated |
| FerretDB | yes | collections (`renameCollection`), fields (`updateMany` with `$rename`) | — | when renaming a field, the indexes that use it are dropped and created with the new field | it has no views; it does not store validators; it is not transactional; renaming a field rewrites every document that has it |
| Amazon DocumentDB | yes (no live test) | collections (`renameCollection`), fields (`updateMany` with `$rename`) | — | when renaming a field, the indexes that use it are dropped and created with the new field; the validator is changed with `collMod` | it has no views; it is not transactional; renaming a field rewrites every document that has it |
| Azure Cosmos DB | no | — | — | — | it does not rename databases or containers; a field cannot be renamed on the server side (the `UPDATE` goes document by document, by `id`, and does not delete fields) |
| Couchbase | yes | fields (`UPDATE ks SET b = a UNSET a WHERE a IS NOT MISSING`) | — | the GSI indexes that use the field are dropped after the `UPDATE` and created with the new field; SQL++ functions are listed and not rewritten | it does not rename buckets, scopes or collections; an index serving the `WHERE` is required (the field's or the primary); it is not atomic |
| CouchDB | no | — | — | — | it does not rename databases; a field could only be renamed by rewriting each document from the client with its `_rev` |
| ClickHouse | yes | tables, views, materialized views, dictionaries (`RENAME TABLE\|DICTIONARY`), columns (`ALTER TABLE … RENAME COLUMN`) and databases with the Atomic engine (`RENAME DATABASE`, from the database's node) | the table's skip indexes and CHECK | `CREATE OR REPLACE`; a materialized view without `TO` is left empty because it is replaced empty | it does not rename key columns or those a materialized view reads (a server-side guard stops it); it does not detect `Distributed` tables or dictionaries that read it; it is not transactional |
| Timeplus Proton | yes | streams, views, materialized views (`RENAME STREAM`) and columns (`ALTER STREAM … RENAME COLUMN`) | — | dropped before and created after; a re-created materialized view loses what it stored | it does not rename databases or the stream key's columns; it does not detect external streams or dictionaries; it is not transactional |
| Cassandra, ScyllaDB | yes | primary key columns (`ALTER TABLE ks.t RENAME a TO b`) | — | — | it does not rename ordinary columns, tables, keyspaces, types or functions; the server rejects a column with a secondary index (DBine warns beforehand) or used by a materialized view |
| Amazon Keyspaces | no | — | — | — | its `ALTER TABLE` has no `RENAME` |
| Elasticsearch, OpenSearch, Open Distro | yes | index by copy (`PUT /old/_block/write` → `POST /old/_clone/new` → wait for the copy → `DELETE /old`, or `_aliases` with `remove_index`); alias (`_aliases` atomic remove+add) | the index's aliases: they move to the new one in the same atomic step | — | the clone copies the data (it takes time, uses disk) and the index does not accept writes while it lasts; fields are not renamed (it requires reindexing); data streams are not renamed |
| Apache Solr | yes | core in standalone mode (CoreAdmin `RENAME`) | — | — | the core's folder keeps the previous name; in SolrCloud it is not renamed (`RENAME` only adds an alias) and the server rejects it |
| Snowflake | yes (no live test) | tables, views, materialized views, sequences (`ALTER … RENAME TO`), functions and procedures (`ALTER FUNCTION\|PROCEDURE f(types) RENAME TO`, each overload), columns (`ALTER TABLE … RENAME COLUMN`), schemas (`ALTER SCHEMA … RENAME TO`) | foreign keys | — (nothing is re-created: views, materialized views, functions, procedures, tasks, streams and dynamic tables that name the object are only listed, to fix by hand; re-creating them would hand them to the renaming role and lose their security settings, the schedule of tasks, the offset of streams and the rows of materialized and dynamic tables) | no indexes or constraints; databases: see [Renaming a database](#renaming-a-database); DDL is committed statement by statement |
| BigQuery | yes (no live test) | tables (`ALTER TABLE … RENAME TO`), columns (`ALTER TABLE … RENAME COLUMN`) | — | `CREATE OR REPLACE` | no views, routines or datasets; it does not rename partition, clustering or key columns or STRUCT fields; search and vector indexes are lost; it cannot be done with streaming active; references written as `project.dataset.table` inside a single pair of backticks are not detected |
| Databricks | yes (no live test) | tables (`ALTER TABLE … RENAME TO`), views (`ALTER VIEW … RENAME TO`), columns (`ALTER TABLE … RENAME COLUMN`, with column mapping) | — | `CREATE OR REPLACE` | columns only in Delta tables with `delta.columnMapping.mode` = `name` or `id`; no schemas, functions or materialized views; with AWS Glue as metastore there is no `RENAME` |
| Trino, Starburst | yes | tables, views and materialized views (`ALTER TABLE\|VIEW\|MATERIALIZED VIEW … RENAME TO`), columns (`ALTER TABLE … RENAME COLUMN`) and schemas (`ALTER SCHEMA … RENAME TO`) | — | — (views, materialized views and SQL functions that name the object are only listed, to fix by hand: re-creating them would make the renaming user their owner and lose the security settings they had); the script starts with a `USE` of the object's schema | what is accepted is decided by the connector (memory and Iceberg rename everything; Hive does not rename some things); names are stored in lowercase, so uppercase is rejected; it is not transactional; when renaming a schema, views inside that name tables unqualified stop working (a warning is given) |
| Presto | yes | tables (`ALTER TABLE … RENAME TO`), views (`ALTER VIEW … RENAME TO`), columns (`RENAME COLUMN`) and schemas (`ALTER SCHEMA … RENAME TO`) | — | — (views and SQL functions that name the object are only listed, to fix by hand, for the same reason as Trino); the script starts with a `USE` of the schema | no materialized views; the memory connector does not rename columns or schemas, and old versions have no `ALTER VIEW … RENAME`; the rest, same as Trino |
| Amazon Athena | yes (no live test) | Iceberg tables (`ALTER TABLE … RENAME TO`), Iceberg table columns (`ALTER TABLE … CHANGE COLUMN`) and views (created with the new name and the old one dropped) | — | `CREATE OR REPLACE VIEW` | external (Hive) tables are not renamed: the server rejects the table and DBine rejects its columns, because in Parquet or ORC they would stop reading data; names only `[a-z0-9_]`; the renamed view loses its Lake Formation permissions; a column is renamed only if the catalog's type for it fits a strict type grammar (primitives, `decimal`, `char`, `varchar`, `array`, `map` and `struct`, with no quotes, `;`, comments or line breaks), otherwise the rename is refused; it is not transactional |
| Dremio | yes | Iceberg table columns (`ALTER TABLE … CHANGE COLUMN`) and views (created with the new name and the old one dropped) | — | `CREATE OR REPLACE VIEW` | it has no `RENAME`: tables, spaces and folders are not renamed; composite-type columns, no; the renamed view loses reflections, wiki, tags and permissions; dependents are only searched in the same space or source and its folders; a view or column type whose stored text would split into several statements (a `;` outside strings and comments; `//` is read as a comment, as the server does) is refused |
| Apache Drill | yes | views (created with the new name and the old one dropped) | — | `CREATE OR REPLACE VIEW` | tables (files), columns and workspaces are not renamed; a view whose stored text would split into several statements (a `;` outside strings and comments; `//` is read as a comment, as the server does) is refused |
| Apache Arrow Flight SQL | no | — | — | — | generic protocol: the DDL depends on the backend and there is no portable one |
| Google Cloud Spanner | yes | tables (`ALTER TABLE … RENAME TO`) and views (created with the new name and the old one dropped) | indexes, foreign keys, interleaved tables and change streams | dropped before the change and created again after (Spanner does not rename a table used by a view); views that read a dropped view, in a chain, are dropped before and created again unchanged | it does not rename columns, indexes, sequences, constraints or schemas; it is not atomic; foreign keys were not tested live (the emulator does not rename tables with foreign keys) |
| SAP HANA | yes (no live test) | table, column, index (`RENAME TABLE s.t TO n`, `RENAME COLUMN s.t.c TO n`, `RENAME INDEX s.ix TO n`) | indexes and foreign keys (to verify) | `CREATE OR REPLACE` (views, procedures, functions) | no views, routines, constraints or schemas; DDL without a transaction; in versions without `CREATE OR REPLACE` that step fails |
| Firebird | yes | column (`ALTER TABLE t ALTER COLUMN a TO b`) | ordinary indexes | dropped before and created after with `CREATE OR ALTER`; no transaction (the drop only takes effect on commit) | it rejects if the column is in a view, routine, trigger, CHECK or primary/unique/foreign key; triggers with `NEW.column` stay manual |
| ODBC: Db2 (LUW) | yes (no live test) | table, column, index (`RENAME TABLE\|INDEX`, `ALTER TABLE … RENAME COLUMN`) | indexes | `CREATE OR REPLACE`; in a transaction | it rejects tables with triggers or in foreign keys |
| ODBC: Db2 for z/OS | yes (no live test) | table, column, index (`RENAME TABLE\|INDEX`, `ALTER TABLE … RENAME COLUMN`) | indexes | drop and create | it rejects tables with triggers or read by views that are not rewritten |
| ODBC: Db2 for i | yes (no live test) | table, view, index (`RENAME TABLE\|INDEX`) | — | drop and create | no columns |
| ODBC: Sybase ASE | yes (no live test) | table, view, column, index (`sp_rename`) | keys and indexes | drop and create | only objects of the connected user |
| ODBC: Informix | yes (no live test) | table, column, index (`RENAME TABLE\|COLUMN\|INDEX`) | views | drop and create (triggers and SPL) | — |
| ODBC: Teradata | yes (no live test) | table, view (`RENAME TABLE\|VIEW db.x TO db.n`) | — | drop and create | no columns |
| ODBC: Vertica | yes (no live test) | table, view, column, schema (`ALTER TABLE\|VIEW\|SCHEMA … RENAME TO`, `RENAME COLUMN`) | — | `CREATE OR REPLACE` | DDL without a transaction |
| ODBC: Exasol | yes (no live test) | table, view, column, schema (`RENAME TABLE\|VIEW\|SCHEMA`, `ALTER TABLE … RENAME COLUMN`) | — | `CREATE OR REPLACE` | — |
| ODBC: rest of the presets | no | — | — | — | unconfirmed syntax |
| Apache Phoenix | no | — | — | — | Phoenix has no `RENAME` of tables or columns |
| ksqlDB | no | — | — | — | it does not rename streams, tables or columns (`ALTER STREAM/TABLE` only adds columns) |
| Redis, Valkey, Dragonfly | yes | keys (`RENAMENX` inside an `EVAL`: if the new key already exists, it fails without overwriting it) | — | — (the server stores nothing that names a key) | it keeps value and TTL; in Redis Cluster both keys have to fall in the same hash slot (a warning is given if not; use the same `{…}` tag) |
| etcd | no | — | — | — | the scripting language (etcdctl) has no `txn`: there is no atomic way to put the new key and delete the old one without overwriting an existing one |
| Amazon DynamoDB | no | — | — | — | the service does not rename tables |
| InfluxDB (v1, v2, v3) | no | — | — | — | InfluxQL does not rename databases, retention policies or measurements; in v2 the API renames a bucket, but the session script is Flux and cannot express it; InfluxDB 3 Core does not rename databases or tables |
| Apache IoTDB, TimechoDB | no | — | — | — | the tree model does not rename series, devices or databases (`ALTER TIMESERIES … RENAME` only changes tag keys); 2.x's table model rejects renaming tables and columns |
| TDengine | yes | ordinary table columns (`ALTER TABLE … RENAME COLUMN a b`, the timestamp one included) and supertable tags (`ALTER STABLE … RENAME TAG a b`) | the tag's index | streams that use the name are dropped before and created again after, rewritten, over the same output table (a tag used by a stream goes the same way) | it does not rename tables, supertables, supertable columns, subtables, views, streams, topics or databases; the server rejects a column or tag used by a topic; what arrives while the stream is dropped is not processed |
| OrientDB | yes | vertex, edge and document classes (`ALTER CLASS … NAME`, `UNSAFE` on edges) and properties (`ALTER PROPERTY … NAME` + `UPDATE … SET new = old REMOVE old`) | — | they are listed, not rewritten (functions) | their indexes are dropped and recreated (they keep the name), read from the class structure; on edges the vertices' `out_`/`in_` fields are moved; it is not atomic; it does not rename indexes, functions or sequences, nor V/E, `out`/`in`, or `@` attributes |
| Neo4j, Memgraph, Amazon Neptune | not offered | — | — | — | A label or relationship type is not renamed: changing it is `SET n:New REMOVE n:Old` on every node (or recreating each relationship), which rewrites the data, can take hours on a large graph, is not atomic outside a transaction the size of the graph and forces recreating the label's indexes and constraints. Queries saved outside the database are not seen either. |

### Renaming a database

**Rename…** on a database node is offered in SQL Server (also Azure SQL
Database and Babelfish), the PostgreSQL family (PostgreSQL, Aurora, AlloyDB,
Cloud SQL, Timescale, YugabyteDB, Greenplum, Cloudberry, Greengage, EDB,
KingbaseES, Fujitsu, openGauss, CockroachDB, RisingWave, Redshift,
Yellowbrick), MySQL, MariaDB, Aurora MySQL, Cloud SQL for MySQL, Snowflake and
MongoDB. Details of each are in [`rename.md`](rename.md#renaming-a-database).

Tested against real servers: SQL Server, Babelfish, PostgreSQL, CockroachDB,
MySQL, MariaDB and MongoDB (SQL Server, PostgreSQL and CockroachDB
included the sessions-open case). YugabyteDB, openGauss, Greengage and
RisingWave were checked by hand. Azure SQL Database, Redshift, Yellowbrick,
Snowflake (unit tests only) and the rest follow the vendor's documentation.

| Engine | What is missing | Reason |
|---|---|---|
| Microsoft Fabric | Rename a warehouse | Warehouses are renamed in the Fabric portal, not from T-SQL |
| SQL Server, Azure SQL, Babelfish | System databases | Refused on purpose |
| Materialize | Rename a database | Its `ALTER DATABASE` has no `RENAME` |
| CrateDB | Rename a database | Its databases are schemas; schemas are renamed from their own node |
| H2 | Rename a database | One database per file and no SQL to rename it: rename the file and edit the connection |
| Denodo | Rename a database | It changes nothing through SQL; databases are managed in Denodo |
| TiDB | Rename a database | `RENAME TABLE` across databases leaves the foreign keys pointing at the old database, so they would break when it is dropped |
| SingleStore, StarRocks, Apache Doris, VeloDB, Databend, GreptimeDB | Rename a database | A table can't be moved to another database with a rename, so the emulation used for MySQL isn't possible |
| Manticore Search | Rename a database | One namespace: there are no databases |
| OceanBase | Rename a database | Pending: the MySQL emulation was not verified against it |
| FerretDB, Amazon DocumentDB | Rename a database | Pending: `renameCollection` across databases was not verified on them |
| SQLite, DuckDB, libSQL / Turso | Rename a database | The file is the database and the connection: rename the file and edit the connection |
| Oracle | Rename a database | Changing the name of a database is an offline DBA operation (`nid`), not SQL |
| BigQuery | Rename a dataset | The driver has no rename for datasets (its rename spec does not offer databases) |
| Cassandra, ScyllaDB | Rename a keyspace | The driver's rename spec does not offer keyspaces |
| Redis, Valkey, Dragonfly | Rename a database | Databases are numbered, they have no name |
| InfluxDB, Neo4j, CouchDB, Firebird, SAP HANA, Databricks (catalogs), Elasticsearch, Amazon DynamoDB and the rest | Rename a database | Their rename spec does not offer database renaming: the engine or its client has no rename for it, or the "database" is another concept (an index, a table, a file) |

## Approximate rows and comments

[Document the database](database-docs.md) shows, below each table,
**Rows (approximate)**, and the comments of views, routines, triggers,
sequences and types. They are fetched by `Session::row_estimates` and
`Session::object_comments` (`crates/dbine-driver/src/stats.rs`); by default
they return empty. Rows come **only from statistics the engine already
keeps**, never from a `COUNT`: it neither scans nor locks. The number may be
out of date until the engine refreshes its statistics. A table without
statistics is left without a figure.

All drivers implement it except Spanner, IoTDB, ksqlDB and InfluxDB 1/2
(their reasons are in the table).

**Tested against real servers:** PostgreSQL 16 and CockroachDB; SQL Server
2022 (including the generic ODBC preset over ODBC Driver 18); Oracle 23;
Firebird; MySQL and MariaDB; ClickHouse and Timeplus; BigQuery (emulator);
Trino (`memory` connector); Dremio, Drill, DSQL (as plain PostgreSQL), Flight
SQL (GizmoSQL) and Phoenix; SQLite and DuckDB (files); libSQL, TDengine and
InfluxDB 3; MongoDB, FerretDB, CouchDB, Elasticsearch and Redis. **Everything
else only has unit tests** and follows the vendor's documentation.

| Driver | Rows (source) | Comments (source) |
|---|---|---|
| `postgres` (PostgreSQL and family) | `pg_class.reltuples` of tables, partitioned tables and materialized views (without `ANALYZE`: -1, or 0 with no pages before PG 14, and it is omitted) | `obj_description` of views, materialized views, sequences, functions, procedures, triggers and types |
| `postgres`: CockroachDB | `estimated_row_count` from `SHOW TABLES` (table statistics; `reltuples` is always null); 0 only if `SHOW STATISTICS` has the table | like PostgreSQL |
| `postgres`: Redshift | `svv_table_info.tbl_rows`, then `pg_class` | like PostgreSQL |
| `postgres`: Yellowbrick | counters from `sys.table`, then `pg_class` | like PostgreSQL |
| `postgres`: CrateDB | documents of the primary shards (`sys.shards`) | none: CrateDB does not store comments |
| `postgres`: RisingWave | keys of each table's and materialized view's state (`rw_catalog.rw_table_stats`) | like PostgreSQL |
| `postgres`: H2 | `information_schema.tables.row_count_estimate` | `REMARKS` |
| `postgres`: Materialize | none: it stores sizes, not rows | `mz_internal.mz_comments` |
| `postgres`: Denodo | none: its views read the sources live | views' description |
| `dsql` | `pg_class.reltuples` (DSQL runs `ANALYZE` by itself; -1 is omitted) | `obj_description` of views, sequences and functions; what DSQL refuses to read is skipped |
| `mysql` (MySQL, MariaDB, TiDB, OceanBase, StarRocks, Doris…) | `information_schema.TABLES.TABLE_ROWS`, the storage engine's estimate (in StarRocks and Doris, the tablet reports); Databend: `system.tables.num_rows`; Manticore: `SHOW TABLE … STATUS` (`indexed_documents`), one table at a time | `ROUTINE_COMMENT` of procedures and functions; `TABLE_COMMENT` of MariaDB sequences and of views (and StarRocks materialized views) when the engine stores one. Triggers have no comments |
| `sqlserver` (and Azure SQL, Fabric) | `sys.dm_db_partition_stats` (needs `VIEW DATABASE STATE`); without that permission, `sys.partitions.rows` | `MS_Description` extended property of views, procedures, functions, triggers, sequences, synonyms and types. Fabric has no extended properties |
| `oracle` | `ALL_TABLES.NUM_ROWS` (`DBMS_STATS` statistics; NULL is omitted); materialized views under the view's name | `ALL_TAB_COMMENTS` (views) and `ALL_MVIEW_COMMENTS`; Oracle does not store comments for PL/SQL units, sequences or synonyms |
| `hana` | `M_TABLES.RECORD_COUNT`; without access, `M_CS_TABLES.RECORD_COUNT` (columnar tables only) | `SYS.VIEWS.COMMENTS` (views); HANA does not store comments for procedures, functions, triggers or sequences |
| `firebird` | Firebird does not store a count: it is estimated as `1 / selectivity` of the unique index statistic (`RDB$INDICES.RDB$STATISTICS`), preferring the primary key. Without a unique index, or with a statistic never computed (0), it is omitted. It is updated with `SET STATISTICS` | `RDB$DESCRIPTION` of views, procedures, functions, packages, triggers, sequences and domains |
| `odbc`: Db2 (LUW) | `SYSCAT.TABLES.CARD` (`RUNSTATS`; -1 = never) | `REMARKS` |
| `odbc`: Db2 for z/OS | `SYSIBM.SYSTABLES.CARDF` (`RUNSTATS`) | `REMARKS` |
| `odbc`: Db2 for i | `QSYS2.SYSTABLESTAT.NUMBER_ROWS` | `LONG_COMMENT` |
| `odbc`: Sybase ASE | `row_count()` (`systabstats`) | tables and columns only |
| `odbc`: SQL Anywhere | `SYS.SYSTAB.count` (updated at each checkpoint) | `remarks` |
| `odbc`: Informix, GBase 8s | `systables.nrows` of the tables `UPDATE STATISTICS` saw | tables and columns only |
| `odbc`: Teradata | `DBC.StatsV.RowCount` (`COLLECT STATISTICS`) | `CommentString` |
| `odbc`: Vertica | `v_monitor.projection_storage.row_count`, the largest projection | `v_catalog.comments` |
| `odbc`: Exasol | `EXA_ALL_TABLES.TABLE_ROW_COUNT` | `*_COMMENT` columns |
| `odbc`: Netezza | `_V_TABLE.RELTUPLES` | `DESCRIPTION` |
| `odbc`: Dameng | `ALL_TABLES.NUM_ROWS` (`DBMS_STATS`) | `ALL_TAB_COMMENTS` |
| `odbc`: MonetDB | `sys.tablestorage.rowcount` | `sys.comments` |
| `odbc`: Ingres | `iitables.num_rows` | tables and columns only |
| `odbc`: SQream | `sqream_catalog.tables.row_count` | none |
| `odbc`: SQL Server (generic preset) | `sys.partitions.rows` | `MS_Description` |
| `odbc`: Hive, Impala, Spark, Kyuubi, Cloudera | none: statistics are in the metastore and require a `DESCRIBE`/`SHOW TABLE STATS` call per table. Explicit pending item: owner's decision | none |
| `odbc`: MaxDB | none: it could not be confirmed that `SYSINFO.TABLESIZE` does not scan data, so it is excluded by the no-scan rule | none |
| `odbc`: IRIS, Caché, OpenEdge, Mimer and the minor presets | none: no reliable catalog source is known | none |
| `sqlite`, `libsql` | `sqlite_stat1`, only if it exists (created by `ANALYZE`); without that table there is no statistic and the answer is empty. `ANALYZE` is never run, because it writes to the file | none: SQLite has no comments on objects |
| `duckdb` | `duckdb_tables().estimated_size` (storage metadata); tables of attached catalogs from other engines do not have it | `COMMENT ON` of views, macros, sequences and types |
| `clickhouse` (and Timeplus Proton) | `system.tables.total_rows`; NULL (Log, external tables, views) is omitted | `COMMENT` of views, materialized views and dictionaries |
| `trino` (and Presto) | `SHOW STATS FOR` per table, `row_count` of the summary row: it asks the connector for the statistics it stores (Hive metastore, Iceberg snapshot summary, Delta log…). At most `MAX_TABLES` tables; without connector statistics it is omitted | `system.metadata.table_comments` (views) and `system.metadata.materialized_views`; Trino has no function comments |
| `athena` | `numRows` (Hive/Spark `ANALYZE`) or `recordCount` (Glue crawler) in the catalog table's parameters, with `ListTableMetadata`: free, does not bill or read S3. Athena's `ANALYZE` only stores column statistics | the views' `comment` parameter; Athena writes "Presto View" there (discarded) and has no `COMMENT` for views, so only those created from Hive or Spark come out |
| `bigquery` | `numRows` from `tables.get` (REST, no query job, no cost) for tables, snapshots, clones and materialized views; views and external tables do not have it | `description` of views, materialized views, functions and procedures. Each object is read separately, up to `MAX_OBJECTS` |
| `databricks` | `spark.sql.statistics.numRows` property through Unity Catalog's REST API (it wakes no warehouse); left by `ANALYZE TABLE … COMPUTE STATISTICS` or predictive optimization. Without `ANALYZE`, it is omitted. The `hive_metastore` catalog is not in the API: empty | `comment` of views, materialized views and functions |
| `snowflake` | `rows` column of `SHOW TABLES` and `SHOW MATERIALIZED VIEWS` (micro-partition metadata; no warehouse, no billing). External tables do not have it | views, materialized views, functions, procedures, sequences, streams and tasks; the fixed `description` text when there is no comment is discarded |
| `dremio` | none: neither `INFORMATION_SCHEMA` nor the catalog API bring counts, and counting would run a job that reads the source | views' description (wiki) through the catalog API, up to `MAX_VIEWS`; no jobs |
| `drill` | `NUM_ROWS` of `INFORMATION_SCHEMA.TABLES`, which comes from Drill's metastore once `ANALYZE TABLE … REFRESH METADATA` has run; without a metastore it is NULL and omitted; before Drill 1.17 the column does not exist | none: Drill has no `COMMENT` |
| `flightsql` | only if the server is DuckDB (GizmoSQL): `duckdb_tables().estimated_size`; Dremio, DataFusion and others: none, because Flight SQL's metadata commands (`GetTables`) bring no counts | only with DuckDB (GizmoSQL): `comment` of `duckdb_views()`; Flight SQL does not list functions or sequences |
| `phoenix` | `SYSTEM.STATS` (guide posts from `UPDATE STATISTICS` and major compactions), sum of `GUIDE_POSTS_ROW_COUNT`; it falls short by the rows after the last guide post (300 MB by default); a small table or one without statistics is omitted; views share storage and have none. Generic Avatica: empty | none: Phoenix has no `COMMENT` |
| `cassandra` (and ScyllaDB) | the node's partition estimates (`system.table_estimates` in Cassandra 4.0+, `system.size_estimates` before and in ScyllaDB), extrapolated to the ring by the fraction of ranges the node covers. **They count partitions, not CQL rows**: a table with clustering columns has more rows. Amazon Keyspaces has neither table: none | `comment` of tables (comes with the schema) and materialized views (`system_schema.views`); types and functions have none |
| `influxdb`: v3 | rows of the Parquet files already persisted (`system.parquet_files`), without reading data; **points still in the WAL (the last few minutes) do not add up yet**; a token without access to the system tables gets nothing | none: InfluxDB has no comments |
| `influxdb`: v1 and v2 | none: the engine stores no row statistics | none: Flux and InfluxQL have no comments |
| `tdengine` | none: `SHOW TABLE DISTRIBUTED` would give them without reading data, but it costs more the more tables grow and is left out by the rule of not loading the server. Explicit pending item: owner's decision | `ins_tables.table_comment` of subtables (tables and supertables already bring theirs); views, streams and topics have none |
| `mongodb` (and FerretDB, DocumentDB) | `estimatedDocumentCount` per collection (metadata, no scan); views and collections the user cannot count are omitted | none: MongoDB stores no comments on collections, views or indexes |
| `cosmosdb` | `documentsCount` from `x-ms-resource-usage` (with `x-ms-populatequotainfo`); the service updates it every few minutes; -1 while it does not know. A `COUNT` would read all items and cost RUs | none |
| `couchbase` | `kv_collection_item_count` gauge per collection, from the statistics REST API (`/pools/default/stats/range`, Couchbase Server 7.0+), summed across nodes; earlier versions or without the privilege: none | none: it stores no comments |
| `couchdb` | `doc_count` from `GET /{db}` for `_all_docs`; views only report the index size | none |
| `dynamodb` | `ItemCount` from `DescribeTable`, of the table and its secondary indexes; the service refreshes it about every six hours, so it lags behind recent writes. A `Scan` with `COUNT` would read and bill the whole table | none: tags are not comments |
| `elasticsearch` (and OpenSearch, Open Distro) | `docs.count` from `_cat/indices` (primary documents; it also counts nested ones); a data stream adds up its backing indexes; closed indexes and aliases are omitted | none |
| `solr` | `index.numDocs` from `admin/cores?action=STATUS`. In SolrCloud the collection is only reported if all its shards have a replica on the node DBine connects to | none |
| `orientdb` | `records` of each class in the database metadata (`GET /database/{db}`); it is polymorphic: it includes the subclasses' records | classes and their properties already bring their description with the schema; functions, sequences and indexes have none |
| `neo4j` | nodes per label and relationships per type, from the count store (only the two exact forms of `count` that the planner answers without touching data). Memgraph: `count` of `SHOW INDEX INFO` on label-only or type-only indexes; a label without an index has no figure. Neptune: none, because its statistics summary gives graph totals and names, not a count per label | none: Neo4j, Memgraph and Neptune store no comments |
| `redis` (and Valkey, Dragonfly) | number of keys in the logical database (`keys=` of `INFO keyspace`), **per database and not per collection**: it does not appear under any table in the document | none |
| `etcd` | `etcd_debugging_mvcc_keys_total` gauge from `/metrics`, of the **whole keyspace** (etcd keeps no count per prefix): it does not appear under any table in the document; a connection limited to a prefix, or without access to `/metrics`, gets nothing | none |
| `spanner` | none: it stores no row counts (`SPANNER_SYS` only reports sizes in bytes) | none: GoogleSQL has no `COMMENT` |
| `iotdb` | none: the schema stores no counts per device, and counting would be a `SELECT COUNT(*)` over the TsFiles, which the no-scan rule forbids | none: it has no comments |
| `ksqldb` | none: it exposes no message count per stream or table | none: it has no comments |

**Explicit pending item:** TDengine (rows) and Hive, Impala, Spark, Kyuubi and
Cloudera through ODBC (rows) await the owner's decision.

## Modify tables (designer in edit mode)

**Modify…** opens the designer with the table as it is, and builds a single
script with the engine's `ALTER` (the same one as **Compare schemas**, `Driver::sync_script`).
How to use it: [`schema-compare.md`](schema-compare.md#modify-a-table).
The explorer offers it when the driver has a designer (`designer`), answers
`supports_schema_sync`, the object's type is the designer's (table, collection,
index, stream…) and the connection is not read-only.

All engines that meet those two conditions have it, that is, those in
this document's "Sync" tables that also have a designer.
What can change in each one is what those tables say: what the engine does not
apply with DDL is left as a warning in the script and is not run (for example,
a column's type in Cassandra, or the fields of documents that already exist in
MongoDB).

**Tested against real servers:** SQL Server, PostgreSQL, CockroachDB,
MySQL, MariaDB, SQLite, Oracle, ClickHouse, Cassandra and MongoDB. **Everything
else only has unit tests** and follows the vendor's documentation.

Renaming a column inside the designer goes through **Rename…**
([`rename.md`](rename.md)). In engines whose `rename_spec` does not cover
columns, an existing column's name is fixed in the designer and the reason is
shown (see "Rename with impact"). The table's name and schema are not changed
here: that is **Rename…**.

| Engine | What is missing | Reason |
|---|---|---|
| Denodo | Modify… | It is a virtual layer: it has no designer (`designer` returns `None`) or table DDL; views are defined in Denodo. |
| Apache Calcite Avatica (Phoenix in generic mode) | Modify… | No designer or sync: the DDL depends on the database behind the server and the driver does not know which one it is. |
| Amazon Neptune | Modify… | No designer or sync: it has no user-defined schema, indexes everything by itself and has no constraints. |
| NetSuite (ODBC) | Modify… | SuiteAnalytics Connect is read-only: it has no designer or sync. |
| InfluxDB (v1, v2, v3) | Modify… | No designer or DDL: measurements and their fields are created when data is written. |
| Apache Drill | Modify… | No designer or sync: its tables are files created with `CREATE TABLE AS`, with no columns to modify. |
| Apache Arrow Flight SQL | Modify… | It is a protocol, not an engine: the DDL depends on the backend and there is no portable one. Connect with the database's own driver. |
| CouchDB | Modify… | It has sync (indexes and design docs), but no designer: documents have no schema, so there is no table to open. |
| Redis, Valkey, Dragonfly | Modify… | They have a key designer, but no sync: keys have no schema to alter. |
| etcd | Modify… | It has a key designer, but no sync: it has no schema, only keys with values. |
| Cosmos DB | Changing an existing container | The sync only creates and drops containers: the partition key, unique keys, index policy, TTL and RU/s are changed from the portal or the Azure CLI, not with SQL. The script warns and does not run. |
| Cassandra, ScyllaDB, Keyspaces | Changing a column's type or the primary key | The engine has no `ALTER` for that: the table has to be recreated. The script warns and does not run. |
| MongoDB, FerretDB, DocumentDB | Changing the fields of documents that already exist | Documents have no columns: they are modified by rewriting each one. The validator, indexes and options are changed; fields are left as a warning. |
| Elasticsearch, OpenSearch | Changing the type or dropping a field, changing shards | The mapping is not modified in place: reindexing is needed. The script warns. |
| Explicit pending item | Modify… in the engines where it was not tested | Only the ten engines above were tested against a server. The rest still have to be tested: there are no containers or emulators for all of them. |
