# TLS test material (throwaway)

Everything in this folder is **throwaway test material**. The keys are public
(they are in the repository), so they protect nothing. They are only for the
`dbine-test-tls-*` containers that `scripts/tls-test-servers.sh` starts on a
developer machine. Never use them for anything else: not in a build, not on a
real server, and never add `ca.pem` to a system trust store.

| File | What it is |
|---|---|
| `ca.pem`, `ca.key` | Test CA ("DBine Test CA (throwaway)"), RSA 2048, valid 10 years |
| `server.pem`, `server.key` | Server certificate signed by the test CA. CN `localhost`, SAN `localhost`, `127.0.0.1`, `host.docker.internal`, EKU serverAuth, 825 days (the macOS limit for user-trusted CAs) |
| `selfsigned.pem`, `selfsigned.key` | Self-signed server certificate, same SAN, not signed by the CA |
| `server.p12`, `selfsigned.p12` | The same pairs as PKCS#12 (password `dbine-test`), for Solr (Java keystore) |

The certificates expire in January 2029. To regenerate them, use the openssl
commands at the end of this file.

## The servers

```sh
scripts/tls-test-servers.sh                 # (re)create every dbine-test-tls-* container
scripts/tls-test-servers.sh up postgres     # only some engines
scripts/tls-test-servers.sh verify          # check TLS is served and verifies (table below)
scripts/tls-test-servers.sh down            # remove them
CERT=selfsigned scripts/tls-test-servers.sh up postgres mysql   # self-signed variant
```

The script only ever creates or removes containers whose name starts with
`dbine-test-tls-`. The plain `dbine-test-*` containers stay as they are.

- `CERT=ca` (default): containers `dbine-test-tls-<engine>` serve `server.pem`.
  Verified modes (`verify-full`, `VERIFY_IDENTITY`, `tlsCAFile`, `--cacert`…)
  must pass with `ca.pem` as the CA and fail without it.
- `CERT=selfsigned`: containers `dbine-test-tls-ss-<engine>`, on the **same
  ports + 500**, serve `selfsigned.pem`. Only the "trust server certificate"
  options (`sslmode=require`, `REQUIRED`, `TrustServerCertificate`,
  `tlsAllowInvalidCertificates`, `--insecure`…) pass; verification against
  `ca.pem` fails.

Host name checks: connect with `localhost`, `127.0.0.1` or (from another
container or through `dbine-test-ssh-bastion`) `host.docker.internal`; these
are in the SAN. Any other name or IP (for example the Mac's LAN IP, or the
`0.250.250.254` that `host.docker.internal` resolves to in OrbStack) must fail
the host name check.

Common password: `Dbine-Tls-2026`.

## Ports and credentials (`CERT=ca`; add 500 for `CERT=selfsigned`)

| Container | Host port → container | Protocol | Plain also allowed? | User / password |
|---|---|---|---|---|
| `dbine-test-tls-postgres` (postgres:16) | 26010 → 5432 | PG, `ssl=on` | Yes, same port (`sslmode=disable`) | `postgres` / `Dbine-Tls-2026` |
| `dbine-test-tls-mysql` (mysql:8) | 26011 → 3306 | MySQL TLS | Yes, same port (`ssl-mode=DISABLED`) | `root` / `Dbine-Tls-2026` |
| `dbine-test-tls-mariadb` (mariadb:11) | 26012 → 3306 | MariaDB TLS | Yes, same port (`--skip-ssl`) | `root` / `Dbine-Tls-2026` |
| `dbine-test-tls-sqlserver` (mssql 2022) | 26013 → 1433 | TDS, `forceencryption = 0` | Yes (`Encrypt=Optional`; login still encrypted) | `sa` / `Dbine-Tls-2026` |
| `dbine-test-tls-mongodb` (mongo:7) | 26020 → 27017 | `tlsMode preferTLS` | Yes, same port (`tls=false`) | `root` / `Dbine-Tls-2026` (authSource `admin`) |
| `dbine-test-tls-redis` (redis:7) | 26030 → 6380 TLS (`rediss://`), 26031 → 6379 plain | RESP | Yes, port 26031 | password `Dbine-Tls-2026` (user `default`) |
| `dbine-test-tls-cassandra` (cassandra:5) | 26040 → 9042 | CQL, `client_encryption_options` `optional: true` | Yes, same port | `cassandra` / `cassandra` (PasswordAuthenticator) |
| `dbine-test-tls-neo4j` (neo4j:5) | 26050 → 7687 Bolt (`tls_level OPTIONAL`), 26051 → 7473 HTTPS, 26052 → 7474 HTTP | Bolt / HTTP | Yes: `bolt://` on 26050, HTTP on 26052 | `neo4j` / `Dbine-Tls-2026` |
| `dbine-test-tls-memgraph` (memgraph) | 26055 → 7687 | Bolt over TLS only | No (Memgraph is TLS-only once a cert is set) | no auth |
| `dbine-test-tls-clickhouse` (latest) | 26060 → 8443 HTTPS, 26061 → 9440 native TLS, 26062 → 8123 HTTP, 26063 → 9000 native | HTTP / native | Yes: 26062 and 26063 | `default` / `Dbine-Tls-2026` |
| `dbine-test-tls-elasticsearch` (8.15.3) | 26070 → 9200 | HTTPS only | No (ES serves either HTTP or HTTPS) | `elastic` / `Dbine-Tls-2026` |
| `dbine-test-tls-couchdb` (couchdb:3) | 26080 → 6984 HTTPS, 26081 → 5984 HTTP | HTTP | Yes, 26081 | `admin` / `Dbine-Tls-2026` |
| `dbine-test-tls-etcd` (v3.5.17) | 26090 → 2379 HTTPS/gRPC-TLS, 26091 → 2382 HTTP | gRPC / HTTP | Yes, 26091 | no auth |
| `dbine-test-tls-trino` (latest) | 26100 → 8443 HTTPS, 26101 → 8080 HTTP | HTTP | Yes, 26101 | any user name, no password |
| `dbine-test-tls-influxdb` (influxdb:2) | 26110 → 8086 | HTTPS only | No | `admin` / `Dbine-Tls-2026`, org `dbine`, bucket `dbine`, token `dbine-tls-token` |
| `dbine-test-tls-solr` (solr:9) | 26120 → 8983 | HTTPS only | No | no auth, core `dbine` |
| `dbine-test-tls-flightsql` (GizmoSQL) | 26130 → 31337 | Flight SQL, gRPC over TLS | No | `gizmosql_username` / `Dbine-Tls-2026` |

