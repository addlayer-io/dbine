# Known bugs

Bugs found while implementing the permissions check
(`Session::permissions`, September 2026). Each entry explains what happens,
where the code is, how to reproduce it and an idea for a fix.

## Status

| # | Status |
|---|---|
| 1 | Fixed and verified live (test `clickhouse/tests/readonly_profile.rs`). The profiler also uses `self.sends_readonly()` for `readonly=2` profiles. |
| 2 | Fixed (Couchbase and DuckDB reject creating and dropping databases in read-only mode; the explorer menu hides both options). Verified live. |
| 3 | Fixed and verified live (mongo:7): `sampleRate` is only sent if the current rate is below 1, and on restore only if it was changed. Test: `db_admin_only_turns_the_profiler_on`. |
| 4 | Doesn't reproduce on CockroachDB v26.3.2: a non-admin user can read `crdb_internal.cluster_queries` and `cluster_sessions`. Even so, the alternative `SHOW CLUSTER STATEMENTS/SESSIONS` was added in case that read fails, along with a notice when `VIEWACTIVITY` is missing (without that permission the user only sees their own queries). The Monitor was also fixed: it asked for `VIEWACTIVITY` for the node metrics and the real permission is `VIEWCLUSTERMETADATA`. Live test: `postgres/tests/cockroach_viewer.rs`. |
| 5 | Fixed: `dbine-test-iotdb2` now uses 27150 (REST) and 27151. Separately: the 2.x types round trip in `transfer_iotdb` makes Docker kill the container for lack of memory (exit 137). |
| 6 | Fixed and tested with Scylla 2026.3.1. |
| 7 | Fixed in Athena and DynamoDB: the tests use a plain HTTP client and pass without `SSL_CERT_FILE` (Athena 45, DynamoDB 46). |
| 8 | Fixed. |
| Inconsistency | Fixed in ClickHouse, Elasticsearch, Solr, CouchDB, Couchbase, OrientDB, Cosmos DB, DynamoDB and MongoDB: the check no longer looks at read-only mode. DuckDB: resolved with point 2. |

---

## 1. ClickHouse: a user with a `readonly = 1` profile can't connect

**Severity:** high. The user can't use DBine with that account.

**What happens.** Every query DBine sends to ClickHouse over HTTP carries
settings in the URL: `output_format_json_quote_64bit_integers=1`,
`output_format_json_quote_decimals=1` and `http_write_exception_in_output_format=0`.
If the user's profile on the server has `readonly = 1`, ClickHouse doesn't let
any setting be changed from the query and rejects everything with an error like
"Cannot modify 'output_format_json_quote_64bit_integers' setting in readonly
mode". The very first query on connect fails.

With `readonly = 2` it works, because that level allows changing settings.
DBine's read-only mode also works: it sends `readonly=1` in the same URL as the
other settings, and ClickHouse accepts them together.

**Where.** `crates/drivers/clickhouse/src/lib.rs`, function
`ClickHouseSession::send` (near line 285), where the vector `q` with the
settings is built.

**How to reproduce** (in `dbine-test-clickhouse`):

```sql
CREATE SETTINGS PROFILE ro_profile SETTINGS readonly = 1;
CREATE USER ro_user IDENTIFIED BY 'x' SETTINGS PROFILE 'ro_profile';
GRANT SELECT ON *.* TO ro_user;
```

Then connect from DBine with `ro_user`.

**Fix idea.**
- On connect, read `getSetting('readonly')`. If it is 1, don't send the format
  settings.
- Make that adjustment on the client side: 64-bit integers and decimals arrive
  without quotes, so they have to be parsed without losing precision. For
  example, request the `JSONCompactEachRowWithNamesAndTypes` format and read
  the numbers as text.
- Another option: if the first attempt fails with "Cannot modify … in readonly
  mode", retry without those settings and remember it for the rest of the
  session.

---

## 2. Couchbase: a read-only connection can create and drop buckets

**Severity:** high. DBine's read-only mode promises that nothing is written,
and this deletes data.

**What happens.** `create_database` and `drop_database` in the Couchbase driver
don't look at `self.conn.read_only`. With a connection marked read-only, "New
database" creates a bucket and "Delete database" removes it with all its data,
as long as the Couchbase user has the permission on the server.

Other paths of the same driver do respect the mode (for example, `execute`
around line 415 of `lib.rs`). Only these two are missing.

**Where.** `crates/drivers/couchbase/src/lib.rs`, `async fn create_database`
(line ~776) and `async fn drop_database` (line ~795).

