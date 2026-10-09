# Database properties

Shows what the engine reports about a database (size, encoding, state,
configuration) and lets you **change what the engine allows changing**,
always showing the script and the warnings about what the change causes
first.

## Where it is

Right-click a **database** › **Properties…**. It appears only on engines that
have the capability (`database_properties`).

## The dialog

- **Tabs:** **General** and then a group for each set of options the engine
  has (for example, in SQL Server: recovery, automatic options, files…). If
  there is a single group, no tabs are shown.
- At the top of each tab, the **read-only data** (size, creation date,
  number of objects, versions…). Below, the **editable fields** with their
  current value. Fields the engine can fill in (collations, locations,
  users…) offer suggestions from the server.
- **View script:** shows the script of what you changed, with **Open in a
  query** to review it or run it separately. It includes only what differs
  from what was read.
- **Apply:** asks for confirmation with the script and the **warnings** about
  changes that disturb the database or its users (for example, that they
  close open sessions or take it offline). Only then does it run it.
- On a **read-only connection** properties can be seen but not changed
  (**Read-only connection…**).
- If the engine has nothing to show or change, it says so.

Properties are read with the connection's server-level session, like
creating and dropping databases: some changes can't be run from inside the
database itself (for example, PostgreSQL's tablespace).

## Engine particularities

Each engine shows and changes its own, and **offers only what the server
reports**: an option from a newer version appears only if it exists.

| Engine | What there is |
|---|---|
| SQL Server, Azure SQL | Owner and collation; recovery, compatibility, `PAGE_VERIFY`, target recovery time, delayed durability; `AUTO_*` options; state, access and read-only; snapshot isolation; ANSI options, `TRUSTWORTHY` and `DB_CHAINING`; size, growth and maximum of each file. In Azure SQL Database, the service (edition, objective, maximum size). Changes that need the database to themselves run with `ROLLBACK IMMEDIATE` and the warning says so beforehand. |
| PostgreSQL and its family (TimescaleDB, EDB, Fujitsu, AlloyDB, Cloud SQL, Aurora, KingbaseES, Greenplum, YugabyteDB, openGauss) | Owner, connection limit, `ALLOW_CONNECTIONS`, `IS_TEMPLATE`, tablespace, `REFRESH COLLATION VERSION`, comment and the per-database defaults (`SET` / `RESET`). Data: size, connections, encoding and locale, collation versions, XID age. |
| CockroachDB | Owner, regions, survival goal, placement, comment and defaults. |
| Redshift, Yellowbrick, RisingWave, Materialize | Redshift: owner, connection limit, case sensitivity, isolation and comment. Yellowbrick: owner, limit, `ALLOW_CONNECTIONS`, `HOT_STANDBY`, read-only and maximum size. RisingWave: owner, resource group, barrier interval and checkpoint frequency. Materialize: owner and comment. |
| MySQL, MariaDB, TiDB, OceanBase | Default character set and collation (in a single `ALTER`); depending on version, `READ ONLY`, `DEFAULT ENCRYPTION` (self-managed installations only: it needs a keyring), comment (MariaDB 10.5+), placement policy (TiDB). Data: size, tables and views. |
| SingleStore, StarRocks, Doris, GreptimeDB | Synchronous or asynchronous replication; data and replica quotas (StarRocks, Doris; Doris adds the transaction quota and `SET PROPERTIES`); default TTL (GreptimeDB). |
| Oracle | The "database" is a schema (an account): its data comes from `DBA_USERS`, `DBA_SEGMENTS` and `DBA_TS_QUOTAS`; you change the default and temporary tablespace, the quota per tablespace, the profile and the account lock. It needs the DBA views and the `ALTER USER` privilege. |
| Snowflake | Owner and comment; Time Travel; options (DDL collation, quoted identifiers, serialization policy, catalog and external volume); tasks and logging. Only the parameters the account lists. Clearing a parameter returns it to the account's value. |
| BigQuery | A *dataset*: description; default expiration of tables and partitions, collation, case-insensitive names, rounding mode; *time travel* window and billing model; labels. All in a single call, which is applied completely or not at all. |
| Databricks | A catalog: comment, predictive optimization and owner. The storage root, isolation and type are shown, not changed. |
| Spanner | Deletion protection, default leader, time zone, version retention, optimizer version, statistics package and default sequence kind. PostgreSQL-dialect databases are shown but not changed. |
| ClickHouse | The comment and, on database engines that support it (MaterializedPostgreSQL, DataLakeCatalog), each `SETTINGS` option. Timeplus Proton has no `ALTER DATABASE`. |
| Firebird | Data from `MON$DATABASE`; the default character set and the comment are changed, and depending on version the *linger*, `SQL SECURITY` and replication publication. Read-only, forced writes and *sweep* interval are shown but not offered. |
| Cassandra, ScyllaDB | Replication (class, factor or per-datacenter factor) and `durable_writes`. In ScyllaDB with *tablets*, only `NetworkTopologyStrategy`, and if it uses *tablets* it can't be changed. |
| Amazon Keyspaces | Adding a region (it enables client-side timestamps, as AWS requires). |
| MongoDB | `dbStats` counters and the *profiler* level (per database); `slowms` and `sampleRate` belong to the whole server. |
| FerretDB, Amazon DocumentDB | Only `dbStats` (FerretDB has no `profile`; DocumentDB defines it in the cluster parameter group). |
| CouchDB | Documents, sizes, cluster and sequences; `_revs_limit`, `_purged_infos_limit` and the `_security` object are changed (replaced whole). |
| Couchbase | A *bucket*: RAM quota, replicas, flush, ejection, durability, maximum TTL, compression and more, only if the server reports it. Type, storage engine and conflict resolution are read-only. |
| Azure Cosmos DB | The database's shared throughput (manual or autoscale). With per-container throughput or *serverless*, it is only viewed. |
| Neo4j | Data from `SHOW DATABASE` and counts. In Enterprise, access, topology, transaction log enrichment and Cypher version are changed; Community, data only. |
| Memgraph | Data from `SHOW STORAGE INFO`; the storage mode and isolation have their own statements outside this dialog. |
| OrientDB | Time zone, locale, charset, date formats, cluster selection, conflict strategy, validation, strict SQL and own attributes. |
| InfluxDB | 1: retention policies (duration, shard group, replication, default). 2: retention, shard group duration and the bucket's description. 3: retention period (from 3.2). On HTTP versions the "script" is the request that is sent. |
| IoTDB | The whole database's TTL and the number of schema and data region groups. |
| TDengine | The `ALTER DATABASE` parameters the server reports (WAL, replicas, shared storage…); they vary by version. |
| Athena | The database properties (`DBPROPERTIES`), changed or new in one statement; the description is only viewed. |
| Dremio | Of a source: metadata refresh and reflection policies. Spaces and homes only show data. |
| ODBC | Sybase ASE: owner, size and `sp_dboption` options. Netezza: owner, default schema, query history and time-travel retention. Informix and GBase 8s: data only (the logging mode is changed with engine tools, not with SQL). |
| DuckDB, SQLite, libSQL | DuckDB: data only (settings belong to the instance or the session). SQLite: `user_version`, `application_id`, the journal (`DELETE` or `WAL`), the page size and *auto vacuum* (these two are applied with `VACUUM`). libSQL: only `user_version`. |
| Redis, Valkey, Dragonfly | Only `INFO keyspace` data: they keep no per-database settings (`CONFIG SET` belongs to the whole server). |
| SAP HANA | The "database" is a schema and HANA has no `ALTER SCHEMA`: data only (owner, creation, number of objects and size in memory and on disk). |
| Databend, Manticore, Amazon Neptune, Denodo, CrateDB, H2 | No properties. Databend: its `ALTER DATABASE` only renames. Manticore: it has no databases. Neptune: settings live in the cluster parameter group (AWS API), not behind openCypher. Denodo, CrateDB and H2: they have no databases that DBine manages. |

Engines not in the list don't offer **Properties…**.

## Contract

Three methods, behind the `database_properties` capability:

- `database_properties(database)` returns a `DatabaseProperties`: the
  `fields` (the editable fields), their current `values`, the read-only
  information (`info`), the server's suggestions (`choices`) and per-field
  warnings (`warnings`).
- `Driver::alter_database_script(database, changes)` generates the script of
  what changed (`changes` is field → new value).
- `Session::alter_database(database, changes)` applies it.

Commands: `database_properties`, `alter_database_script` and `alter_database`
(rejected on read-only connections). See [`api-commands.md`](api-commands.md).
