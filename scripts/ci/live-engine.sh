#!/usr/bin/env bash
# One engine's live tests (the driver's #[ignore] tests) against servers started here in Docker, for
# CI's `live-engines` job:
#
#   scripts/ci/live-engine.sh postgres
#
# The servers listen on the ports the tests know (the same as the dbine-test-* containers a developer
# runs, see each tests/*.rs header), so it runs only in CI, where nothing else uses them: on a computer,
# run the suites against your own containers. Passwords are test-only; the TLS servers come from
# scripts/tls-test-servers.sh and serve the throwaway certificates in tests/tls/.
#
# Left out: tests that need a cloud account (AWS, Microsoft Entra ID) and variants no container here
# starts (each of those skips itself when its variable isn't set).
set -euo pipefail

engine=${1:?usage: live-engine.sh <engine>}
if [[ -z ${CI:-} ]]; then
  echo "live-engine.sh runs in CI only: it takes the ports of the dbine-test-* containers." >&2
  exit 2
fi
cd "$(dirname "$0")/../.."

# start <name> <docker run args…>
start() {
  local name=dbine-test-ci-$1
  shift
  docker run -d --name "$name" "$@" >/dev/null
  echo "started $name"
}

# ready <label> <command…>: waits up to 6 minutes for the command to succeed.
ready() {
  local label=$1
  shift
  for _ in $(seq 1 180); do
    if "$@" >/dev/null 2>&1; then
      echo "$label is ready"
      return 0
    fi
    sleep 2
  done
  echo "$label didn't start in time"
  docker ps -a --filter name=dbine-test- --format '{{.Names}}\t{{.Status}}'
  return 1
}

# resp_ping <port>: a Redis-protocol server answers PING there.
resp_ping() { printf 'PING\r\n' | nc -w 2 localhost "$1" | grep -q PONG; }

# The TLS servers of scripts/tls-test-servers.sh, with the CA's certificate and self-signed.
tls_servers() {
  scripts/tls-test-servers.sh up "$@"
  CERT=selfsigned scripts/tls-test-servers.sh up "$@"
}

run_tests() {
  cargo test -p "dbine-driver-$1" --tests --no-fail-fast -- --ignored --test-threads=1 \
    --skip cloud_auth_tests::live_ --skip entra::tests::live_
}

TLS_PW='Dbine-Tls-2026'