**How to reproduce.** Create a connection to `dbine-test-couchbase` with
"Read-only" turned on and an administrator user. In the explorer, use
"New database" or "Delete database": the bucket is created or dropped.

**Fix idea.** At the start of both functions:

```rust
if self.conn.read_only {
    return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
}
```

and the same with "drop". That is how `ReadOnlySession` in
`crates/dbine-driver/src/read_only.rs` and the other drivers do it. It's also
worth checking that the UI doesn't offer those options on read-only
connections.

---

## 3. MongoDB: the profiler fails for a user with `dbAdmin` on a single database

**Severity:** medium. The profiler shows nothing to a user who has more than
enough permissions to use it.

**What happens.** On start, the profiler sends
`{ profile: 2, sampleRate: 1.0 }` to the database. Changing `sampleRate`
affects the whole server, so MongoDB requires `enableProfiler` on all
databases. A user with `dbAdmin` on a single database gets "not authorized",
even if `sampleRate` is already 1.

The driver then falls back to the sampled mode with `currentOp`, which that
user can't read either (it needs `inprog`). Result: the profiler shows nothing.

**Where.** `crates/drivers/mongodb/src/profiler.rs`, near line 106:

```rust
let set = doc! { "profile": 2, "sampleRate": 1.0, "comment": s.tag.as_str() };
```

And the restore, near line 124, which also sends `sampleRate`.

**How to reproduce** (on a MongoDB with authentication):

```js
use app
db.createUser({ user: "dba", pwd: "x", roles: [{ role: "dbAdmin", db: "app" }, { role: "read", db: "app" }] })
```

Then open the profiler for the `app` database with that user.

**Fix idea.** Send `sampleRate` only if it needs changing: if the value read
(`rate`, line ~72) is already 1.0, send only `{ profile: 2 }`. Do the same on
restore: if `rate` didn't change, don't send it. That way the user with
`dbAdmin` can raise the level to 2 on their database.

---

## 4. CockroachDB v26.3: the profiler may fail for non-admin users

**Severity:** medium. **Unverified.**

**What happens.** CockroachDB's profiler and Monitor read
`crdb_internal.cluster_queries`. In v26.3, the test container rejects any
access to `crdb_internal` for users that aren't `admin` unless
`allow_unsafe_internals` is enabled. If the same happens in production, a user
with the `VIEWACTIVITY` privilege (which used to be enough) can no longer use
the profiler or the Monitor.

**Where.**
- `crates/drivers/postgres/src/profiler.rs` (lines ~95 and ~239).
- `crates/drivers/postgres/src/monitor.rs` (line ~949).

**How to verify.** In `dbine-test-cockroach`:

```sql
CREATE USER viewer;
GRANT SYSTEM VIEWACTIVITY TO viewer;
```

Then connect as `viewer` and run
`SELECT * FROM crdb_internal.cluster_queries`.

**Fix idea.** If it fails, use the public, stable replacement:
`SHOW CLUSTER STATEMENTS` or the `crdb_internal` views that CockroachDB has
moved to `information_schema` or to `system`. If there's no alternative, have
the profiler explain the reason (`allow_unsafe_internals` is missing or being
admin is required) instead of just failing.

---

## 5. Test containers: `dbine-test-iotdb2` and `dbine-test-dragonfly` use the same port

**Severity:** low. It only affects tests.

**What happens.** Both publish host port 25407:
- `dbine-test-iotdb2`: 18080 → 25407 (and 9092 → 25408).
- `dbine-test-dragonfly`: 6379 → 25407.

If Dragonfly is up, IoTDB 2 starts without a network and can't be reached. The
live IoTDB 2 tests had to be run in a disposable container on another port.

**Fix idea.** Recreate `dbine-test-iotdb2` with free ports (for example 27150
and 27151) and update the `DBINE_TEST_IOTDB2_URL` variable or the comment in
the tests of `crates/drivers/iotdb/tests/` that documents it.

---

## 6. The documented command to start Scylla with authentication no longer works

**Severity:** low. It only affects tests.

**What happens.** Scylla 2026.x no longer creates the default `cassandra`
superuser. With the command in the comment, the container starts with
authentication, but there is no user to log in with.

**Where.** `crates/drivers/cassandra/tests/security.rs`, line 4 of the header
comment.

