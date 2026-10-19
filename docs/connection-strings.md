# Copy connection string

**Copiar cadena de conexión…** writes a saved connection as the connection
string an application uses, to paste it into the app's configuration or
secret store instead of building it by hand. It is on the connection's
context menu in the explorer and in the footer of the connection form when
editing a saved connection.

The dialog asks for:

- **Database**: the connection's by default; any other (the list of the
  connected server, or typed).
- **Format**: the ones that make sense for the engine (table below).
- **Incluir contraseña**: off by default. Off, the string carries a
  placeholder where each secret goes; on, the preview shows it masked until
  **Mostrar** is pressed, and **Copiar** copies the real value.

The preview updates as the options change. Under it, notes about the
connection (an SSH tunnel, a cloud IAM token, a secret manager) and about
the format (JDBC properties, the ODBC driver's name, a port other than
DBine's).

## Secrets

- The string is built in the backend (`src-tauri/src/connstring`). The
  preview is built from the connection's non-secret fields only: the
  password and every secret field (tokens, keys, client secrets, saved
  connection strings) are never read for it, whether or not they are saved.
- With **Incluir contraseña**, **Mostrar** and **Copiar** resolve the
  secrets the way a connect does: the keychain, the field's secret source
  with its confirmation (an imported definition asks before it runs, a
  master password set to "ask" is asked and kept in memory only), or a
  password typed then if the connection doesn't keep it. A password typed
  for the copy is used for that string only.
- The resolved text goes to the clipboard only: it is not logged, kept in
  history, or sent to telemetry, MCP, the AI assistant or the phone. The
  commands are not exposed to MCP or the assistant.
- After copying with a password the toast says so, and 60 seconds later the
  clipboard is cleared if it still holds that text (when the system lets
  DBine read the clipboard back; otherwise it is left as it is).
- **Vault database credentials** (`vault_db` source) are leased and revoked
  when DBine disconnects: they are not copied (the toggle is disabled and
  the command refuses), so no lease is kept alive for a string.
- **Cloud IAM sign-in** (AWS IAM, Google Cloud IAM and Entra ID for
  PostgreSQL and MySQL, Aurora DSQL, Neptune with IAM): the password is a
  temporary token DBine generates when it connects. The string carries
  `${DB_IAM_TOKEN}` and the toggle is disabled.

Placeholders: `${DB_PASSWORD}`, `${DB_TOKEN}`, `${DB_CLIENT_SECRET}`,
`${DB_ACCOUNT_KEY}`, `${DB_API_KEY}`, `${DB_AUTH_TOKEN}`,
`${DB_WALLET_PASSWORD}`, `${DB_CONNECTION_STRING}`,
`${AWS_SECRET_ACCESS_KEY}`, `${DB_IAM_TOKEN}`, and for files
`${PRIVATE_KEY_FILE}` and `${GOOGLE_APPLICATION_CREDENTIALS}`. They are
written as they are (not escaped), so they can be replaced by hand or
expanded by the environment.

## The real server

A connection behind an **SSH tunnel** is written with the database server's
host and port, not the local end of the tunnel: from another machine the
same tunnel is needed (the dialog says so). The tunnel's credentials are
never in the string.

## Escaping and TLS

Each value is escaped for its format:

- URIs: user, password, database and parameters percent-encoded.
- ADO.NET: a value with `;`, `=`, quotes, braces or edge spaces in double
  quotes (`"` doubled), or in single quotes when it only has `"`.
- ODBC: a value with `;`, `=`, braces or edge spaces in braces (`}` doubled).
- libpq: a value with spaces, `'` or `\` in single quotes, escaped.
- JDBC: query parameters percent-encoded (PostgreSQL, MySQL, MariaDB);
  SQL Server's properties in braces. Where the driver takes the user and
  password as connection properties, they go on separate `user=` and
  `password=` lines (Java properties escaping) under the URL.
- Node.js configuration objects: JSON.

TLS follows the connection: with **encrypt** the server's certificate is
checked unless the connection trusts it, as DBine does: `sslmode=verify-full`
or `require` (libpq, pgjdbc; Npgsql `VerifyFull` / `Require`), `ssl-mode` /
`sslMode` / `SslMode` (MySQL family), `tls=true` or `skip-verify` (Go),
`Encrypt` and `TrustServerCertificate` (SQL Server; Azure SQL and Fabric
always encrypt), `rediss://`, `tls=true` (MongoDB), `https` (HTTP engines),
`neo4j+s` / `+ssc` (Bolt). SQL Server's sign-in methods map to each format's
own keyword (`Authentication=Active Directory …`,
`authentication=ActiveDirectory…`, `Integrated Security`,
`Trusted_Connection`, `fedauth`, tedious' `authentication.type`).

The SQLAlchemy URL is the one the notebook's Python export builds
([`notebooks.md`](notebooks.md)), rendered as a string, with the TLS mapping
above.

## Formats per engine

ODBC strings use the driver name the connection has (or the engine's
default) and work with any ODBC client (pyodbc, `System.Data.Odbc`,
node-odbc…). Where a format doesn't apply to an engine, the reason is in
[`engine-support.md`](engine-support.md#copy-connection-string).

| Engine | Formats |
|---|---|
| Actian Ingres | ODBC |
| Actian Zen (Pervasive PSQL) | ODBC; JDBC |
| AlloyDB para PostgreSQL | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Altibase | ODBC; JDBC |
| Amazon Athena | JDBC (Athena JDBC 3); ODBC; Python (SQLAlchemy) |
| Amazon Aurora DSQL | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Amazon Aurora MySQL | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Amazon Aurora PostgreSQL | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Amazon DocumentDB | URI (mongodb://) |
| Amazon DynamoDB | AWS SDK · JSON |
| Amazon Keyspaces | Node.js (cassandra-driver) · JSON; JDBC (cassandra-jdbc-wrapper) |
| Amazon Neptune | URI (Bolt); Node.js (neo4j-driver) · JSON |
| Amazon Redshift | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Apache Calcite Avatica | URL (Avatica); JDBC (thin client); Python (SQLAlchemy) |
| Apache Cassandra | Node.js (cassandra-driver) · JSON; JDBC (cassandra-jdbc-wrapper) |
| Apache Cloudberry | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Apache Doris | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Apache Drill | URL (REST); JDBC (drill-jdbc); Python (SQLAlchemy) |
| Apache Hive | ODBC; JDBC; Python (SQLAlchemy) |
| Apache Ignite 2 | ODBC; JDBC |
| Apache Ignite 3 | ODBC; JDBC |
| Apache Impala | ODBC; JDBC; Python (SQLAlchemy) |
| Apache IoTDB | URL (REST); JDBC (iotdb-jdbc) |
| Apache Kyuubi | ODBC; JDBC; Python (SQLAlchemy) |
| Apache Phoenix | URL (Avatica); JDBC (thin client); Python (SQLAlchemy) |
| Apache Solr | URL |
| Archivos CSV / Parquet / JSON | File path |
| Archivos dBase (DBF) | ODBC |
| Arrow Flight SQL | URI (Arrow Flight SQL); JDBC (Arrow Flight SQL) |
| Azure Cosmos DB | Azure Cosmos DB |
| Azure Databricks | JDBC (Databricks); ODBC; Python (SQLAlchemy); Go (databricks-sql-go); Node.js (@databricks/sql) · JSON |
| Azure SQL Database | ADO.NET (Microsoft.Data.SqlClient); JDBC (Microsoft JDBC Driver); ODBC; Python (SQLAlchemy); Node.js (mssql, tedious) · JSON; Go (go-mssqldb) |
| Babelfish for PostgreSQL | ADO.NET (Microsoft.Data.SqlClient); JDBC (Microsoft JDBC Driver); ODBC; Python (SQLAlchemy); Node.js (mssql, tedious) · JSON; Go (go-mssqldb) |
| ClickHouse | URL (HTTP); ADO.NET (ClickHouse.Client); JDBC (clickhouse-jdbc); Python (SQLAlchemy, clickhouse-connect); Node.js (@clickhouse/client) · JSON |
| Cloud SQL para MySQL | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Cloud SQL para PostgreSQL | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Cloudera CDP (Hive por HTTP) | ODBC; JDBC |
| CockroachDB | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Couchbase | URI (couchbase://); Node.js (couchbase) · JSON |
| CouchDB | URL |
| CrateDB | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| CUBRID | ODBC; JDBC |
| Dameng (DM) | ODBC; JDBC |
| Databend | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Databricks SQL | JDBC (Databricks); ODBC; Python (SQLAlchemy); Go (databricks-sql-go); Node.js (@databricks/sql) · JSON |
| Denodo | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Dragonfly | URI (redis://); ADO.NET (StackExchange.Redis) |
| Dremio | URI (Arrow Flight SQL); JDBC (Arrow Flight SQL) |
| DuckDB | File path; JDBC (duckdb_jdbc); Python (SQLAlchemy, duckdb_engine); ODBC |
| EDB Postgres Advanced Server | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Elasticsearch | URL; Node.js (@elastic/elasticsearch) · JSON; JDBC |
| etcd | URL (endpoint) |
| Exasol | ODBC; JDBC; Python (SQLAlchemy) |
| FerretDB | URI (mongodb://) |
| Firebird | JDBC (Jaybird); ADO.NET (FirebirdSql.Data.FirebirdClient); Python (SQLAlchemy); ODBC |
| Fujitsu Enterprise Postgres | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| GBase 8s | ODBC; JDBC |
| Google BigQuery | JDBC (Simba BigQuery); Python (SQLAlchemy) |
| Google Cloud Spanner | JDBC (google-cloud-spanner-jdbc); Python (SQLAlchemy); Go (go-sql-spanner) |
| Greengage DB | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Greenplum | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| GreptimeDB | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| H2 (servidor PostgreSQL) | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| HeavyDB (OmniSciDB) | ODBC; JDBC |
| IBM Db2 (LUW) | ODBC; JDBC; ADO.NET (IBM.Data.Db2); Python (SQLAlchemy) |
| IBM Db2 for i (AS/400) | ODBC; JDBC; ADO.NET (IBM.Data.Db2); Python (SQLAlchemy) |
| IBM Db2 for z/OS | ODBC; JDBC; ADO.NET (IBM.Data.Db2); Python (SQLAlchemy) |
| IBM Informix | ODBC; JDBC |
| IBM Netezza | ODBC; JDBC |
| InfluxDB 1 (InfluxQL) | URL |
| InfluxDB 2 (Flux) | URL; Node.js (@influxdata/influxdb-client) · JSON |
| InfluxDB 3 (SQL) | URL; Node.js (@influxdata/influxdb3-client) · JSON |
| InterSystems Caché | ODBC; JDBC; ADO.NET (InterSystems); Python (SQLAlchemy) |
| InterSystems IRIS | ODBC; JDBC; ADO.NET (InterSystems); Python (SQLAlchemy) |
| KingbaseES | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| ksqlDB | URL |
| libSQL / Turso | URI (libsql://); Python (SQLAlchemy); Node.js (@libsql/client) · JSON |
| Machbase | ODBC; JDBC |
| Manticore Search | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| MariaDB | URI (mysql://); ADO.NET (MySqlConnector); JDBC (MariaDB Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Materialize | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Memgraph | URI (Bolt); Node.js (neo4j-driver) · JSON; JDBC (Neo4j JDBC) |
| Microsoft Access | ODBC; JDBC; ADO.NET (OleDb, ACE) |
| Microsoft Fabric Data Warehouse | ADO.NET (Microsoft.Data.SqlClient); JDBC (Microsoft JDBC Driver); ODBC; Node.js (mssql, tedious) · JSON; Go (go-mssqldb) |
| Mimer SQL | ODBC; JDBC |
| MonetDB | ODBC; JDBC; Python (SQLAlchemy) |
| MongoDB | URI (mongodb://) |
| MySQL | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Neo4j | URI (Bolt); Node.js (neo4j-driver) · JSON; JDBC (Neo4j JDBC) |
| NuoDB | ODBC; JDBC |
| OceanBase (MySQL) | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Ocient | ODBC; JDBC |
| ODBC (genérico) | ODBC |
| Open Distro for Elasticsearch | URL; Node.js (@opensearch-project/opensearch) · JSON; JDBC |
| openGauss | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| OpenLink Virtuoso | ODBC; JDBC |
| OpenSearch | URL; Node.js (@opensearch-project/opensearch) · JSON; JDBC |
| Oracle | Easy Connect / TNS; ADO.NET (Oracle.ManagedDataAccess); JDBC (thin); Python (SQLAlchemy); Python (python-oracledb); Node.js (node-oracledb) · JSON; Go (go-ora) |
| Oracle Autonomous Database | Easy Connect / TNS; ADO.NET (Oracle.ManagedDataAccess); JDBC (thin); Python (SQLAlchemy); Python (python-oracledb); Node.js (node-oracledb) · JSON; Go (go-ora) |
| Oracle NetSuite (SuiteAnalytics Connect) | ODBC; JDBC |
| OrientDB | URL (REST); JDBC (orientdb-jdbc) |
| PostgreSQL | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Presto | JDBC; Python (SQLAlchemy) |
| Progress OpenEdge | ODBC; JDBC |
| Redis | URI (redis://); ADO.NET (StackExchange.Redis) |
| RisingWave | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| SAP ASE (Sybase) | ODBC; JDBC; ADO.NET (AdoNetCore.AseClient) |
| SAP HANA | JDBC (ngdbc); ADO.NET (Sap.Data.Hana); Python (SQLAlchemy); ODBC; Node.js (@sap/hana-client) · JSON |
| SAP MaxDB | ODBC; JDBC |
| SAP SQL Anywhere | ODBC; JDBC; ADO.NET (Sap.Data.SQLAnywhere) |
| ScyllaDB | Node.js (cassandra-driver) · JSON; JDBC (cassandra-jdbc-wrapper) |
| SingleStore | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Snowflake | JDBC (snowflake-jdbc); ADO.NET (Snowflake.Data); Python (SQLAlchemy); Go (gosnowflake); ODBC; Node.js (snowflake-sdk) · JSON |
| Spark Thrift Server | ODBC; JDBC; Python (SQLAlchemy) |
| SQL Server | ADO.NET (Microsoft.Data.SqlClient); JDBC (Microsoft JDBC Driver); ODBC; Python (SQLAlchemy); Node.js (mssql, tedious) · JSON; Go (go-mssqldb) |
| SQLite | File path; URI (file:); ADO.NET (Microsoft.Data.Sqlite); JDBC (sqlite-jdbc); Python (SQLAlchemy); ODBC |
| SQream DB | ODBC; JDBC |
| Starburst | JDBC; Python (SQLAlchemy); Go (trino-go-client) |
| StarRocks | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| TDengine | JDBC (taos-jdbcdriver, REST); Python (SQLAlchemy); Go (driver-go, taosRestful) |
| Teradata | ODBC; JDBC; ADO.NET (Teradata.Client.Provider); Python (SQLAlchemy) |
| TiDB | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| TimechoDB | URL (REST); JDBC (iotdb-jdbc) |
| Timeplus Proton | URL (HTTP); ADO.NET (ClickHouse.Client); JDBC (clickhouse-jdbc); Python (SQLAlchemy, clickhouse-connect); Node.js (@clickhouse/client) · JSON |
| TimescaleDB | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| Trino | JDBC; Python (SQLAlchemy); Go (trino-go-client) |
| Valkey | URI (redis://); ADO.NET (StackExchange.Redis) |
| VeloDB | URI (mysql://); ADO.NET (MySqlConnector); JDBC (Connector/J); Python (SQLAlchemy); ODBC; Node.js (mysql2); Go (go-sql-driver/mysql) |
| Vertica | ODBC; JDBC; ADO.NET (Vertica.Data); Python (SQLAlchemy) |
| Yellowbrick | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |
| YugabyteDB | URI (postgresql://); libpq (psql, psycopg); ADO.NET (Npgsql); JDBC; Python (SQLAlchemy); ODBC; Node.js (pg); Go (pgx) |

## Commands

`connection_string_info` and `connection_string_build`
([`api-commands.md`](api-commands.md#copy-connection-string-connection-stringsmd)).
