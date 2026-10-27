# Per-engine TLS options

This is the design for moving every driver from the legacy `encrypt` /
`trust_server_certificate` pair to its engine's own TLS options. The contract
is already on `main` (commit 6037bbd3). This page says, for each of the 131
engine ids still listed in `NOT_YET_MIGRATED` (`crates/dbine-drivers/src/lib.rs`):

- what it does today;
- what its client library can and can't do;
- the native option names and values;
- the field declaration;
- the TLS port and `TlsSupport`;
- how it verifies through an SSH tunnel;
- how an imported connection string keeps its TLS setting.

It ends with the implementation batches and the risks.

## Decisions (binding)

1. **New connections are TLS on, with the server certificate verified**, for
   every engine that supports TLS. The registry guard enforces it: a mode
   field's `default` must be a verified level (`VerifyCa` or `VerifyFull`).
2. **Every setting can be turned off as an option** (plaintext, skip
   verification, a CA file…) wherever the client library can honour it.
3. **Option names and values are spelled exactly as the engine's own tools
   spell them** (`sslmode` / `verify-full`, `Encrypt` / `Mandatory`,
   `ssl-mode` / `VERIFY_IDENTITY`, `tlsAllowInvalidCertificates`…).
4. **Saved connections keep their exact current behaviour.** There is no
   migration on disk. Each TLS field has a legacy table that maps today's
   `(encrypt, trust_server_certificate)` pair, in the order `[ff, ft, tf, tt]`,
   to the native value that reproduces today's behaviour. An empty entry
   (`""`) means "keep the driver's legacy code path". Use it only when no
   native value reproduces today's behaviour.
5. **MariaDB and OceanBase use MariaDB's names**: `ssl`,
   `ssl-verify-server-cert` and `ssl-ca`.
6. **Oracle**: new connections default to `TCPS` on port 2484.
7. **Imported connection strings keep the TLS setting they carry.** The
   importer writes the native option; it doesn't collapse the setting into
   the legacy bools.
8. `drivers-epoch` is already 4.
9. The legacy PostgreSQL default (ff) shows as `prefer`.

## Certificate pinning and verify-ca (crates/dbine-tls)

The modes that don't verify the chain keep their native names
(`sslmode=require`, `insecure`, `bolt+ssc`, `validate=false`,
`TrustServerCertificate`…), but where the client lets DBine supply the
certificate check they no longer accept any certificate:

- **Unverified (and PostgreSQL's `prefer`/`allow`) pin the certificate on
  first use.** The first connection accepts the server's certificate and
  reports the SHA-256 of its DER (`Session::tls_pin_seen`); the app saves it
  in the connection's options as `tls_pin` (`dbine_driver::TLS_PIN`, hex,
  not a secret). Later connections accept only that certificate. Another one
  fails with `ConnectFailureKind::TlsPinChanged`, whose message names both
  fingerprints; the problem dialog offers "Confiar en el certificado nuevo"
  (`forget_tls_pin`: the pin is cleared, the sessions close and the next
  connect pins the new one). The connection form shows the pinned
  fingerprint in the SSL/TLS tab, with "Olvidar". A connection pointed at
  another host, port or engine drops its pin. Connections saved before this
  have no pin and pin on their next connect. The handshake signatures are
  still verified, so the server must hold the certificate's key.
- **VerifyCa checks the whole chain against the configured roots** (the CA
  file; without one, the system and web PKI roots, or Amazon's RDS bundle
  where it applied) and forgives only the host-name mismatch
  (`CertificateError::NotValidForName*`, which rustls checks after the
  chain). No `danger_accept_invalid_*`.

`crates/dbine-tls` has both rustls verifiers (`PinVerifier` with its
`PinWatch`, `CaOnlyVerifier`) and the configs the drivers use
(`pinned_config`, `ca_only_config`, `verified_config`), on rustls' `ring`
provider. Which engines pin, and which keep their client's own "accept any
certificate" because it can't take a verifier, is in
`docs/engine-support.md` ("Connection encryption").

## The contract, in short

The types live in `crates/dbine-driver/src/info.rs` and
`crates/dbine-driver/src/config.rs`. The UI side is
`web/src/composables/tlsFields.ts`.

