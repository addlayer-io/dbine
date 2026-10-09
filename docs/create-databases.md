# Create databases

"New database…", in a connection's menu, opens a dialog with the name and,
for the engines that have them, the advanced options of that engine's
`CREATE DATABASE`.

## The dialog

- **Name:** required.
- **Advanced options:** collapsed by default. It only appears for engines that
  have options; for the rest the dialog asks for the name only.
- **Server suggestions:** when you open the options, DBine asks the server
  what it can offer (collations, default folders, users, tablespaces,
  clusters, regions…) and shows it as a list. The field still accepts any
  other value.
- **Empty field = the server default.** If there's a known value, it's shown
  as "Default: …". With all options empty, the database is created the usual
  way, with just the name.
- **Show script:** shows exactly what "Create" is going to run. It can be
  opened in a query with "Open in a query".
- **Create:** runs as a background task, like the rest of the long
  operations.

Each value is validated before the script is built, and names and literals
are escaped: what you type in a field can't escape its clause.

## Post-creation steps

In some engines an option doesn't fit in the `CREATE DATABASE` and is applied
afterwards with another statement (for example, the recovery model, the
compatibility level and the owner in SQL Server, or the default character set
in Firebird). "Show script" shows all of them, in order.

If a later step fails, **the database already exists**. The error says so:
"the database … was created, but this step failed: …", with the statement
that failed and the server's message. The creation isn't rolled back.

## What each engine offers

### SQL Server family

- **SQL Server:** collation, owner, recovery model, compatibility level, and
  the data and log files (folder, initial size, growth and maximum size of
  each one).
  - The folder goes in `CREATE DATABASE … ON / LOG ON`, which needs both
    parts: **the log folder needs the data folder**.
  - Sizes without a folder are applied afterwards with `ALTER DATABASE …
    MODIFY FILE`, on the files the server named after the database (`name`,
    `name_log`).
  - Recovery model, compatibility level and owner are later `ALTER`s.
- **Azure SQL:** collation, edition, service objective, maximum size or
  elastic pool. Not verified against a real server.