**Fix idea.** Add `--auth-superuser-name cassandra --auth-superuser-salted-password '<hash>'`
to the command, escaping the hash's `$` in the shell. The hash of `cassandra`
has to be generated in the format Scylla expects (crypt SHA-512). Another
option is to pin the image to an earlier version that still creates the
superuser.

---

## 7. Athena and DynamoDB: tests that fail when creating the AWS client

**Severity:** low. It is probably the environment and not the code, but it
should be confirmed.

**What happens.** Several unit tests that build an AWS client fail:
- Athena: `tests::never_drops_its_own_database` and five of `transfer::tests::*`.
- DynamoDB:
  `transfer::tests::a_writer_dropped_during_runtime_shutdown_neither_panics_nor_hangs`.

The panic is:

```
aws-smithy-http-client-1.4.2/src/client/tls/rustls_provider.rs:163
TrustStore configured to enable native roots but no valid root certificates parsed!
```

The AWS client loads the system's root certificates, and in the environment
where they ran (Claude sessions, possibly with limited access to the macOS
keychain) it found none. It is a `debug_assert!`, so it only fails in debug
builds.

**How to verify.** Run `cargo test -p dbine-driver-athena` from a normal
terminal. If they pass there, it's the environment.

**Fix idea, if they also fail in a normal terminal or in CI.** The tests don't
need real TLS: build the client with a test HTTP connector
(`aws_smithy_http_client::test_util`) or with a configuration that doesn't load
the system roots.

---

## 8. Warning: unused `Zone` type in PostgreSQL table cloning

**Severity:** very low. It is a compile warning.

**Where.** `crates/dbine-transfer/src/clone_table/pg.rs:611`:

```rust
type Zone = (Option<String>, String);
```

It comes from the "Clone table" feature. It has to be deleted or used where it
was meant to be used.

---

## Pending verification against a real server

These are not confirmed bugs. They are permissions-check queries that were
written following the vendor's documentation and never ran against a server. If
they fail, the check leaves the actions enabled (it doesn't error), but they
should be confirmed:

- **SAP HANA** (`crates/drivers/hana/src/permissions.rs`): that
  `SYS.EFFECTIVE_PRIVILEGES` accepts the filter `USER_NAME = CURRENT_USER`.
- **Snowflake** (`crates/drivers/snowflake/src/permissions.rs`): that
  `IS_ROLE_IN_SESSION` accepts a column as an argument. It is used to know
  whether the database's owner role is in the session.
- **Cloud Spanner** (`crates/drivers/spanner/src/permissions.rs`): that
  instance-level `testIamPermissions` accepts the `spanner.backups.*`
  permissions.
- **BigQuery** (`crates/drivers/bigquery/src/permissions.rs`): the check uses
  the Resource Manager API, which has to be enabled in the project. If it
  isn't, everything stays enabled.
- **Dremio Enterprise, Db2 LUW, SAP ASE, Redshift, RisingWave, CrateDB,
  Memgraph Enterprise, InfluxDB 3 Enterprise, Amazon DocumentDB, Open Distro:**
  implemented without a test server.

## Minor inconsistency in the permissions check

With a connection in DBine read-only mode, some drivers report write actions
as "missing: write (the connection is read-only)": ClickHouse, MongoDB,
Elasticsearch, Solr, CouchDB, Couchbase, OrientDB, Cosmos DB and DynamoDB. The
others only report what the server allows. In practice nothing changes,
because the UI already hides writes on read-only connections. Even so, a
criterion should be chosen and applied to all of them. The simplest is for the
permissions check never to look at read-only mode, since `ReadOnlySession`
takes care of that.

Related: in DuckDB, with the connection in read-only mode, "Delete database"
still detaches the database and deletes its file. It has to be decided whether
read-only mode should block it.

---

## Minor details of the migration screen (September 2026)

Found while testing the "Migrations" node. None of them breaks anything.

- **Runs selector while running:** with the migration in progress, the runs
  selector in the "Run" panel says "No runs on this machine". When it finishes
  or is cancelled, it lists the run correctly. It's in
  `web/src/views/MigrationView.vue`: the history is loaded from
  `migration_runs` and the run in progress isn't there yet. The live run has to
  be added to the list.
- **Repeated note:** after two Clone runs on the same migration, the note "Los
  dueños, permisos y tablespaces no se clonan…" appears once per run. Notes
  have to be deduplicated when displayed.
- **Script comment in Spanish:** the generated script's comment ("Estructura de
  … convertida a …") comes out in Spanish even if the UI is in another
  language. It has to be decided whether the script follows the UI language.
