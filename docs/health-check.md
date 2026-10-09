# Health check

Reviews a database and lists what deserves attention, ordered by severity:
what is already a problem, what may become one and what is fine. Each finding
says what it means, which objects it involves and, when there is one, brings
a **fix** to open in a query.

## Where it is

Right-click a **database** › **Health check**. It opens in its own tab
(**Health**) and runs by itself; **Check again** repeats it.

## What it shows

- Findings grouped by severity: **Critical**, **Warnings**, **Information**
  and **OK**. What is fine is shown by choosing **Show what's OK**, so you can
  see it was checked.
- Each finding has its category (for example, Configuration, Performance,
  Space, Activity, Backups, Design), a one-line title, the detail of what it
  means and what to do, and the **objects** involved (up to a few hundred;
  the rest, "and N more").
- **Open in a query** opens the fix script. **DBine never runs it**: you
  review it and run it yourself.
- **Could not be checked** lists the checks that didn't run and why (missing
  permissions, an engine version that doesn't have it…). A failing check is
  skipped and doesn't stop the others.
- When it was checked (**Checked on…**).

The check runs on its own **read-only session** and can be cancelled.

## Common checks

Those DBine can answer on any engine with what it already reads:

| Check | Looks at | Severity |
|---|---|---|
| `connections` | Connection usage against the maximum (where the Monitor reports both). | Warning from 75 %; critical from 90 %. |
| `cache_hit` | The cache hit percentage. | Information under 90 %; warning under 80 %. |
| `long_queries` | Active queries over **5 minutes** (doesn't count DBine's own or system ones). | Warning. |
| `blocking` | Sessions blocked by others. | Warning up to 4; critical from 5. |
| `idle_in_transaction` | Sessions with an open transaction idle for more than **10 minutes**. | Warning. |
| `last_backup` | The last backup the engine records. | Warning with no recorded backups or older than **7 days**. |

They depend on the engine having a Monitor, processes and its own backups
respectively (see [`locks.md`](locks.md), [`processes.md`](processes.md) and
[`backups.md`](backups.md)). The backups one uses the engine's history:
backups made with other tools outside the server don't appear.

## Each engine's own checks

Each driver adds its own. All are read queries on the catalog.

| Engine | What it checks |
|---|---|
| SQL Server, Azure SQL | Configuration (`AUTO_SHRINK`, `AUTO_CLOSE`, `PAGE_VERIFY`, `FULL` recovery without log backups, compatibility level, VLFs), statistics, unused indexes (with the window in which they were observed), untrusted constraints, foreign keys without an index, heaps and disabled indexes. |
| PostgreSQL and its family | Autovacuum off (global or per table), dead tuples, never-analyzed tables, the transaction ID limit (*wraparound*), unused indexes (`idx_scan = 0`, with the counters' window), invalid and duplicate indexes, foreign keys without an index, tables without a primary key and sequences near their limit. In CockroachDB, automatic statistics collection off; in Redshift, stale statistics and unsorted rows. |
| MySQL, MariaDB, TiDB, OceanBase | Tables without a primary key, MyISAM tables on an InnoDB server, unused and redundant indexes, fragmented tables, foreign keys without an index and tables with a collation different from the database's. |
| Oracle | Invalid objects, unusable indexes, nearly full tablespaces, stale or missing statistics, foreign keys without an index, tables without a primary key, sequences near their limit and the recycle bin. |
| SAP HANA | Invalid objects, columnar tables whose *delta* part needs merging (or with automatic merge off), tables without a primary key and virtual tables without statistics. |
| Firebird | The distance between the oldest interesting transaction and the next one, forced writes, index statistics never computed, inactive indexes and tables without a primary key. |
| ClickHouse | Partitions with too many active parts, detached or broken parts, lagging or read-only replicas, mutations that fail or don't finish and large tables with dates but no TTL (information only). |
| Snowflake | Time Travel retention at 0, large tables without a clustering key, small tables paying for automatic reclustering, long retention on large tables, dropped tables that Time Travel still holds and *warehouses* that don't suspend. |
| BigQuery | Large tables neither partitioned nor clustered, partitioned tables that don't require a partition filter, time-partitioned tables without expiration, the storage billing model that would cost less and a long *time travel* window. |
| Databricks | The *warehouse*'s auto-stop, predictive optimization off, Delta tables that retain deleted files for a long time and tables that aren't Delta. |
| Db2 LUW (ODBC) | Invalid objects, tables pending reorganization or in pending integrity, tables without RUNSTATS and without a primary key. |
| Sybase ASE (ODBC) | `sp_dboption` options that matter for recovery or access, the log sharing a device with the data and tables with no index at all. |
| Informix, GBase 8s (ODBC) | Database without transaction logging, tables without `UPDATE STATISTICS` and without a primary key. |

The other engines show only the common checks. The other ODBC profiles (Db2
for i and z/OS, Teradata, Vertica and the generic profile) have no checks of
their own because what is needed is in catalogs DBine doesn't read yet.

Some checks depend on the variant: for example, *vacuum* and *wraparound*
don't apply to CockroachDB or YugabyteDB, whose storage doesn't have them;
and the unused-index ones say **Inconclusive** if the counters cover less
than 14 days.

## Contract

One `Session` method with a default implementation:

- `health_checks(&mut self, database: &str) -> Result<Vec<HealthCheck>>`.
  By default it returns nothing and is added to the common checks. A
  `HealthCheck` carries `id`, `category`, `title`, `severity` (`ok`, `info`,
  `warning`, `critical`), `detail`, `objects` and `fix`.

The common checks come from `monitor()`, `processes()` and `backups()` in the
`database_health` command (in `src-tauri/src/commands/db_health.rs`), which
takes `connectionId`, `database` and a `runId` to cancel it with
`cancel_query` on `health:<runId>`. See [`api-commands.md`](api-commands.md).