- **Microsoft Fabric and Babelfish:** no options (Fabric is created from its
  portal; Babelfish's T-SQL has none).

### PostgreSQL family

- **PostgreSQL and those that keep its `CREATE DATABASE`** (TimescaleDB, EDB,
  Fujitsu, AlloyDB, Cloud SQL, Aurora): owner, template, encoding, locale
  provider (15+), `LC_COLLATE`, `LC_CTYPE`, ICU locale (15+), builtin locale
  (17+), tablespace, connection limit and `IS_TEMPLATE` (9.5+). Clauses the
  connected server doesn't have because it's an older version are rejected
  before running, with the version they require.
- **KingbaseES, Greenplum, Cloudberry and Greengage:** the same, without the
  locale provider (their PostgreSQL base doesn't have it).
- **YugabyteDB:** no tablespace (theirs place tables, not databases), plus
  `COLOCATION`.
- **openGauss:** no `IS_TEMPLATE`, plus `DBCOMPATIBILITY` (the SQL dialect).
- **CockroachDB:** owner and the multi-region clauses (`PRIMARY REGION`,
  `REGIONS`, `SURVIVE … FAILURE`). `ENCODING` and `CONNECTION LIMIT` only
  accept their default values, so they aren't offered.
- **Redshift:** owner, connection limit, `COLLATE CASE_SENSITIVE` /
  `CASE_INSENSITIVE` and isolation level. Not verified against a real server.
- **RisingWave:** owner, resource group, barrier interval and checkpoint
  frequency.
- **Yellowbrick:** owner, encoding (`UTF8` or `LATIN9`), connection limit and
  `HOT_STANDBY`. Not verified against a real server.
- **Materialize:** no options (`CREATE DATABASE` only takes the name).

### MySQL family

- **MySQL (Aurora, Cloud SQL), MariaDB, TiDB and OceanBase:** character set
  and collation. MariaDB adds a comment and TiDB a placement policy.
- **SingleStore:** number of partitions.
- **StarRocks:** storage volume (`storage_volume`) and other `key=value`
  properties. **Doris / VeloDB:** replicas (`replication_num`) and other
  `key=value` properties.
- **GreptimeDB:** default retention of its tables (`WITH (ttl)`).
- **Databend:** no options (its `ENGINE` has a single useful value).

### Oracle, SAP HANA, Firebird and over ODBC

- **Oracle:** here a "database" is a schema, that is, a user without
  authentication (`CREATE USER … NO AUTHENTICATION`). The options are its
  storage: default tablespace, temporary tablespace and quota on the default
  tablespace (unlimited if left empty). With no tablespace chosen, the quota
  goes to the database's default permanent tablespace, which only the server
  knows: the script is an anonymous block that reads it.
- **SAP HANA:** a connection's "databases" are schemas, and `CREATE SCHEMA`
  takes a single option: the owner (`OWNED BY`). Tenant databases are created
  from the system database's cockpit, not from a connection to a tenant. Not
  verified against a real server.
- **Firebird:** the database is a file the server creates. The options are
  the folder (together with the name it forms the file path), the page size
  and the default character set, which is applied with an `ALTER DATABASE …
  SET DEFAULT CHARACTER SET` right after (Firebird 3+). The script shows the
  `CREATE DATABASE` and the `ALTER` separated by `;`.
- **Sybase ASE (ODBC):** data and log devices with their sizes (`ON device =
  size`, `LOG ON …`). If both share a device it uses `WITH OVERRIDE`, as ASE
  requires. It runs from `master` and the session goes back to its database
  afterwards, even if the `CREATE` fails. Not verified against a real server.
- **Netezza (ODBC):** query history (`COLLECT HISTORY`) and version retention
  in days (`DATA VERSION RETENTION TIME`). Not verified against a real
  server.
- **Generic ODBC:** no options; the engine behind it isn't known.

### Analytics and cloud

- **ClickHouse:** the database engine (`Atomic`, `Replicated` with its Keeper
  path, shard and replica, `Memory`), `ON CLUSTER` and comment. `Lazy` isn't
  offered because recent servers (26.x) removed it, and the engines that
  mirror another server (MySQL, PostgreSQL, S3…) ask for a connection, not
  options. With `ON CLUSTER` the response from each host is read to the end,
  so a host that fails shows up.
- **Snowflake:** `TRANSIENT`, Time Travel days, maximum retention extension,
  default collation and comment. Not verified against a real server.
- **Databricks:** a "database" is a Unity Catalog catalog. Managed location
  (`MANAGED LOCATION`, with the external locations the user sees as a
  suggestion) and comment. Not verified against a real server.
- **Athena:** comment, S3 location (`LOCATION`) and `DBPROPERTIES`
  (`key=value` lines). Not verified against a real server.
- **BigQuery:** a "database" is a dataset. **The script isn't SQL: it shows
  the API call** (`datasets.insert`) with its body. Options: location,
  default table expiration, description, labels and default collation. The
  session adds the project and, if no location is chosen, the connection's.
- **Cloud Spanner:** version retention period and default leader, as
  `ALTER DATABASE … SET OPTIONS` that run together with the `CREATE DATABASE`
  in a single atomic operation: if one fails, no database is left. The
  PostgreSQL dialect isn't offered (see
  [`engine-support.md`](engine-support.md#create-databases-options)).

### Documents, graphs, time series and keys

- **Cassandra and ScyllaDB (keyspace):** replication class
  (`NetworkTopologyStrategy` by default, or `SimpleStrategy`), replication
  factor or one per datacenter, and `durable_writes`. ScyllaDB adds
  `tablets`. Datacenters are suggested from `system.local` and
  `system.peers`. With no options, the creation is the usual one:
  `NetworkTopologyStrategy` with one replica per datacenter. ScyllaDB not
  verified against a real server.
- **Amazon Keyspaces:** `SingleRegionStrategy` or `NetworkTopologyStrategy`
  with the regions (Keyspaces always keeps three replicas per region). Not
  verified against a real server.
- **Couchbase (bucket):** **the script isn't SQL: it shows the call**
  `POST /pools/default/buckets` with its form, which is what gets sent.
  Options: bucket type, memory (the cluster's free memory is suggested),
  replicas, eviction policy, minimum durability, storage, maximum document
  lifetime and allow flush. With no options, the bucket is 100 MB with no
  flush.
- **CouchDB:** shards (`q`), replicas (`n`) and whether it's partitioned, as
  `PUT /{db}` parameters. Cluster values are suggested only if the user is an
  administrator.
- **OrientDB:** the storage type. The database type isn't asked: it remains
  `graph`, which in OrientDB 3 has the same classes (`V`, `E`) as a document
  database.
- **InfluxDB 1 (InfluxQL):** default retention policy of `CREATE DATABASE …
  WITH DURATION … REPLICATION … SHARD DURATION … NAME …`.
- **InfluxDB 2 (buckets):** retention, shard group duration and description,
  in the body of `POST /api/v2/buckets`. The organization id is looked up by
  the session on creation, so **the script shows it as the text `(the id of
  the connection's organization)`** and not as an id.
- **InfluxDB 3:** retention period of `POST /api/v3/configure/database`.
  Durations are validated (`30d`, `1h30m`, `INF` where applicable).
- **IoTDB:** properties of `CREATE DATABASE root.x WITH …`: TTL, time
  partition interval, number of schema and data region groups, and, in
  IoTDB 1.x, the replication factors (IoTDB 2 takes them only from the
  cluster configuration and rejects them here). Durations accept a unit
  (`7d`, `12h`) or milliseconds.
- **TDengine:** the parameters of TDengine 3's `CREATE DATABASE`: time
  precision, retention (`KEEP`), days per file, replicas, vgroups, memory
  (`BUFFER`, `PAGES`, `PAGESIZE`), last-value cache, WAL, compression, block
  sizes, `STT_TRIGGER` and `SINGLE_STABLE`.
- **Neo4j** (Enterprise, the edition that creates databases): topology
  (primaries and secondaries) and the `OPTIONS` map (storage format,
  transaction log enrichment and a backup or dump to seed from). It runs in
  `system`.
- **Cosmos DB:** throughput shared by the database's containers, manual or
  autoscale, as `POST /dbs` headers. Without it, each container has its own,
  as before. Serverless accounts reject provisioned throughput and the
  server's error comes back as is. Not verified against a real server.

## Contract

- `Driver::create_database_fields()` returns the options (`Field`), in order,
  with their `key`. Empty: the engine only takes the name. It travels in
  `DriverInfo` as `create_database_fields`.
- `Driver::create_database_script(name, options)` returns the code that "Show
  script" shows; by default, `Error::Unsupported`.
- `Session::create_database_choices()` returns the server's suggestions
  (`FieldChoices`: `key`, `default`, `values`).
- `Session::create_database_with(name, options)` creates the database.
  `options` is `key` → value; empty or absent is the server default. With all
  options empty it calls `Session::create_database`; if the engine doesn't
  implement the options, it returns `Error::Unsupported`.

The Tauri commands are in [`api-commands.md`](api-commands.md#databases).
What each engine supports is in
[`engine-support.md`](engine-support.md#create-databases-options).