| Piece | Use |
|---|---|
| `TlsLevel` | `Off` < `Optional` < `Unverified` < `VerifyCa` < `VerifyFull`. `legacy_pair()` gives the dual-written bools: Off/Optional → ff, Unverified → tt, VerifyCa/VerifyFull → tf. |
| `Field::tls_mode(key, label, &[(value, gloss, level)], default, legacy)` | A select that sets the level (`sslmode`, `Encrypt`, `ssl-mode`, `PROTOCOL`…). At most one per driver. |
| `Field::tls_switch(key, label, on, off, default, legacy)` | A checkbox that sets the level (`tls`, `ssl`, `SSL`, `secure`…). It counts as the mode field. `on` and `off` may be flipped (etcd's `insecure-transport`). |
| `Field::tls_skip_verify(key, label, inverted, legacy)` | A checkbox that skips the certificate checks. It caps the level at `Unverified`. `inverted`: "false" skips (`validateCertificate`, `validate`, `verify_certs`). |
| `Field::tls_skip_hostname(key, label, inverted, legacy)` | A checkbox that skips only the host-name check, capping the level at `VerifyCa`. `inverted`: "false" skips (`ssl_assert_hostname`, `verifyHost`, `check_hostname`). |
| `Field::tls_verify(key, label, &[(value, gloss, level)], default, legacy)` | A verification select next to the mode (`SSLVerification`, `verification_mode`, `hostname_verification`): each value gives the highest level it allows, and it caps the mode's level without raising it. At most one per driver, with a mode field, a verified default and a legacy table (the registry guard checks). An older app reads it as a plain select, so a driver that declares one needs this app version. |
| `Field::tls_ca_file(key, label)` | CA / trust-store file. It has no legacy table. `.ca_required_at(&[levels])`: the levels the driver can't verify at without it; the form marks it required there. |
| `Field::tls_insecure(legacy)`, `Field::tls_cacert()` | curl's `insecure` and `cacert`, for engines whose clients have no names of their own. |
| `Field::tls_server_name(key, label)` | Host-name override for the check (`HostNameInCertificate`). |
| `DriverInfo.tls: TlsSupport` | `Configurable` (has a mode field), `Forced` (always TLS and verified), `InUrl` (the URL or endpoint the user types decides), `None` (no TLS: must be listed in `docs/engine-support.md`), `NotApplicable` (a local file). |
| `DriverInfo.tls_port` | The TLS port when it differs from `default_port`. The form swaps ports when the mode changes (`portForLevel`). |
| `ConnectionConfig::tls_value(field)` | The saved value, else the legacy table's entry. `None`: the legacy path. |
| `ConnectionConfig::tls_level(fields)` | The effective level: the mode's, capped by the verification select and by any skip option that is on. |
| `ConnectionConfig::tls_ca_missing(fields)`, `require_tls_ca(fields)` | The CA file the current level needs and doesn't have; `require_tls_ca` returns a `TlsUntrusted` connect error that names the option that set the level and the CA option. Drivers with a `ca_required_at` call it before connecting. |
| `ConnectionConfig::tls_server_name()` | The real host when the connection goes through an SSH tunnel (`tunnel.origin_host`), else `host`. |

### Rules for every driver

- **The field `key` is the stored option key.** Use the native name as the
  key. The exception is a driver that already stores the option under
  another key (`ssl_root_cert` in PostgreSQL, `ca_cert` in MySQL, `ca_file`
  in DocumentDB, `truststore` in NetSuite). There, keep the stored key and
  put the native spelling in the `label`, so saved values still show up with
  no migration.
- **Read every TLS option through `cfg.tls_value(&field)`**, never through
  `cfg.option()`, so the legacy table applies. Branch on `encrypt` /
  `trust_server_certificate` only where the legacy table has `""`.
- **Keep the legacy table exact.** Each entry has to reproduce today's
  behaviour for that pair. When a native value is close but not exact,
  document the deviation in the engine's section. The PostgreSQL and SQL
  Server sections show the cases.
- **Don't expose an option the client library can't honour.** List it under
  the engine's "Gaps".
- **A verified mode never silently degrades.** If the library can't verify
  in some situation (a tunnel with no server-name override, a mode that needs
  a CA file that is missing), fail with a clear message that names the
  native option to change. Never fall back to plaintext or unverified.
- **Remove the legacy fields** (`Field::encrypt()`, `Field::trust_cert()`, and
  the TLS half of `Field::server_set()`) from the driver's form, and take its
  ids out of `NOT_YET_MIGRATED`. The guard fails for a migrated driver that is
  still listed, and for one that still has the legacy fields.
- **An id that declares `TlsSupport::None`** must be listed in
  `docs/engine-support.md` as `` `id` ``. The guard checks this.

## Cross-cutting work

These items live outside the driver folders. They aren't part of the per-crate
batches; see [Shared work](#shared-work-outside-the-batches).

### SSH tunnels: verifying the real host

Through a tunnel the driver dials `127.0.0.1`. To verify the certificate it
must use `cfg.tls_server_name()`. Per client library:

| Client | How | Engines |
|---|---|---|
| tiberius | `config.hostname_in_certificate(cfg.tls_server_name())` unless `HostNameInCertificate` is set (SNI and verification) | sqlserver, azuresql, babelfish |
| tokio-postgres + native-tls | `pg.host(cfg.tls_server_name()).hostaddr(127.0.0.1)`. This is done today only for cloud sign-ins (`postgres/src/lib.rs:803-818`); do it always. | postgres family, dsql |
| mysql_async | `SslOpts::with_danger_tls_hostname_override(Some(name))`. Today only for cloud sign-ins (`mysql/src/lib.rs:667`); do it always. | mysql family |
| reqwest 0.13 (rustls) | URL host = `cfg.tls_server_name()` with the tunnel's local port, plus `ClientBuilder::resolve(name, 127.0.0.1:0)`. The URL port wins. | every HTTP engine |
| tonic | `ClientTlsConfig::domain_name(cfg.tls_server_name())` | flightsql |
| own rustls (Bolt) | `ServerName::try_from(cfg.tls_server_name())` (`neo4j/src/bolt.rs:516`) | neo4j, memgraph |
| scylla | a custom verifier (see Cassandra) | cassandra, scylladb, keyspaces |
| **no override** | Verification through a tunnel is impossible: a clear error, or the documented fallback | oracle, oracle_adb (oracledb thin), hana (hdbconnect), mongodb family, redis family, most ODBC drivers |

### Imported and exported connection strings

**Import today.** The importers collapse TLS into the legacy bools:

- `crates/dbine-core/src/conn_import/mod.rs:567`: `encrypt|ssl|sslmode|usessl` go through `truthy`.
- `apply_ado`, around `mod.rs:581-608`.
- `ads.rs`, `ssms.rs`, `dbeaver.rs:242-275`, `jetbrains.rs:232`, `dbgate.rs:170`.
- `urls.rs`.

Two kinds of loss follow:

- `sslmode=require` becomes tf, which means `verify-full`.
- `Encrypt=Strict`, `SSLVerification=NONE`, `validateCertificate=false` and
  Flight's `useEncryption` are dropped.

**What import must do instead.** Write the native option of the target driver.
Each engine's section has its import line; the general rules are:

- A native key in the string is copied verbatim (normalised to the
  driver's case for values: `ssl-mode` upper case, `sslmode` lower case).
- JDBC aliases map to the native key: PgJDBC `ssl=true` → `verify-full`;
  Connector/J `useSSL` + `verifyServerCertificate`; MariaDB Connector/J
  `sslMode`; HANA `encrypt` + `validateCertificate`…
- **No TLS parameter** writes the default *of the client the string was
  written for*, so the import connects as that client would. Examples:
  - libpq: `prefer`
  - MySQL: `PREFERRED`
  - MariaDB Connector/J: `ssl=false`
  - SqlClient ≥ 4 / ODBC 18: `Mandatory` (see the open decisions)
- URIs that carry their scheme keep it:
  - `rediss://` → `tls=true`
  - `bolt+s://` → `bolt+s`
  - `bolt+ssc://` and `neo4j+ssc://` → `bolt+ssc` (both are missing from
    `urls.rs:49` today)
  - `tcps://` → `PROTOCOL=TCPS`
- MongoDB URIs stay whole in `connection_string`.
- ODBC: TLS keys that have no field go verbatim into `extra`.
- Dual-write the bools with `TlsLevel::legacy_pair()` of the resulting level.

**Export.** `src-tauri/src/connstring/mod.rs` writes the native values instead
of deriving them from `encrypt`/`trust`. The places today are SQL Server
`:1181`, PostgreSQL `pg_sslmode` `:667`, MySQL `:833-900`, Oracle `:1324-1420`
and ODBC `:1689`.

### Contract additions (batch 0, done)

These landed in batch 0, before the batches that use them (see the table in
[The contract, in short](#the-contract-in-short)). A skip-hostname option is a
checkbox ("true" / "false"); a driver whose engine spells it otherwise (Db2
`SSLClientHostnameValidation=Basic|OFF`) writes its own values when it builds
the connection string.

1. **`Field::tls_skip_hostname(key, label, inverted, legacy)`** with
   `TlsRole::SkipHostname { inverted }`. Inverted host-name checks are common:
   - ksqlDB `verifyHost`
   - Elasticsearch / OpenSearch `ssl_assert_hostname`
   - Solr `solr.ssl.checkPeerName`
   - newer cqlsh `check_hostname`
   - Db2 `SSLClientHostnameValidation=Basic|OFF`
2. **A verification select that caps the level**: `Field::tls_verify` with
   `TlsRole::Verify { levels }`:
   - Trino `SSLVerification` FULL / CA / NONE
   - Avatica `hostname_verification` STRICT / NONE
   - Elasticsearch `verification_mode`

### A verified default that needs a CA file

Some ODBC drivers can't verify without a CA file: SQL Anywhere
(`trusted_certificates`), Netezza (`CaCertFile`), Ignite (`SSL_CA_FILE`), and
Informix / GBase 8s (a GSKit keystore). MySQL's `VERIFY_CA` and the Cloud SQL
modes need `ssl-ca` too.

For these the verified default only holds with a CA. Two things apply:

- The form marks the CA field required while the mode is verified.
- The driver refuses to connect without it, with a message that names the
  option.

The driver must not run unverified.

### Error messages

A verified default will meet self-signed certificates: the SQL Server default
certificate, self-hosted MySQL/TiDB/StarRocks, Oracle without a wallet. Each
driver maps the certificate failure to the finer TLS failure kinds added in
6037bbd3. The message names the native option to change, for example "set
`TrustServerCertificate`", "use `REQUIRED` or give `ssl-ca`", or "Oracle
listens on 2484 for TCPS; choose `TCP` for 1521".

## All engines at a glance

The **Mode** column is the native key of the mode field. "—" means none
(`Forced`, `InUrl`, `None` or `NotApplicable`).

| Id | Crate | TlsSupport | Mode (key) | New default | tls_port | Batch | Risk |
|---|---|---|---|---|---|---|---|
| `sqlserver` | sqlserver | Configurable | `Encrypt` | `Mandatory` | — | 1 | medium-high |
| `azuresql` | sqlserver | Configurable | `Encrypt` | `Mandatory` | — | 1 | low |
| `babelfish` | sqlserver | Configurable | `Encrypt` | `Mandatory` | — | 1 | medium |
| `fabric` | sqlserver | Forced | — | — | — | 1 | low |
| `postgres`, `redshift`, `cockroachdb`, `yugabytedb` | postgres | Configurable | `sslmode` | `verify-full` | — | 1 | medium |
| `cloudsql_postgres`, `alloydb` | postgres | Configurable | `sslmode` | `verify-ca` | — | 1 | high |
| `aurora_postgres` | postgres | Configurable | `sslmode` | `verify-full` | — | 1 | high |
| `timescaledb`, `greenplum`, `cloudberry`, `greengage`, `edb`, `fujitsu`, `kingbase`, `opengauss`, `materialize`, `risingwave`, `cratedb`, `yellowbrick`, `denodo` | postgres | Configurable | `sslmode` | `verify-full` | — | 1 | low |
| `h2` | postgres | None | — | — | — | 1 | low |
| `dsql` | dsql | Forced | — | — | — | 1 | medium |
| `mysql`, `aurora-mysql` | mysql | Configurable | `ssl-mode` | `VERIFY_IDENTITY` | — | 1 | low-medium |
| `cloudsql-mysql` | mysql | Configurable | `ssl-mode` | `VERIFY_CA` | — | 1 | medium |
| `tidb`, `singlestore`, `starrocks`, `doris`, `velodb`, `databend`, `manticore`, `greptimedb` | mysql | Configurable | `ssl-mode` | `VERIFY_IDENTITY` | — | 1 | low (code) / medium (users) |
| `mariadb`, `oceanbase` | mysql | Configurable | `ssl` | `true` | — | 1 | medium |
| `oracle` | oracle | Configurable | `PROTOCOL` | `TCPS` | 2484 | 3 | high |
| `oracle_adb` | oracle | Forced | — | — | — | 3 | low (tunnel: high) |
| `hana` | hana | Configurable | `encrypt` | `true` | — | 3 | high |
| `mongodb`, `ferretdb` | mongodb | Configurable | `tls` | `true` | — | 3 | high |
| `documentdb` | mongodb | Configurable | `tls` | `true` | — | 3 | high |
| `redis`, `valkey`, `dragonfly` | redis | Configurable | `tls` | `true` | — | 3 | medium |
| `neo4j`, `memgraph` | neo4j | Configurable | `scheme` | `bolt+s` | — | 3 | low |
| `neptune` | neo4j | Configurable | `enableSsl` | `true` | — | 3 | medium |
| `cassandra`, `scylladb` | cassandra | Configurable | `ssl` | `true` | — | 3 | high |
| `keyspaces` | cassandra | Forced | — (skip field only) | — | — | 3 | high |
| `clickhouse`, `timeplus` | clickhouse | Configurable | `secure` | `true` | 8443 | 4 | low |
| `elasticsearch`, `opensearch`, `opendistro` | elasticsearch | Configurable | `use_ssl` | `true` | — | 4 | medium |
| `solr` | solr | Configurable | `urlScheme` | `https` | — | 4 | medium |
| `couchbase` | couchbase | Configurable | `scheme` | `couchbases` | 18093 | 4 | medium |
| `couchdb` | couchdb | Configurable | `scheme` | `https` | 6984 | 4 | low |
| `orientdb` | orientdb | Configurable | `scheme` | `https` | — | 4 | low |
| `etcd` | etcd | Configurable | `insecure-transport` | `false` | — | 4 | low |
| `trino`, `presto`, `starburst` | trino | Configurable | `SSL` | `true` | 8443 | 4 | medium |
| `dremio` | dremio | Configurable | `useEncryption` | `true` | — | 4 | low |
| `drill` | drill | Configurable | `enableTLS` | `true` | — | 4 | low |
| `influxdb1` | influxdb | Configurable | `ssl` | `true` | — | 5 | medium |
| `influxdb`, `influxdb3` | influxdb | Configurable | `scheme` | `https` | — | 5 | medium |
| `iotdb`, `timechodb` | iotdb | Configurable | `use_ssl` | `true` | — | 5 | medium |
| `ksqldb` | ksqldb | Configurable | `useTls` | `true` | — | 5 | medium |
| `tdengine` | tdengine | Configurable | `useSSL` | `true` | — | 5 | medium |
| `phoenix`, `avatica` | phoenix | Configurable | `scheme` | `https` | — | 5 | medium |
| `flightsql` | flightsql | Configurable | `useEncryption` | `true` | — | 5 | low |
| `libsql` | libsql | InUrl | — | — | — | 5 | low |
| `cosmosdb` | cosmosdb | InUrl | — (skip field only) | — | — | 5 | low |
| `bigquery` | bigquery | InUrl | — | — | — | 5 | low |
| `spanner` | spanner | InUrl | — | — | — | 5 | low |
| `dynamodb` | dynamodb | InUrl | — | — | — | 5 | low |
| `athena` | athena | Forced | — | — | — | 5 | low |
| `snowflake` | snowflake | Forced | — | — | — | 5 | low |
| `databricks`, `azure_databricks` | databricks | Forced | — | — | — | 5 | low |
| `firebird` | firebird | None | — | — | — | 2 | low |
| `duckdb`, `duckdb_files` | duckdb | NotApplicable | — | — | — | 2 | none |
| `sqlite` | sqlite | NotApplicable | — | — | — | 2 | none |
| ODBC ids (39) | odbc | see the ODBC section in batch 2 | | | | 2 | low to high |

---

## Batch 1: SQL Server, PostgreSQL, MySQL family

### SQL Server family: `sqlserver`, `azuresql`, `fabric`, `babelfish`

**Crate:** `crates/drivers/sqlserver`. The ids come from `Variant::ALL`
(`variant.rs:25`, declared at `:180-237`).

**Client:** `tiberius` 0.13.0, vendored in `vendor/tiberius` with `rustls`.

**Today** (`lib.rs:737-750`): `encrypt` is `cfg.encrypt || variant.forces_encryption() || Login::Token`.
Azure SQL and Fabric force it, and so does an Entra token login.

- `encrypt` true → `EncryptionLevel::Required`.
- `encrypt` false → `EncryptionLevel::Off`, which wraps only the login in TLS.
  Babelfish gets `NotSupported` instead (plain TDS).
- `trust_cert()` is called when `trust || !encrypt`.

| Pair | sqlserver | babelfish | azuresql | fabric |
|---|---|---|---|---|
| ff | login-only TLS, certificate not checked | plain TDS | Required, verified | Required, verified |
| ft | login-only TLS, certificate not checked | plain TDS | Required, `trust_cert` | Required, verified |
| tf | Required, verified (OS store, host name) | same as sqlserver | Required, verified | Required, verified |
| tt | Required, `trust_cert` | same as sqlserver | Required, `trust_cert` | Required, verified |

**What the library can do:**

| Capability | Available | How |
|---|---|---|
| Verify chain and host name | yes | default `TrustConfig`. rustls rejects old v1 self-signed SQL Server certificates (`UnsupportedCertVersion`). |
| Skip verification | yes | `trust_cert()`, which skips chain and name. It **panics** combined with `trust_cert_ca`. |
| Skip host name only | **no** | gap |
| CA / certificate file | yes | `trust_cert_ca(path)`, added on top of the OS store |
| Server-name override | yes | `hostname_in_certificate()`, used for SNI and verification |
| TDS 8 strict | yes | `EncryptionLevel::Strict` |

**Native names:** these are the ODBC 18 / Microsoft.Data.SqlClient spellings:
`Encrypt` (`Optional` / `Mandatory` / `Strict`), `TrustServerCertificate`,
`HostNameInCertificate` and `ServerCertificate`.

#### `sqlserver`, `babelfish`

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `Encrypt` | `tls_mode` | `Optional` → Optional (sqlserver: login-only TLS) or Off (babelfish: plain TDS); `Mandatory` → VerifyFull; `Strict` → VerifyFull (sqlserver only) | `Mandatory` | `Optional, Optional, Mandatory, Mandatory` |
| `TrustServerCertificate` | `tls_skip_verify` | `true` skips | `false` | `false, true, false, true` |
| `ServerCertificate` | `tls_ca_file` | path (`trust_cert_ca`) | — | — |
| `HostNameInCertificate` | `tls_server_name` | text | — | — |

```rust
Field::tls_mode("Encrypt", "Encrypt", &[
    ("Optional", "Optional (solo el inicio de sesión)", TlsLevel::Optional), // babelfish: TlsLevel::Off
    ("Mandatory", "Mandatory", TlsLevel::VerifyFull),
    ("Strict", "Strict (TDS 8.0)", TlsLevel::VerifyFull),                   // not on babelfish
], "Mandatory", ["Optional", "Optional", "Mandatory", "Mandatory"]),
Field::tls_skip_verify("TrustServerCertificate", "TrustServerCertificate", false,
    ["false", "true", "false", "true"]).when("Encrypt", &["Optional", "Mandatory"]),
Field::tls_ca_file("ServerCertificate", "ServerCertificate"),
Field::tls_server_name("HostNameInCertificate", "HostNameInCertificate"),
```

Driver rules:

- `Optional` keeps calling `trust_cert()`, as ODBC does for login-only TLS.
  That makes ff and ft exact.
- An Entra token login raises `Optional` to `Mandatory`, as today.
- `Strict` ignores `TrustServerCertificate`, as ODBC does. The field is hidden
  under `Strict` (`when`) so the chip doesn't show Unverified.
- `TrustServerCertificate=true` with `ServerCertificate` must never reach
  tiberius together, because tiberius panics. The skip wins and the file is
  ignored, or the form rejects the pair.
- **Deviation:** tiberius treats `ServerCertificate` as an added trust
  anchor and still checks the host name. ODBC pins the exact certificate and
  skips the name check. DBine's behaviour is stricter, and a self-signed
  certificate given as the anchor still works.

#### `azuresql`

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `Encrypt` | `tls_mode` | `Mandatory` → VerifyFull; `Strict` → VerifyFull (no `Optional`: Azure refuses plain TDS) | `Mandatory` | `Mandatory, Mandatory, Mandatory, Mandatory` |
| `TrustServerCertificate` | `tls_skip_verify` | `true` skips | `false` | `false, true, false, true` |
| `HostNameInCertificate` | `tls_server_name` | text (private endpoints) | — | — |

There is no `ServerCertificate` field: Azure uses a public CA.

#### `fabric`

`TlsSupport::Forced`, no fields. It is always `Required` and verified, and
`trust_server_certificate` can't be set today. `Strict` stays out until Fabric
is shown to support TDS 8.

**Port:** no separate TLS port (1433, Strict included). **TlsSupport:**
Configurable, except Fabric (Forced).

**SSH tunnel:** unless the user set `HostNameInCertificate`, call
`hostname_in_certificate(cfg.tls_server_name())`. A `host\INSTANCE` value is
parsed off before this, so it isn't affected.

**Import:** write `Encrypt` (`yes`/`true`/`Mandatory` → `Mandatory`;
`no`/`false`/`Optional` → `Optional`; `Strict` → `Strict`), and copy
`TrustServerCertificate`, `HostNameInCertificate` and `ServerCertificate`.
`truthy()` (`conn_import/mod.rs:455`) currently loses `Strict`. When `Encrypt`
is absent, see the open decisions.

**Gaps:** no host-name-only skip.

**Risk:**

- **sqlserver: medium-high.** The verified default fails against SQL Server's
  default self-signed certificate (the same experience as ODBC 18). The
  trust-plus-CA panic needs guarding, and `Strict` needs testing on 2022+.
- **babelfish: medium.** `Mandatory` fails on servers without SSL, and
  `Optional` must keep mapping to `NotSupported`.
- **azuresql, fabric: low.**

### PostgreSQL family: 21 ids

**Ids:** `postgres`, `cockroachdb`, `redshift`, `greenplum`, `yugabytedb`,
`timescaledb`, `kingbase`, `denodo`, `alloydb`, `cloudsql_postgres`,
`aurora_postgres`, `edb`, `fujitsu`, `opengauss`, `cloudberry`, `greengage`,
`risingwave`, `materialize`, `cratedb`, `yellowbrick`, `h2`. They come from
`Variant` in `crates/drivers/postgres/src/lib.rs:283-311`.

**Client:** `tokio-postgres` 0.7.18, `postgres-native-tls` 0.5.3 and
`native-tls` 0.2.18 (Security.framework, SChannel or OpenSSL).

**Today** (`lib.rs:823-864`, `986-1067`):

- **Fields:** `Field::server_set()` plus `ssl_root_cert` (a file, `lib.rs:415`).
  `encrypt` defaults to true for AlloyDB, Cloud SQL, Aurora and Redshift, and
  `h2` has no TLS fields.
- **sslmode:** `Require` when `encrypt`, else `Prefer`. A cloud IAM sign-in
  forces `Require` and refuses `trust` (`cloud_auth.rs:254`).
- **Checks** are done by the connector:
  - `danger_accept_invalid_certs(trust)`.
  - `danger_accept_invalid_hostnames` when there is no host-name check.
  - `disable_built_in_roots` when the CA must be the only anchor.
- **Cloud SQL, AlloyDB or GCP IAM with a CA file:** chain only, anchored only
  on that CA.
- **RDS bundle:** Aurora (when `encrypt`) and AWS IAM without a CA file add the
  embedded RDS bundle.

| Pair | sslmode sent | Certificate when TLS happens | Native equivalent |
|---|---|---|---|
| ff | `Prefer` | **verified**, chain and host name. A server with an unverifiable certificate fails; there is no fallback. | none exact: a "verifying prefer" |
| ft | `Prefer` | not checked | `prefer` |
| tf | `Require` | chain and host name (Cloud SQL/AlloyDB/GCP IAM with a CA: chain only) | `verify-full` (`verify-ca` in that cloud case) |
| tt | `Require` | not checked | `require` |

**What the library can do:**

- `disable`, `prefer` and `require` exist in tokio-postgres.
- `verify-ca` and `verify-full` are built from `Require` plus the connector
  flags.
- **No `allow`** (plaintext first, then TLS): not exposed.
- CA file: `add_root_certificate`, optionally with `disable_built_in_roots`.
- Server-name override: `host(name).hostaddr(ip)`.

**Native names:** libpq's `sslmode` and `sslrootcert`. All the variants above
use libpq's names, including Redshift, CockroachDB, YugabyteDB, Materialize,
RisingWave, CrateDB, Kingbase (a libpq fork), openGauss and Denodo's
PostgreSQL interface.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `sslmode` | `tls_mode` | `disable` → Off; `prefer` → Optional; `require` → Unverified; `verify-ca` → VerifyCa; `verify-full` → VerifyFull | `verify-full`; `verify-ca` for `cloudsql_postgres` and `alloydb` (their certificates name the instance, not the address) | `prefer, prefer, verify-full, require`; for `cloudsql_postgres`/`alloydb`: `prefer, prefer, verify-ca, require` |
| `sslrootcert` (stored key `ssl_root_cert`) | `tls_ca_file` | PEM path or bundle | — | — |

```rust
Field::tls_mode("sslmode", "sslmode", &[
    ("disable", "disable", TlsLevel::Off),
    ("prefer", "prefer (si el servidor lo ofrece)", TlsLevel::Optional),
    ("require", "require (sin verificar)", TlsLevel::Unverified),
    ("verify-ca", "verify-ca (CA)", TlsLevel::VerifyCa),
    ("verify-full", "verify-full (CA y nombre)", TlsLevel::VerifyFull),
], "verify-full", ["prefer", "prefer", "verify-full", "require"]),
Field::tls_ca_file("ssl_root_cert", "sslrootcert"),
```

Driver rules that keep saved connections exact:

1. **The "verifying prefer" for ff.** As decided, ff shows `prefer`. But today
   ff verifies when TLS is negotiated, and ft doesn't. So `prefer` resolved
   from the legacy table, with no saved `sslmode` and `trust == false`, keeps
   today's checks (chain and host name). A saved `prefer`, including every
   new or re-saved connection, has libpq's semantics (no checks). This is
   the only deviation in this family, and only for legacy connections.
2. **What each level checks.**
   - `verify-ca` drops the host-name check.
   - Cloud SQL, AlloyDB or GCP IAM with `sslrootcert` keep "the CA file is the
     only anchor" at both verify levels.
   - Elsewhere the CA is added on top of the system roots, as today. libpq
     replaces the roots instead; document that DBine is less strict here.
3. **GCP IAM on a non-cloud variant** with `sslrootcert`, under the legacy
   `verify-full`, keeps today's chain-only check while `sslmode` isn't saved.
4. **Cloud IAM sign-ins** need at least `verify-ca`. Under `disable`,
   `prefer` or `require` they raise the existing "token can't go to an
   unverified server" error. Legacy IAM connections keep the forced `Require`
   plus verification.
5. **The RDS bundle** is still added for `aurora_postgres` at any verify level,
   and for AWS IAM, when there is no `sslrootcert`.
6. **A known relaxation.** On `cloudsql_postgres` / `alloydb`, legacy tf
   without a CA used to check the host name against the system roots;
   `verify-ca` doesn't. The instance certificate isn't publicly trusted, so no
   outcome changes in practice.

**`h2`:** `TlsSupport::None`, listed in `docs/engine-support.md`. Send
`SslMode::Disable` instead of `Prefer`. Nothing changes in practice, because
H2's PostgreSQL server answers `N`.

**Port:** none (TLS goes through `SSLRequest` on the normal port).
**TlsSupport:** Configurable, except `h2` (None).

**SSH tunnel:** use `pg.host(cfg.tls_server_name()).hostaddr(127.0.0.1)` for
every connection. Today a tunnelled tf connection to a non-cloud server
checks the name against `127.0.0.1` and fails.

**Import:** write `sslmode` verbatim. PgJDBC `ssl=true` with no `sslmode` →
`verify-full`. No parameter → `prefer`. `sslrootcert` → `ssl_root_cert`.

**Gaps:** no `allow`. `sslnegotiation=direct` (PG17) is possible later.

**Risk:** high for the family as a whole.

- `cloudsql_postgres`, `alloydb`, `aurora_postgres`: high (cloud rules).
- `postgres`, `redshift`, `cockroachdb`, `yugabytedb`: medium.
- The other forks and `h2`: low.

Live tests need three servers: one with a self-signed certificate (ff
versus ft), one with a CA-signed certificate (`verify-ca` versus
`verify-full`), and one behind a tunnel.

### Aurora DSQL: `dsql`

**Done (batch 1): `TlsSupport::Forced`.** The real connect always used
`SslMode::Require` (`DsqlDriver::connect`); `SslMode::Disable` below was only
the `connect_with_password` test hook, which runs the session code against a
local PostgreSQL. The driver now always verifies chain and name (the form never
had a trust option) and, through an SSH tunnel, signs the token for and checks
the certificate against the real endpoint while dialing the tunnel
(`host` + `hostaddr`). The table below is the original analysis.

**Crate:** `crates/drivers/dsql`. **Client:** tokio-postgres 0.7.18 and
native-tls (`lib.rs:437-460`). **Today:** the form has no `encrypt` field, so
new connections save `encrypt=false`.

| Pair | Today |
|---|---|
| ff, ft | `SslMode::Disable` (`lib.rs:413-418`). DSQL only accepts TLS, so this very probably can't connect; to confirm live. |
| tf | `Require`, verified (chain and host name) |
| tt | `Require` with invalid certificates and host names accepted |

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `sslmode` | `tls_mode` | `require` → Unverified; `verify-ca` → VerifyCa; `verify-full` → VerifyFull (`disable`/`prefer` left out: the server refuses plaintext) | `verify-full` | `"", "", verify-full, require`. If ff/ft are confirmed broken: `verify-full, verify-full, verify-full, require`. |
| `sslrootcert` | `tls_ca_file` | PEM | — | — |

- **verify-ca:** use `danger_accept_invalid_hostnames(true)` and keep the chain
  checked.
- **Port and TlsSupport:** no separate port. Configurable.
- **Tunnel:** `host(cfg.tls_server_name())` plus `hostaddr(127.0.0.1)`. The IAM
  token is signed for the real host too.
- **Risk: medium.**

### MySQL family: `mysql`, `aurora-mysql`, `cloudsql-mysql`, `tidb`, `singlestore`, `starrocks`, `doris`, `velodb`, `databend`, `manticore`, `greptimedb`

**Crate:** `crates/drivers/mysql`. The ids come from `Variant::info`
(`lib.rs:226-245`).

**Client:** `mysql_async` 0.37.1 with `native-tls-tls`, configured through
`SslOpts` (`lib.rs:649-677`).

**Today:** `Field::server_set()` plus `ca_cert` (`lib.rs:272-281`).
`aurora-mysql`, `cloudsql-mysql` and `velodb` default `encrypt` to true.

| Pair | Today |
|---|---|
| ff | plaintext (no `ssl_opts`, so no TLS is ever attempted) |
| ft | plaintext (`trust` is ignored) |
| tf | TLS required, chain and host name checked against the system roots plus `ca_cert`. Aurora or AWS IAM without `ca_cert` adds the RDS bundle. |
| tt | TLS required, nothing checked |

Exceptions:

- A cloud sign-in forces verified TLS, even with `encrypt` off.
- Cloud SQL or GCP with `ca_cert` checks the chain only, anchored on that CA
  (`lib.rs:663-666`).

**What the library can do:**

- Verify full: yes.
- Skip verify: `with_danger_accept_invalid_certs`.
- Skip host name only: `with_danger_skip_domain_validation`.
- CA file: `with_root_certs`, and `with_disable_built_in_roots` turns the
  system roots off.
- Server-name override: `with_danger_tls_hostname_override`.
- **`PREFERRED` is not supported:** with `ssl_opts` set, a server without TLS
  fails with `NoClientSslFlagFromServer` (`conn/mod.rs:567`). That check runs
  on the server's greeting, before any credentials are sent. So `PREFERRED`
  can be emulated: TLS without checks, and a plaintext retry only on that exact
  error. If the batch judges that too risky, `PREFERRED` is left out.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `ssl-mode` | `tls_mode` | `DISABLED` → Off; `PREFERRED` → Optional (emulated); `REQUIRED` → Unverified; `VERIFY_CA` → VerifyCa; `VERIFY_IDENTITY` → VerifyFull | `VERIFY_IDENTITY`; `VERIFY_CA` for `cloudsql-mysql` | `DISABLED, DISABLED, VERIFY_IDENTITY, REQUIRED` |
| `ssl-ca` (stored key `ca_cert`) | `tls_ca_file` | PEM | — | — |

```rust
Field::tls_mode("ssl-mode", "ssl-mode", &[
    ("DISABLED", "DISABLED", TlsLevel::Off),
    ("PREFERRED", "PREFERRED", TlsLevel::Optional),
    ("REQUIRED", "REQUIRED", TlsLevel::Unverified),
    ("VERIFY_CA", "VERIFY_CA (requiere ssl-ca)", TlsLevel::VerifyCa),
    ("VERIFY_IDENTITY", "VERIFY_IDENTITY", TlsLevel::VerifyFull),
], "VERIFY_IDENTITY", ["DISABLED", "DISABLED", "VERIFY_IDENTITY", "REQUIRED"]),
Field::tls_ca_file("ca_cert", "ssl-ca"),
```

Driver rules:

- While `ssl-mode` isn't saved, a cloud sign-in raises `DISABLED` to verified
  TLS.
- Cloud SQL or GCP with `ca_cert` turns `VERIFY_IDENTITY` into "chain only,
  that CA only".
- `VERIFY_CA` requires `ssl-ca` and turns the system roots off, as the MySQL
  client does. New `cloudsql-mysql` connections need `server-ca.pem`.
- `aurora-mysql` and AWS IAM keep adding the RDS bundle when `ssl-ca` is
  empty.

**Port and TlsSupport:** no separate port. Configurable.

**SSH tunnel:** always set
`with_danger_tls_hostname_override(Some(cfg.tls_server_name()))` when it
differs from `host`. The test `plain_tls_checks_the_name_with_the_system_roots`
(`lib.rs:1164-1182`) has to change.

**Import:** `ssl-mode` / Connector/J `sslMode` go in verbatim, upper case.
Connector/J's older keys map as follows:

| Connector/J | `ssl-mode` |
|---|---|
| `useSSL=false` | `DISABLED` |
| `useSSL=true` + `verifyServerCertificate=false` | `REQUIRED` |
| `useSSL=true` + `verifyServerCertificate=true` | `VERIFY_CA` |
| no TLS parameter | `PREFERRED` |

**Risk:**

- `mysql`, `aurora-mysql`: low-medium (the `PREFERRED` emulation, the cloud
  rules).
- `cloudsql-mysql`: medium.
- The self-hosted variants: low in code, medium for users. Self-signed
  certificates or no TLS at all make `VERIFY_IDENTITY` fail, so the error
  message must point to `ssl-ca` or `REQUIRED`.

### MariaDB and OceanBase: `mariadb`, `oceanbase`

These use the same crate and code path as MySQL, with the same legacy
behaviour. `mariadb` also offers the AWS IAM sign-in. The decision is
MariaDB's names. Those can't say "chain only", so there is no VerifyCa level
here; that's a limit of the names, not of the library.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `ssl` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `ssl-verify-server-cert` | `tls_skip_verify`, inverted | false skips (accept invalid certificates and skip the domain) | `true` | `true, true, true, false` |
| `ssl-ca` (stored key `ca_cert`) | `tls_ca_file` | PEM | — | — |

```rust
Field::tls_switch("ssl", "ssl", TlsLevel::VerifyFull, TlsLevel::Off, true,
    ["false", "false", "true", "true"]),
Field::tls_skip_verify("ssl-verify-server-cert", "ssl-verify-server-cert", true,
    ["true", "true", "true", "false"]),
Field::tls_ca_file("ca_cert", "ssl-ca"),
```

There is no "if offered" mode. Since 11.4 the MariaDB client defaults to TLS on
and verified, and `--skip-ssl` turns it off.

**Tunnel:** the same as MySQL.

**Import:** MariaDB Connector/J's `sslMode` maps as follows:

| `sslMode` | Fields written |
|---|---|
| `disable` | `ssl=false` |
| `trust` | `ssl=true`, `ssl-verify-server-cert=false` |
| `verify-ca` | `ssl=true`, `ssl-verify-server-cert=true`. Stricter than the source; the importer adds a note. |
| `verify-full` | `ssl=true`, `ssl-verify-server-cert=true` |

`useSsl` maps to `ssl`. A string with no TLS parameter imports as `ssl=false`.

**Risk: medium.** Two option schemes share one `connect`, and import and export
branch on the variant.

---

## Batch 2: ODBC and the engines without TLS

### ODBC crate (`crates/drivers/odbc`)

The crate has 39 ids (the presets in `src/presets.rs`).

**Today:**

- The form has no `encrypt` / `trust` fields. `connstr::build`
  (`src/connstr.rs:37-…`) never reads them, so the four legacy pairs build the
  same string.
- TLS comes only from three places:
  - preset literals: `cloudera` always sends `SSL=1`; `netsuite` always sends
    `Encrypted=1;…;Truststore=…` (`presets.rs:997-999`, `1622-1624`);
  - the `extra` field ("Atributos adicionales"), whose keys win over the
    template's;
  - the DSN or connection string typed for the generic `odbc` id.

**Consequence:** the legacy table of every new ODBC field is `L0 = ["", "", "",
""]`, meaning no keyword is written, exactly as today. The only exception is
`cloudera`, whose `SSL` field gets `["1", "1", "1", "1"]`. Writing `disable`,
`PREFER` or similar instead would override the vendor driver's own default and
`odbc.ini`.

**How the fields are written:**

- New TLS fields go through the template (`V::Opt(key)`), so a key the user put
  in `extra` still wins.
- When `extra` holds a TLS key, the SSL tab says "set in the additional
  attributes" instead of showing a level chip, because `tls_level()` can't see
  `extra`.

**Vendor keywords must be confirmed** against each vendor's ODBC reference
before they are implemented. "confirm" below means unverified; never ship a
guessed keyword.

| Id(s) | Vendor TLS keywords | Proposed declaration | New default | Legacy | tls_port | TlsSupport | Sure? |
|---|---|---|---|---|---|---|---|
| `db2`, `db2zos` | `Security=SSL`, `SSLServerCertificate`, `SSLClientHostnameValidation=Basic\|OFF` (11.5.6+) | `tls_switch("Security")` writing `SSL`; `tls_ca_file("SSLServerCertificate")`; inverted SkipHostname `SSLClientHostnameValidation` (needs the contract addition) | `SSL` | L0 | site's `SSL_SVCENAME` | Configurable | yes |
| `db2i` | `SSL=0\|1` (system store) | `tls_switch("SSL", VerifyFull, Off, true, L0)` | `1` | L0 | **9471** (instead of 8471) | Configurable | mostly |
| `hive`, `impala` | Simba: `SSL`, `AllowSelfSignedServerCert`, `CAIssuedCertNamesMismatch`, `TrustedCerts` | `tls_switch("SSL")`, `tls_skip_verify("AllowSelfSignedServerCert")`, SkipHostname `CAIssuedCertNamesMismatch`, `tls_ca_file("TrustedCerts")` | `1` | L0 | — | Configurable | yes |
| `spark`, `kyuubi` | Simba Spark: `SSL`, `AllowSelfSignedServerCert`, `AllowHostNameCNMismatch`, `TrustedCerts` | as Hive, with SkipHostname `AllowHostNameCNMismatch` | `1` | L0 | — | Configurable | yes |
| `cloudera` | as Hive | as Hive; `SSL` legacy `["1","1","1","1"]` | `1` | today's literal | (443 is `default_port`) | Configurable | yes |
| `teradata` | `SSLMODE=DISABLE\|ALLOW\|PREFER\|REQUIRE\|VERIFY-CA\|VERIFY-FULL`, `SSLCA` | `tls_mode("SSLMODE", [DISABLE→Off, ALLOW→Optional, PREFER→Optional, REQUIRE→Unverified, VERIFY-CA→VerifyCa, VERIFY-FULL→VerifyFull])`, `tls_ca_file("SSLCA")` | `VERIFY-FULL` | L0 (driver default PREFER stays) | — (driver uses `HTTPS_PORT`) | Configurable | yes |
| `vertica` | `SSLMode` disable/allow/prefer/require; newer drivers `verify-ca` / `verify-full` (key may be `TLSMode`) | `tls_mode("SSLMode", …)`, CA key to confirm | `verify-full` | L0 | — | Configurable | confirm |
| `exasol` | `ENCRYPTION=Y\|N`; `SSLCertificate=SSL_VERIFY_NONE`; fingerprint pinning | `tls_switch("ENCRYPTION")` (Y/N); skip via `SSLCertificate` | `Y` | L0 | — | Configurable | mostly |
| `netezza` | `SecurityLevel=onlyUnSecured\|preferredUnSecured\|preferredSecured\|onlySecured`, `CaCertFile` | `tls_mode("SecurityLevel", [onlyUnSecured→Off, preferredUnSecured→Optional, preferredSecured→Optional, onlySecured→VerifyCa])`, `tls_ca_file("CaCertFile")` | `onlySecured` (needs a CA) | L0 | — | Configurable | verify semantics: confirm |
| `openedge` | DataDirect: `EncryptionMethod=0\|1`, `ValidateServerCertificate`, `TrustStore`, `HostNameInCertificate` | `tls_switch("EncryptionMethod")`, `tls_skip_verify("ValidateServerCertificate", inverted)`, `tls_ca_file("TrustStore")`, `tls_server_name("HostNameInCertificate")` | `1` | L0 | — | Configurable | yes |
| `sqlanywhere` | `Encryption=NONE\|SIMPLE\|TLS(trusted_certificates=…;certificate_name=…;skip_certificate_name_check=ON)` | `tls_mode("Encryption", [NONE→Off, SIMPLE→Off (obfuscation), TLS→VerifyFull])`, `tls_ca_file("trusted_certificates")`, `tls_server_name("certificate_name")`, SkipHostname `skip_certificate_name_check`; assembled into one `TLS(…)` value | `TLS` (needs a CA) | L0 | — | Configurable | yes |
| `informix`, `gbase8s` | `PROTOCOL=onsoctcp\|onsocssl`; trust comes from the GSKit keystore (`conssl.cfg`) | `tls_mode("PROTOCOL", [onsoctcp→Off, onsocssl→VerifyCa])`; no CA field | `onsocssl` (needs the keystore) | `onsoctcp` ×4 (today's literal) | site port | Configurable | yes |
| `ignite` / `ignite3` | `SSL_MODE` / `ssl_mode` = disable\|require, `SSL_CA_FILE` / `ssl_ca_file` | `tls_mode(…, [disable→Off, require→VerifyCa])`, `tls_ca_file(…)` | `require` (needs a CA) | L0 | — | Configurable | yes / mostly |
| `monetdb` | `TLS=ON\|OFF`, `CERT`, `CERTHASH` | `tls_switch("TLS")`, `tls_ca_file("CERT")` | `ON` | L0 | — | Configurable | confirm |
| `sqream` | `SSL=true\|false` | `tls_switch("SSL")`, only if the driver verifies; otherwise it can't be the default | confirm | L0 | **5100** | Configurable | confirm |
| `sybase` | FreeTDS `Encryption=off\|request\|require\|strict`, or SAP `Encryption=ssl` + `TrustedFile`, depending on `odbc_driver` | only once FreeTDS and the SAP driver can be told apart | `require` | L0 | — | stays in `NOT_YET_MIGRATED` until confirmed | confirm |
| `netsuite` | `Encrypted=1` (always), `Truststore` | no mode; `tls_ca_file("truststore", "Truststore")` (keep the stored key) | — | — | — | **Forced** | yes |
| `odbc` (generic) | in the DSN or the typed string | none | — | — | — | **InUrl** | — |
| `access`, `dbase` | local `.mdb`/`.accdb`/`.dbf` files | none | — | — | — | **NotApplicable** | — |
| `zen` | proprietary wire encryption, not TLS | none | — | — | — | **None** (listed in engine-support.md) | yes |
| `altibase`, `iris`, `cache`, `mimer`, `ocient`, `nuodb`, `dameng`, `cubrid`, `virtuoso`, `ingres`, `maxdb`, `heavydb`, `machbase` | Not decidable from the code. Leads to confirm: Altibase `CONNTYPE=6` + `SSL_CA` + `SSL_VERIFY`, port 20443; IRIS/Caché `SSLDefs.ini` entry; Ocient JDBC `tls=off\|unverified\|on`; NuoDB `trustStore`/`verifyServer`/`sslHostname`; DM `dm_svc.conf` | Configurable where the vendor has a real keyword; **InUrl** ("set in the driver's configuration / additional attributes") where TLS lives only in the driver's config file; **None** with a reason where the ODBC client has no TLS | — | L0 | per vendor | per confirmation | unknown |

**Gaps** (options that aren't exposed):

- **No server-name override:** Db2 CLI, Teradata, Vertica, Simba, Netezza,
  Ignite, Exasol (fingerprint pinning works instead), FreeTDS.
- **No skip-verify:** Db2, Informix / GBase, db2i, Teradata (use `REQUIRE`),
  Ignite.
- **No CA keyword:** Informix / GBase (the keystore), db2i (the system store).

**SSH tunnel:**

- `openedge`: writes `HostNameInCertificate=<tls_server_name()>` when the field
  is empty.
- `sqlanywhere`: writes `certificate_name=<tls_server_name()>`.
- Exasol: fingerprint pinning doesn't depend on the name.
- **Everyone else can't override the name.** Through a tunnel with a
  VerifyFull mode the driver turns off only the host-name check:
  - Db2: `SSLClientHostnameValidation=OFF`
  - Simba: `CAIssuedCertNamesMismatch=1` / `AllowHostNameCNMismatch=1`
  - Teradata: `VERIFY-CA`

  It **reports `verify_ca` as the effective level**, never silently.
  Where even that can't be done, the connection fails with a message.

**Import:** map native keys for ODBC ids:

- JDBC `sslConnection=true` (Db2) → `Security=SSL`
- Teradata `SSLMODE` → verbatim
- Vertica `TLSmode` / `ssl` → `SSLMode`
- Exasol `encryption=1` → `ENCRYPTION=Y`
- Simba `SSL=1` → `SSL=1`

Unmapped TLS keys go verbatim into `extra`. Export (`connstring/mod.rs:1689`,
`odbc_engine`) writes the native key.

**Testing:** no ODBC engine has a `dbine-test-*` container, and the only vendor
driver installed on the development machine is ODBC Driver 18 for SQL Server.
Unit tests can check the generated string; nothing more ships tested until a
container and the vendor driver are added.

**Risk:**

- **High:** `sybase`, `db2`, `db2zos`, `db2i`, `informix`, `gbase8s`,
  `sqlanywhere`, `netezza`, `ignite`, `ignite3`, and every id whose keyword is
  unconfirmed.
- **Medium:** `teradata`, `vertica`, `exasol`, `openedge`, `hive`, `impala`,
  `spark`, `kyuubi`, `cloudera`.
- **Low:** `odbc`, `access`, `dbase`, `zen`, `netsuite`.

### Firebird: `firebird`

The client is `rsfbclient-rust` 0.28, which does SRP plus Firebird's own wire
encryption. That is not TLS, and Firebird has no TLS listener.

`TlsSupport::None`. List `` `firebird` `` in `docs/engine-support.md`, noting
that the wire protocol is encrypted. **Risk: low.**

### DuckDB and SQLite: `duckdb`, `duckdb_files`, `sqlite`

Local files. `TlsSupport::NotApplicable`. **Risk: none.**

---

## Batch 3: binary-protocol clients with library gaps

### Oracle Database: `oracle`

**Crate:** `crates/drivers/oracle`.

**Client:** `oracledb =26.0.0-beta.4`, the thin client in pure Rust, using
rustls 0.23 with the **webpki roots** (not the OS store) (`transport.rs:487`).
A wallet whose `ewallet.pem` has no key adds trusted roots; one with a key does
mTLS.

**Today:**

- The form has `Field::encrypt()` ("Usa TCPS (el puerto suele ser 2484)"). There
  is no trust or CA field.
- `connect_string` writes `PROTOCOL=TCPS` or `TCP` (`lib.rs:292`).
- A `connect_descriptor` or TNS alias is used as written, and its own
  `PROTOCOL` wins.

| Pair | Today |
|---|---|
| ff, ft | TCP (`trust` is never read) |
| tf, tt | TCPS, chain and host name verified against the webpki roots (`trust` is ignored) |

**What the library can do:**

- Verification is always on.
- **No skip-verify, no host-name skip, no server-name override:** SNI and the
  check use the dialled host (`transport.rs:342`).
- A CA can only be given as a wallet folder containing `ewallet.pem`.
- `SSL_SERVER_DN_MATCH` and `SSL_SERVER_CERT_DN` are parsed but not enforced.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `PROTOCOL` | `tls_mode` | `TCP` → Off; `TCPS` → VerifyFull | `TCPS` | `TCP, TCP, TCPS, TCPS` (exact: tt verifies today too) |
| `WALLET_LOCATION` | `tls_ca_file` | a folder (or a file inside it) with `ewallet.pem`; reuse `wallet_dir()` (`lib.rs:188`) | — | — |

```rust
Field::tls_mode("PROTOCOL", "PROTOCOL", &[
    ("TCP", "TCP", TlsLevel::Off),
    ("TCPS", "TCPS", TlsLevel::VerifyFull),
], "TCPS", ["TCP", "TCP", "TCPS", "TCPS"]),
Field::tls_ca_file("WALLET_LOCATION", "WALLET_LOCATION"),
```

Both fields apply only to the host/port/service form; hide them when
`connect_descriptor` is set.

**Port and TlsSupport:** `default_port` 1521, `tls_port: Some(2484)`.
Configurable.

**SSH tunnel: gap.** The thin client can't verify against a name other than
the one it dials, so TCPS (now the default) fails through a tunnel. It already
fails today with `encrypt` on. The fix is a TLS server-name option on
`oracledb`'s `Config`: send it upstream or carry it as a vendored patch. Until
then, return a clear error for TCPS through a tunnel, and never fall back to
TCP.

**Import:** descriptors and `tcps://` strings keep their protocol in
`connect_descriptor`. When the importer splits `@//h:p/s` into fields, a
`tcps:` prefix sets `PROTOCOL=TCPS`, and none sets `TCP`.

**Gaps:** skip-verify, `SSL_SERVER_DN_MATCH=OFF`, `SSL_SERVER_CERT_DN` and a
server-name override aren't exposed.

**Risk: high.**

- A beta library with no way to skip verification or override the name.
- Verification uses the webpki roots, not the OS store.
- Self-hosted servers often listen only on 1521 TCP, so new connections on TCPS
  2484 fail until the user changes them. The error message must say so.

### Oracle Autonomous Database: `oracle_adb`

It never reads the bools today. The descriptor is always `tcps` on 1522, with
mTLS when there is a wallet.

`TlsSupport::Forced`, no mode field. `wallet_dir` and `wallet_password` stay:
they are the mTLS identity and trust, not a level. The behaviour doesn't
change.

**Tunnel:** the same `oracledb` gap. **Risk:** low, high for tunnels.

### SAP HANA: `hana`

**Client:** `hdbconnect_async` 0.32 (rustls).

| Pair | Today (`lib.rs:202-207`) |
|---|---|
| ff, ft | plain TCP |
| tf | `tls_with(ServerCerts::RootCertificates)`, verified |
| tt | `tls_without_server_verification()` |

**What the library can do:**

- CA: `ServerCerts::Direct(pem)` (or `Directory` / `Environment`).
- **No host-name-only skip, no server-name override:** SNI and the check use
  `params.host()`.

**Native names:** JDBC `encrypt`, `validateCertificate`,
`hostNameInCertificate` and `trustStore`. hdbcli spells them with an `ssl`
prefix. DBine's export already writes `encrypt` and `validateCertificate`.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `encrypt` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `validateCertificate` | `tls_skip_verify`, inverted | false skips | `true` | `true, true, true, false` |
| `trustStore` | `tls_ca_file` | PEM (`ServerCerts::Direct`) | — | — |

**Port and TlsSupport:** no separate port (HANA Cloud is 443, on-premise
3NN15, both typed). Configurable.

**Tunnel: gap.** Verification is checked against 127.0.0.1. It needs a
server-name parameter in hdbconnect, upstream or as a vendored patch. Until
then, give a clear error.

**Gaps:** `hostNameInCertificate` isn't exposed.

**Risk: high.** HANA Cloud users often connect through bastions.

### MongoDB, FerretDB, Amazon DocumentDB: `mongodb`, `ferretdb`, `documentdb`

**Client:** `mongodb` 3.9.1 with rustls. The ids are at `lib.rs:161-164`;
the TLS wiring is at `:375-384` and the CA at `:329-335`. `lib.rs:376`
already reads a saved `tls` option.

| Pair | mongodb / ferretdb | documentdb |
|---|---|---|
| ff | no TLS (unless a saved `tls=true`: verified) | no TLS, unless `ca_file` is set: then verified against it |
| ft | no TLS | same as ff |
| tf | TLS, verified | TLS, verified (`encrypt` defaults to true) |
| tt | TLS, `allow_invalid_certificates` | same |

When `connection_string` is set it replaces every field, and TLS comes from the
URI.

**What the library can do:** verify, skip verify, and CA file (`ca_file_path`).
`allow_invalid_hostnames` needs the `openssl-tls` feature, which isn't used;
there is no server-name override either.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `tls` | `tls_switch` | true → VerifyFull, false → Off | `true` | mongodb/ferretdb: `false, false, true, true`; documentdb: `"", "", true, true` (`""` keeps the "TLS if `ca_file`" rule) |
| `tlsAllowInvalidCertificates` | `tls_skip_verify` | true skips | `false` | `false, false, false, true` |
| `tlsCAFile` | `tls_ca_file` | path. DocumentDB also keeps reading the old `ca_file` as a fallback. | — | — |

**Port and TlsSupport:** no separate port. Configurable. When a
`connection_string` is set the form says the TLS fields don't apply.

**Tunnel: gap.** Verification is checked against 127.0.0.1, so it works only
with a `localhost` SAN or `tlsAllowInvalidCertificates`.
`tlsAllowInvalidHostnames` would need `openssl-tls`, which changes the shared
`Cargo.lock`. Document the gap.

**Import:** the URI stays whole in `connection_string`.

**Gaps:** `tlsAllowInvalidHostnames` and a server-name override aren't exposed.

**Risk: high.** The URI overrides the fields, DocumentDB's legacy rule depends
on `ca_file`, and tunnels can't verify.

### Redis, Valkey, Dragonfly: `redis`, `valkey`, `dragonfly`

**Client:** `redis` 1.7.1 (`tokio-rustls-comp`, `tls-rustls-insecure`, webpki
roots). Today it uses `ConnectionAddr::TcpTls { insecure: trust }`
(`lib.rs:258-269`).

| Pair | Today |
|---|---|
| ff, ft | plaintext |
| tf | TLS verified |
| tt | TLS `insecure` |

**What the library can do:** verify, `insecure`, and a CA file through
`build_with_tls(TlsCertificates { root_cert })`. A host-name-only skip exists
only in the cluster builder, and there is no `--sni`.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `tls` (`--tls`, `rediss://`) | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `insecure` (`--insecure`) | `tls_skip_verify` | true skips | `false` | `false, false, false, true` |
| `cacert` (`--cacert`) | `tls_ca_file` | path | — | — |

**Port and TlsSupport:** no separate port. Configurable.

**Tunnel: gap.** There is no SNI override; the same workaround as MongoDB
applies.

**Import:** `rediss://` writes `tls=true`.

**Risk: medium.**

### Neo4j, Memgraph: `neo4j`, `memgraph`

**Client:** DBine's own Bolt client on tokio-rustls / rustls 0.23, with webpki
plus native roots (`bolt.rs:494-519`).

**Today:** `target()` (`bolt.rs:103-125`) sets the scheme; a scheme typed in
the host wins.

| Pair | Today |
|---|---|
| ff, ft | `bolt` |
| tf | `bolt+s` |
| tt | `bolt+ssc` |

The library can do everything, because DBine owns the rustls config.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `scheme` | `tls_mode` | `bolt` → Off; `bolt+s` → VerifyFull; `bolt+ssc` → Unverified | `bolt+s` | `bolt, bolt, bolt+s, bolt+ssc` |
| `trustedCertificates` | `tls_ca_file` | path | — | — |

`neo4j://`, `neo4j+s://` and `neo4j+ssc://` behave like the bolt values,
because DBine doesn't route. A scheme typed in the host keeps winning.

**Port and TlsSupport:** no separate port. Configurable.

**Tunnel:** `ServerName::try_from(cfg.tls_server_name())`, a one-line change.

**Import:** add `+ssc` to `urls.rs:49`.

**Risk: low.**

### Amazon Neptune: `neptune`

**Client:** reqwest, openCypher over HTTP (`neptune.rs:38-44`, `197-201`).

**Today:** a scheme typed in the host is used as is. Otherwise the scheme is
`https` when `encrypt || iam`, else `http`, and trust maps to
`danger_accept_invalid_certs`.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `enableSsl` (TinkerPop) | `tls_switch` | true → VerifyFull, false → Off | `true` | `"", "", true, true` (`""` keeps "IAM forces https") |
| `sslSkipCertValidation` (TinkerPop) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |

- No CA field: Amazon's roots are in the system store.
- **Port and TlsSupport:** no separate port. Configurable, not Forced, because
  saved ff connections use plain http (mocks).
- **Tunnel:** `resolve()`. The SigV4 `Host` header then matches the real name.
- **Risk: medium.**

### Cassandra, ScyllaDB: `cassandra`, `scylladb`

**Client:** `scylla` 1.9 with rustls 0.23. The roots are **native only**
(`lib.rs:455-475`).

**Library fact:** scylla always verifies against
`ServerName::IpAddress(node_ip)` (`connection.rs:1608`). Today's "verified"
therefore means the chain plus an **IP SAN** for each node. Certificates with
host names only fail.

| Pair | Today (`lib.rs:377-378`) |
|---|---|
| ff, ft | plaintext |
| tf | TLS, chain and node-IP SAN |
| tt | TLS, `AcceptAny` |

**Proposal:** a custom verifier that checks the chain, then accepts the name
when it matches **either** the node's IP (today's behaviour, so saved tf
connections don't change) **or** `cfg.tls_server_name()`. That fixes host-name
certificates on single-node clusters and through tunnels. Multi-node clusters
with host-name-only certificates per node still need `validate=false`, which
is a documented gap.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `ssl` (cqlsh `--ssl`) | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `validate` (cqlsh `[ssl] validate`) | `tls_skip_verify`, inverted | false skips | `true` | `true, true, true, false` |
| `certfile` (cqlsh `[ssl] certfile`) | `tls_ca_file` | path | — | — |

`check_hostname` isn't exposed (version-dependent, and it would be an inverted
SkipHostname).

**Port and TlsSupport:** no separate port (9142 is common but per cluster).
Configurable.

**Risk: high.** IP-based names, native-only roots and a custom verifier. Needs
live tests.

### Amazon Keyspaces: `keyspaces`

**Today:** TLS is always on (`encrypt || Keyspaces`, `lib.rs:377`) on 9142, and
`trust` is honoured even with `encrypt` off.

| Pair | Today |
|---|---|
| ff, tf | TLS checked against the node IP. This probably **fails against AWS certificates** today; to confirm live. |
| ft, tt | `AcceptAny` |

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `validate` | `tls_skip_verify`, inverted | false skips | `true` | `true, false, true, false` |
| `certfile` | `tls_ca_file` | path (`sf-class2-root.crt`) | — | — |

`DriverInfo { tls: TlsSupport::Forced, .. }`, with no mode field. The guard
allows a skip field next to `Forced`.

**Tunnel:** the same verifier, checked against
`cassandra.<region>.amazonaws.com`.

**Risk: high.**

---

## Batch 4: HTTP engines (reqwest), part 1

**Common facts:**

- reqwest 0.13.5 with rustls supports `danger_accept_invalid_certs`,
  `tls_danger_accept_invalid_hostnames` (`IgnoreHostname`),
  `add_root_certificate` and `resolve()`.
- For the tunnel: build the URL with `cfg.tls_server_name()` and the local
  port, then add `.resolve(name, 127.0.0.1:0)`.
- Engines with no client-side name for skip-verify or the CA use curl's
  `insecure` / `cacert` (see the open decisions).

### ClickHouse, Timeplus Proton: `clickhouse`, `timeplus`

**Today** (`lib.rs:355-365`):

| Pair | Today |
|---|---|
| ff, ft | `http`, port 8123 |
| tf | `https`, port 8443, verified |
| tt | `https`, skip verify |

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `secure` (clickhouse-client `--secure`) | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `accept-invalid-certificate` (clickhouse-client) | `tls_skip_verify` | true skips | `false` | `false, false, false, true` |
| `caConfig` (`openSSL.client.caConfig`) | `tls_ca_file` | path | — | — |

`tls_port: Some(8443)` (DBine speaks HTTP, so the native TCP 9440 doesn't
apply; Timeplus's HTTPS port is to confirm). Configurable. **Risk: low.**

### Elasticsearch, OpenSearch, Open Distro: `elasticsearch`, `opensearch`, `opendistro`

**Client:** reqwest through `elasticsearch/src/http.rs`. Solr shares it.

**Today:**

- A scheme typed in the host is used as typed. Elastic Cloud hosts and the
  Cloud ID are always `https`.
- Otherwise: ff, ft → `http`; tf → `https` verified; tt → `https` with skip
  verify.
- `trust` applies to any https URL, even one typed in the host with `encrypt`
  off.

The names are the official clients' (elasticsearch-py, opensearch-py). The
server-side `verification_mode` (`full` / `certificate` / `none`) needs the
contract's verification select.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `use_ssl` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `verify_certs` | `tls_skip_verify`, inverted | false skips | `true` | `true, false, true, false` (ft keeps skip-verify for an https URL typed in the host) |
| `ca_certs` | `tls_ca_file` | path | — | — |

- `ssl_assert_hostname=false` waits for the inverted SkipHostname.
- **Port and TlsSupport:** no separate port. Configurable. Elastic Cloud stays
  forced to https.
- **Tunnel:** `resolve()` inside the shared `http::client`, which fixes Solr
  too.
- **Risk: medium.**

### Solr: `solr`

Solr reuses `dbine_driver_elasticsearch::http` (`solr/src/lib.rs:29-31`, `372`),
so it goes in the same batch as Elasticsearch. The legacy behaviour is the same
as Elasticsearch.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `urlScheme` (cluster property) | `tls_mode` | `http` → Off; `https` → VerifyFull | `https` | `http, http, https, https` |
| `insecure` (curl `-k`; Solr has no client name) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `cacert` (curl) | `tls_ca_file` | path | — | — |

`solr.ssl.checkPeerName` waits for the inverted SkipHostname. **Risk: medium.**

### Couchbase: `couchbase`

**Today** (`lib.rs:333-357`):

| Pair | Today |
|---|---|
| ff, ft | `http` on 8093 / 8091 |
| tf | `https` on 18093 / 18091, verified |
| tt | `https` on 18093 / 18091, skip verify |

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `scheme` (`couchbase://` / `couchbases://`) | `tls_mode` | `couchbase` → Off; `couchbases` → VerifyFull | `couchbases` | `couchbase, couchbase, couchbases, couchbases` |
| `tls_verify` (SDK, `peer` / `none`) | `tls_skip_verify`, inverted | false skips | `true` | `true, true, true, false` |
| `truststorepath` (SDK) | `tls_ca_file` | path | — | — |

- **Port:** `tls_port: Some(18093)`. The driver also swaps the management port
  8091 ↔ 18091 by level.
- **Tunnel:** `resolve()`, but a tunnel covers one port and Couchbase needs
  two.
- **Risk: medium.**

### CouchDB, OrientDB: `couchdb`, `orientdb`

**Today:**

- A URL typed in the host is used as typed.
- Otherwise: ff, ft → `http`; tf → `https` verified; tt → `https` with skip
  verify.
- Skip verify also applies to an https URL typed in the host.

Neither engine has client-side names of its own (their docs use curl).

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `scheme` | `tls_mode` | `http` → Off; `https` → VerifyFull | `https` | `http, http, https, https` |
| `insecure` (curl `-k`) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `cacert` (curl) | `tls_ca_file` | path | — | — |

`tls_port`: CouchDB `Some(6984)`. OrientDB `None` (its HTTPS listener is set in
`orientdb-server-config.xml`). **Risk: low.**

### etcd: `etcd`

**Today:** ff, ft → `http`; tf → `https` verified; tt → `https` with skip
verify.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `insecure-transport` (etcdctl) | `tls_switch`, flipped: true → Off, false → VerifyFull | | `false` | `true, true, false, false` |
| `insecure-skip-tls-verify` (etcdctl) | `tls_skip_verify` | true skips | `false` | `false, false, false, true` |
| `cacert` (etcdctl `--cacert`) | `tls_ca_file` | path | — | — |

```rust
Field::tls_switch("insecure-transport", "--insecure-transport",
    TlsLevel::Off, TlsLevel::VerifyFull, false, ["true", "true", "false", "false"]),
```

No separate port. **Risk: low.**

### Trino, Presto, Starburst: `trino`, `presto`, `starburst`

**Today** (`lib.rs:278-287`): the reqwest pattern, and the port falls back to
8443 with `encrypt`, else 8080.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `SSL` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `SSLVerification` | verification select (contract addition) | `FULL` → VerifyFull; `CA` → VerifyCa; `NONE` → Unverified | `FULL` | `FULL, NONE, FULL, NONE` |
| `SSLTrustStorePath` | `tls_ca_file` | PEM | — | — |

- **If the contract addition isn't there yet,** `SSLVerification` can't be
  shown with a correct chip. Interim: expose it only as `FULL` or `NONE` with
  `tls_skip_verify` semantics, or wait for batch 0.
- **Port:** `tls_port: Some(8443)`. The port fallback has to follow `SSL`, not
  `encrypt`.
- **Import:** map `SSL` and `SSLVerification`.
- **Risk: medium.**

### Dremio: `dremio`

REST on 9047 with the reqwest pattern. The names are the Arrow Flight SQL JDBC
ones Dremio documents.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `useEncryption` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `disableCertificateVerification` | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `tlsRootCerts` | `tls_ca_file` | PEM | — | — |

No separate port. **Risk: low.**

### Apache Drill: `drill`

REST on 8047 (the same port serves https). Drill JDBC's names:

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `enableTLS` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `disableCertificateVerification` | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `disableHostVerification` | SkipHostname | true caps the level at VerifyCa | `false` | `false, false, false, false` |
| `trustStorePath` | `tls_ca_file` | PEM (Drill's own is JKS; say so in the help text) | — | — |

**Risk: low.**

---

## Batch 5: HTTP engines part 2, gRPC, and the cloud services

### InfluxDB 1.x, 2.x, 3: `influxdb1`, `influxdb`, `influxdb3`

**Client:** reqwest via `influxdb/src/http.rs`. A URL typed in the host wins,
and the reqwest pattern applies otherwise.

| Engine | Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|---|
| influxdb1 | `ssl` (CLI `-ssl`) | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| influxdb1 | `unsafeSsl` (CLI `-unsafeSsl`) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| influxdb, influxdb3 | `scheme` (the `--host` URL's) | `tls_mode` | `http` → Off; `https` → VerifyFull | `https` | `http, http, https, https` |
| influxdb, influxdb3 | `skip-verify` (`--skip-verify`) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| influxdb3 | `tls-ca` (`--tls-ca`) | `tls_ca_file` | PEM | — | — |

- When the host holds a full URL, its scheme wins, and the form says so.
- The v3 skip flag's name is uncertain; confirm it.
- **Risk: medium.**

### Apache IoTDB, TimechoDB: `iotdb`, `timechodb`

REST on 18080 with the reqwest pattern. The metrics URL stays as typed
(default `http://`).

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `use_ssl` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `insecure` (no native key) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `ca_certs` | `tls_ca_file` | PEM | — | — |

**Risk: medium.**

### ksqlDB: `ksqldb`

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `useTls` (Java client) | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `insecure` (no native key) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `trustStore` | `tls_ca_file` | PEM (the Java client takes JKS) | — | — |

`verifyHost` waits for the inverted SkipHostname. **Risk: medium.**

### TDengine: `tdengine`

REST on taosAdapter 6041.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `useSSL` (JDBC-RS / WS) | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` |
| `insecure` (no native key) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |

A CA file is possible in reqwest but has no native key; expose it only if the
owner accepts `cacert`. **Risk: medium.**

### Apache Phoenix, Avatica: `phoenix`, `avatica`

Query Server / Avatica on 8765. Phoenix's monitor URLs (16010, 16030) are typed
and not affected.

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `scheme` (thin JDBC `url=https://…`) | `tls_mode` | `http` → Off; `https` → VerifyFull | `https` | `http, http, https, https` |
| `insecure` (no native key) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |
| `truststore` | `tls_ca_file` | PEM (Avatica takes JKS) | — | — |

`hostname_verification` (STRICT / NONE) waits for the verification select.
**Risk: medium.**

### Arrow Flight SQL: `flightsql`

**Client:** tonic 0.14 / arrow-flight 58.4 (`tls-ring`, `tls-webpki-roots`),
`lib.rs:198-211`.

| Pair | Today |
|---|---|
| ff, ft | `grpc+tcp` |
| tf, tt | verified against the webpki roots with `domain_name(host)`. tt is verified too: tonic has no skip. |

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `useEncryption` | `tls_switch` | true → VerifyFull, false → Off | `true` | `false, false, true, true` (exact) |
| `tlsRootCerts` | `tls_ca_file` | PEM (`Certificate::from_pem`) | — | — |

- **Gap:** `disableCertificateVerification` would need a custom rustls
  connector; not exposed for now.
- **Tunnel:** `.domain_name(cfg.tls_server_name())`. Today it passes `host`.
- **Import:** read `useEncryption`; Flight JDBC defaults it to true.
- **Risk: low.**

### Azure Cosmos DB: `cosmosdb`

The `host` field is the endpoint URL (`https://` by default; a typed `http://`
is kept). The only TLS field today is `trust`, which maps to
`danger_accept_invalid_certs` (for the emulator).

| Option (native key) | Kind | Values → TlsLevel | New default | Legacy [ff, ft, tf, tt] |
|---|---|---|---|---|
| `DisableServerCertificateValidation` (SDK connection string; confirm the exact key) | `tls_skip_verify` | true skips | `false` | `false, true, false, true` |

`TlsSupport::InUrl` plus this skip field. No CA field (no native key).
**Tunnel:** `resolve()`. **Risk: low.**

### libSQL / Turso: `libsql`

The URL decides (`hrana.rs:72-98`):

- `libsql://`, `wss://`, `https://` and a bare host → `https`, verified.
- `ws://` and `http://` → `http`.

`TlsSupport::InUrl`. The importer keeps the scheme. **Risk: low.**

### Google BigQuery, Cloud Spanner: `bigquery`, `spanner`

The Google API is always https. The optional `endpoint_url` (`gcp.rs:44`) is
for the emulators, which are plain http.

`TlsSupport::InUrl`: the scheme of a typed endpoint decides. This is more
honest than `Forced` with an emulator. **Risk: low.**

### Amazon DynamoDB: `dynamodb`

aws-sdk with `default-https-client`. The `endpoint_url` field is for DynamoDB
Local.

`TlsSupport::InUrl`. **Risk: low.**

### Amazon Athena: `athena`

aws-sdk, no endpoint field. `TlsSupport::Forced`. **Risk: low.**

### Snowflake: `snowflake`

Always `https://<account>.snowflakecomputing.com`. A typed `http(s)://` is kept,
but only the mocks use it. Snowflake's `insecureMode` only turns off OCSP.

`TlsSupport::Forced`. **Risk: low.**

### Databricks, Azure Databricks: `databricks`, `azure_databricks`

Always `https`. A typed `http://` is kept, but only the mocks use it. The
Databricks drivers are always `SSL=1`.

`TlsSupport::Forced`. **Risk: low.**

---

## Batches

Each batch touches only its own `crates/drivers/<engine>/` folders. The batches
are balanced by work rather than by id count. Batch 1 has the largest, most
used drivers. Batches 4 and 5 are many small crates with the same reqwest
pattern.

| Batch | Crate folders | Ids | What's in it |
|---|---|---|---|
| **1** | `sqlserver`, `postgres`, `mysql`, `dsql` | 39 | tiberius, tokio-postgres and mysql_async; cloud sign-in rules; the verifying `prefer`; MariaDB names; `PREFERRED` emulation; tunnel overrides |
| **2** | `odbc`, `firebird`, `duckdb`, `sqlite` | 43 | 39 ODBC presets (confirm vendor keywords, L0 legacy, CA-required modes); three trivial declarations (None / NotApplicable) |
| **3** | `oracle`, `hana`, `mongodb`, `redis`, `neo4j`, `cassandra` | 15 | binary protocols with library gaps: no server-name override (oracledb, hdbconnect, mongodb, redis), the scylla custom verifier, the Bolt scheme |
| **4** | `clickhouse`, `elasticsearch`, `solr`, `couchbase`, `couchdb`, `orientdb`, `etcd`, `trino`, `dremio`, `drill` | 15 | the reqwest pattern with real switches, skip and CA options, TLS ports (8443, 18093, 6984), the shared ES/Solr HTTP helper |
| **5** | `influxdb`, `iotdb`, `ksqldb`, `tdengine`, `phoenix`, `flightsql`, `libsql`, `cosmosdb`, `bigquery`, `spanner`, `dynamodb`, `athena`, `snowflake`, `databricks` | 19 | the reqwest pattern without native skip names, tonic, and the Forced / InUrl cloud services |

Totals: 38 crates, 131 ids.

### Shared work outside the batches

These items touch shared files (AGENTS.md, "Shared files") and need the usual
announcement and confirmation:

- **Batch 0, before the batches that use it:** the contract additions in
  `crates/dbine-driver` (`tls_skip_hostname` with `inverted`, the verification
  select). Batches 2, 4 and 5 use them; batches 1 and 3 don't need them.
- **`NOT_YET_MIGRATED`** in `crates/dbine-drivers/src/lib.rs`: each batch takes
  its ids out when it merges. One session does it, or each batch does it under
  the shared-file protocol. The guard test fails until the list matches.
- **`docs/engine-support.md`:**
  - list the `None` ids (`h2`, `firebird`, `zen`, and any ODBC id confirmed
    without TLS);
  - add the "Connection encryption" section that the code already
    references.
- **Import** (`crates/dbine-core/src/conn_import/*`) and **export**
  (`src-tauri/src/connstring/mod.rs`) of the native options: one session,
  after or alongside the batches, following each engine's "Import" line.
- **Tests:** live checks against the `dbine-test-*` containers for every engine
  with an image. Each case covers the four legacy pairs (behaviour unchanged),
  the new default against a CA-signed certificate, the skip option, the CA file
  and a tunnel.
- **Texts:** new labels or glosses ("si el servidor lo ofrece", "sin
  verificar"…) and the error messages ship in the ten languages. Native option
  names aren't translated.
- **CHANGELOG.md:** a line under `Unreleased` once a batch is user-visible.

## Riskiest engines

1. **Oracle (`oracle`, `oracle_adb`).** The beta thin client can't skip
   verification or verify against the real host, so TCPS through an SSH tunnel
   fails. TCPS on 2484 becomes the default while many servers only listen on
   1521. Verification uses the webpki roots, not the OS store.
2. **PostgreSQL family, especially `cloudsql_postgres`, `alloydb` and
   `aurora_postgres`.** The legacy ff "verifying prefer" can't be expressed as a
   native value. The cloud rules (CA-only anchoring, IAM minimums, the RDS
   bundle) have to survive. It covers 21 ids.
3. **Cassandra / ScyllaDB / Keyspaces.** scylla verifies against node IPs and
   needs a custom verifier. Keyspaces' verified mode probably fails today.
4. **SAP HANA.** No server-name override, so no verified tunnel; HANA Cloud
   users often connect through bastions.
5. **MongoDB / DocumentDB.** The URI overrides the fields, DocumentDB's legacy
   rule depends on `ca_file`, and tunnels can't verify.
6. **SQL Server.** The verified default meets SQL Server's self-signed
   default certificate; there is the trust-plus-CA panic in tiberius and
   `Strict` to test.
7. **The ODBC presets.** Many vendor keywords are unconfirmed, several drivers
   can't verify without a CA, and there are no containers or vendor drivers to
   test against.

## Open decisions for the owner

1. **SQL Server import without `Encrypt`.** The recommendation is
   `Mandatory`, the default of SqlClient ≥ 4 and ODBC 18. Older clients
   default to `Optional`.
2. **Engines without a native skip-verify or CA name** (IoTDB, ksqlDB,
   TDengine, Phoenix/Avatica, CouchDB, OrientDB, Solr). The proposal is curl's
   `insecure` and `cacert` everywhere. The alternative, not exposing them,
   breaks the exact legacy tt behaviour.
3. **The MySQL `PREFERRED` emulation:** TLS first, and a plaintext retry only
   on `NoClientSslFlagFromServer`, before any credentials are sent. The
   alternative is to leave `PREFERRED` out.
4. **Verified modes that need a CA file** (SQL Anywhere, Netezza, Ignite,
   Informix/GBase, MySQL `VERIFY_CA`). The proposal is a required CA field plus
   a refusal to connect without it.
5. **Oracle and HANA tunnels.** A vendored patch for the server-name option, or
   a documented "not through SSH tunnels with TCPS / encrypt" until upstream
   adds it.
6. **MongoDB `openssl-tls`.** It would bring `tlsAllowInvalidHostnames` but
   changes `Cargo.lock` and the TLS backend. The proposal is not to do it.