## Verification results (2026-10-26)

`scripts/tls-test-servers.sh verify` runs, for each port, `openssl s_client`
from the host (with `-starttls postgres|mysql` where the protocol needs it)
three times: with `ca.pem` and host `localhost` (must be `0 (ok)`), with the
wrong trust anchor (must fail), and with host `wrong.example` (must be
`62 hostname mismatch`). Then it logs in with the engine's own client from
inside the container, through `host.docker.internal`, verified and plain.

| Engine | s_client: right CA / wrong CA / wrong host | Client login, verified | Client login, plain |
|---|---|---|---|
| PostgreSQL | 0 ok / 21 / 62 | `psql sslmode=verify-full sslrootcert=ca.pem`: `ssl=t`, TLSv1.3 | `sslmode=disable`: `ssl=f` |
| MySQL | 0 ok / 19 / 62 | `--ssl-mode=VERIFY_IDENTITY --ssl-ca`: TLSv1.3 | `DISABLED`: no Ssl_version |
| MariaDB | 0 ok / 19 / 62 | `--ssl --ssl-ca --ssl-verify-server-cert`: TLSv1.3 | `--skip-ssl`: no Ssl_version |
| SQL Server | n/a (TLS inside TDS prelogin) | `sqlcmd -N` fails with "unable to get local issuer certificate" until the CA is trusted; with it, `encrypt_option=TRUE`; by IP (not in SAN): "subject name does not match host name"; `-N -C`: TRUE | `-No`: `encrypt_option=FALSE` |
| MongoDB | 0 ok / 19 / 62 | `mongosh tls=true&tlsCAFile=ca.pem`: ok | `tls=false`: ok |
| Redis | 0 ok / 19 / 62 (26030) | `redis-cli --tls --cacert`: PONG | 26031: PONG |
| Cassandra | 0 ok / 21 / 62 | `cqlsh --ssl` (`SSL_VALIDATE=true`, `SSL_CERTFILE=ca.pem`): 5.0.9 | `cqlsh` plain: 5.0.9 |
| Neo4j | Bolt 0 ok / 21 / 62; HTTPS 0 ok / 21 / 62 | `cypher-shell bolt+ssc://`: ok | `bolt://`: ok |
| Memgraph | 0 ok / 21 / 62 | (s_client only) | n/a |
| ClickHouse | HTTPS 0 ok / 19 / 62; native TLS 0 ok / 19 / 62 | `curl --cacert https://localhost:26060`: 26.9.2.8 | HTTP 26062: ok |
| Elasticsearch | 0 ok / 21 / 62 | `curl --cacert`: status green | n/a |
| CouchDB | 0 ok / 19 / 62 | `curl --cacert /_up`: ok | 26081: ok |
| etcd | 0 ok / 21 / 62 | `curl --cacert /version`: 3.5.17 | 26091: ok |
| Trino | 0 ok / 21 / 62 | `curl --cacert /v1/info`: ACTIVE | 26101: ACTIVE |
| InfluxDB 2 | 0 ok / 21 / 62 | `curl --cacert /health`: pass | n/a |
| Solr | 0 ok / 19 / 62 | `curl --cacert`: solr-spec 9.10.1 | n/a |
| Flight SQL (GizmoSQL) | 0 ok / 21 / 62 (ALPN h2) | (s_client only) | n/a |