case $engine in
  postgres)
    start postgres -p 25010:5432 -e POSTGRES_PASSWORD=pw postgres:16 \
      -c shared_preload_libraries=pg_stat_statements
    start cockroach -p 26014:26257 cockroachdb/cockroach:latest start-single-node --insecure
    tls_servers postgres
    ready postgres docker exec dbine-test-ci-postgres pg_isready -U postgres
    ready cockroach docker exec dbine-test-ci-cockroach ./cockroach sql --insecure -e 'SELECT 1'
    ready postgres-tls docker exec dbine-test-tls-postgres pg_isready -U postgres
    ready postgres-tls-ss docker exec dbine-test-tls-ss-postgres pg_isready -U postgres
    export DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres
    export DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb
    export DBINE_TEST_COCKROACHDB_URL=postgres://root@localhost:26014/defaultdb
    run_tests postgres
    ;;
  mysql)
    start mysql -p 25011:3306 -e MYSQL_ROOT_PASSWORD=pw mysql:8
    start mariadb -p 25012:3306 -e MARIADB_ROOT_PASSWORD=pw mariadb:11
    start mariadb-notls -p 26600:3306 -e MARIADB_ROOT_PASSWORD=pw mariadb:11 --skip-ssl
    tls_servers mysql mariadb
    ready mysql docker exec dbine-test-ci-mysql mysqladmin ping -h 127.0.0.1 -ppw --silent
    ready mariadb docker exec dbine-test-ci-mariadb healthcheck.sh --connect --innodb_initialized
    ready mariadb-notls docker exec dbine-test-ci-mariadb-notls healthcheck.sh --connect --innodb_initialized
    for n in tls-mysql tls-ss-mysql; do
      ready "$n" docker exec "dbine-test-$n" mysqladmin ping -h 127.0.0.1 "-p$TLS_PW" --silent
    done
    for n in tls-mariadb tls-ss-mariadb; do
      ready "$n" docker exec "dbine-test-$n" healthcheck.sh --connect --innodb_initialized
    done
    export DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011
    export DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012
    run_tests mysql
    ;;
  sqlserver)
    # The Agent on: the scheduler, the jobs and the lineage of jobs need it.
    start sqlserver -p 25013:1433 -e ACCEPT_EULA=Y -e 'MSSQL_SA_PASSWORD=Pw_12345!' \
      -e MSSQL_AGENT_ENABLED=true mcr.microsoft.com/mssql/server:2022-latest
    tls_servers sqlserver
    ready sqlserver docker exec dbine-test-ci-sqlserver /opt/mssql-tools18/bin/sqlcmd -C \
      -S localhost -U sa -P 'Pw_12345!' -Q 'SELECT 1'
    for n in tls-sqlserver tls-ss-sqlserver; do
      ready "$n" docker exec "dbine-test-$n" /opt/mssql-tools18/bin/sqlcmd -C \
        -S localhost -U sa -P "$TLS_PW" -Q 'SELECT 1'
    done
    export 'DBINE_TEST_SQLSERVER_URL=mssql://sa:Pw_12345!@localhost:25013'
    export 'DBINE_TEST_SQLSERVER_AGENT_URL=mssql://sa:Pw_12345!@localhost:25013'
    run_tests sqlserver
    ;;
  mongodb)
    start mongodb -p 25201:27017 -e MONGO_INITDB_ROOT_USERNAME=root \
      -e MONGO_INITDB_ROOT_PASSWORD=secret mongo:7
    start ferretdb -p 25203:27017 -e POSTGRES_USER=root -e POSTGRES_PASSWORD=secret \
      ghcr.io/ferretdb/ferretdb-eval:2
    ready mongodb docker exec dbine-test-ci-mongodb mongosh --quiet -u root -p secret \
      --eval 'db.runCommand({ ping: 1 })'
    ready ferretdb docker exec dbine-test-ci-mongodb mongosh --quiet \
      'mongodb://root:secret@172.17.0.1:25203/' --eval 'db.runCommand({ ping: 1 })'
    export 'DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin'
    export DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/
    run_tests mongodb
    ;;
  redis)
    # Redis 7 and its family: 6.2 (no COMMAND DOCS), 8 (query engine, vector sets), Valkey, Valkey
    # with search, Dragonfly.
    start redis -p 25400:6379 redis:7
    start redis6 -p 25406:6379 redis:6.2
    start redis8 -p 27203:6379 redis:8
    start valkey -p 25401:6379 valkey/valkey:latest
    start valkey-search -p 27204:6379 valkey/valkey-bundle:latest
    start dragonfly -p 25407:6379 docker.dragonflydb.io/dragonflydb/dragonfly:latest \
      --proactor_threads=1 --maxmemory=512mb
    for p in 25400 25406 27203 25401 27204 25407; do ready "redis-family:$p" resp_ping "$p"; done
    export DBINE_TEST_REDIS_URL=redis://localhost:25400
    export DBINE_TEST_REDIS6_URL=redis://localhost:25406
    export DBINE_TEST_REDIS8_URL=redis://localhost:27203
    export DBINE_TEST_VALKEY_URL=redis://localhost:25401
    export DBINE_TEST_VALKEY_SEARCH_URL=redis://localhost:27204
    export DBINE_TEST_DRAGONFLY_URL=redis://localhost:25407
    run_tests redis
    ;;
  oracle)
    start oracle -p 25601:1521 -e ORACLE_PASSWORD=Secret123 -e APP_USER=dbine \
      -e APP_USER_PASSWORD=dbine gvenzl/oracle-free:23-slim
    ready oracle docker exec dbine-test-ci-oracle healthcheck.sh
    export DBINE_TEST_ORACLE_URL=oracle://dbine:dbine@localhost:25601/FREEPDB1
    export DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1
    export DBINE_TEST_ORACLE_SYSDBA_URL=oracle://sys:Secret123@localhost:25601/FREE
    run_tests oracle
    ;;
  elasticsearch)
    # path.repo: the snapshot tests run on these same servers.
    start elasticsearch -p 25520:9200 -e discovery.type=single-node -e xpack.security.enabled=false \
      -e path.repo=/tmp/snaps -e 'ES_JAVA_OPTS=-Xms512m -Xmx512m' \
      docker.elastic.co/elasticsearch/elasticsearch:8.15.3
    start opensearch -p 25521:9200 -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true \
      -e DISABLE_INSTALL_DEMO_CONFIG=true -e path.repo=/tmp/snaps \
      -e 'OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m' opensearchproject/opensearch:2.17.1
    ready elasticsearch curl -sf 'http://localhost:25520/_cluster/health?wait_for_status=yellow'
    ready opensearch curl -sf 'http://localhost:25521/_cluster/health?wait_for_status=yellow'
    export DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520
    export DBINE_TEST_OPENSEARCH_URL=http://localhost:25521
    export DBINE_TEST_ES_BACKUP_URL=http://localhost:25520
    export DBINE_TEST_OS_BACKUP_URL=http://localhost:25521
    run_tests elasticsearch
    ;;
  *)
    echo "unknown engine: $engine" >&2
    exit 2
    ;;
esac