(19 = "self-signed certificate in certificate chain", 21 = "unable to verify
the first certificate": both are verification failures; the code depends on
whether the server also sends the CA.)

Self-signed variant (`CERT=selfsigned`: postgres 26510, mysql 26511, mongodb
26520, redis 26530/26531): s_client gives `0 ok` only with `selfsigned.pem` as
the anchor, `18 self-signed certificate` with `ca.pem`, `62` with a wrong host.
With `ca.pem` every verified client fails (`psql verify-full`: "certificate
verify failed"; `mysql VERIFY_CA`: error 2026; `redis-cli --cacert`: "certificate
verify failed"; `mongosh tlsCAFile`: "self-signed certificate"), while the
no-verify options pass (`sslmode=require`: ssl=t; `REQUIRED`: TLSv1.3;
`redis-cli --insecure`: PONG; `tlsAllowInvalidCertificates=true`: ok).

## Engines not covered here, and why

- **SQL Server strict (TDS 8.0)**: the 2022 Linux image loads the certificate
  and serves Optional/Mandatory TLS with it, but refuses strict connections
  ("A valid TLS certificate is not configured to accept strict (TDS 8.0 and
  above) connections"), even with the container host name set to `localhost`.
  `tlsprotocols = 1.2,1.3` breaks every TLS handshake. Strict stays pending a
  working configuration.
- **Oracle (TCPS)**: needs an Oracle wallet built with `orapki` and listener
  changes inside the Free image; not a simple flag. Pending.
- **ScyllaDB, Valkey, Dragonfly, OpenSearch, CockroachDB, YugabyteDB, other
  PostgreSQL/MySQL-compatible engines**: they use the same driver crates as
  Cassandra, Redis, Elasticsearch, PostgreSQL and MySQL, which are covered;
  can be added to the script when a variant needs its own check.
- **Couchbase, Cosmos DB emulator**: already serve TLS with their own
  self-signed certificates in the existing `dbine-test-couchbase` (18091/18093)
  and `dbine-test-cosmosdb`; loading a custom CA certificate into Couchbase
  takes REST calls after the cluster is initialised.
- **Dremio, Drill, Phoenix, ksqlDB, IoTDB, TDengine, OrientDB**: TLS needs a
  Java keystore plus configuration files inside heavy images; not added yet.
- **libSQL (sqld)**: TLS only on its gRPC listener with its own flags; not added.
- **No TLS at all**: H2, Firebird (wire encryption, not TLS), SQLite/DuckDB
  (local files). **Emulators without TLS**: DynamoDB Local, Spanner,
  BigQuery. **No container**: HANA, Snowflake, Databricks, Athena, DSQL.

## Regenerating the certificates

```sh
cd tests/tls
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 3650 \
  -subj "/O=AddLayer DBine tests/CN=DBine Test CA (throwaway)" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -addext "subjectKeyIdentifier=hash" -out ca.pem
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out server.key
openssl req -new -key server.key -subj "/O=AddLayer DBine tests/CN=localhost" -out server.csr
printf '%s\n' 'basicConstraints=critical,CA:FALSE' \
  'keyUsage=critical,digitalSignature,keyEncipherment' 'extendedKeyUsage=serverAuth,clientAuth' \
  'subjectAltName=DNS:localhost,IP:127.0.0.1,DNS:host.docker.internal' \
  'subjectKeyIdentifier=hash' 'authorityKeyIdentifier=keyid,issuer' > leaf.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 -sha256 \
  -extfile leaf.ext -out server.pem
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out selfsigned.key
openssl req -x509 -new -key selfsigned.key -sha256 -days 825 \
  -subj "/O=AddLayer DBine tests/CN=localhost (self-signed)" \
  -addext "basicConstraints=critical,CA:FALSE" -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,DNS:host.docker.internal" -out selfsigned.pem
openssl pkcs12 -export -in server.pem -inkey server.key -certfile ca.pem -name dbine \
  -passout pass:dbine-test -out server.p12
openssl pkcs12 -export -in selfsigned.pem -inkey selfsigned.key -name dbine \
  -passout pass:dbine-test -out selfsigned.p12
rm -f server.csr leaf.ext ca.srl
chmod 644 *    # the containers read them through a read-only bind mount
```
