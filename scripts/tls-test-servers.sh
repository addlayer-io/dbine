#!/usr/bin/env bash
# Test servers WITH TLS for the TLS-by-default work.
#
# Starts `dbine-test-tls-<engine>` containers that serve the throwaway
# certificates in tests/tls/ (see tests/tls/README.md for the port table,
# credentials and the client options to test with).
#
#   scripts/tls-test-servers.sh [up] [engine...]   (re)create containers (default: all)
#   scripts/tls-test-servers.sh down [engine...]   remove them
#   scripts/tls-test-servers.sh verify [engine...] check TLS is served and verifies
#   scripts/tls-test-servers.sh status             list them
#
# CERT=ca (default) serves server.pem, signed by tests/tls/ca.pem.
# CERT=selfsigned serves selfsigned.pem instead, in containers named
# `dbine-test-tls-ss-<engine>` on the same ports + 500.
#
# It only ever creates or removes containers whose name starts with
# `dbine-test-tls-`. Every other container is left alone.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TLS="$ROOT/tests/tls"
PW='Dbine-Tls-2026'
# Published ports listen on loopback only: the password above is public and
# some servers run without auth. (Docker Desktop still reaches them through
# host.docker.internal for `verify`; on Linux set BIND=<docker bridge IP>.)
BIND="${BIND:-127.0.0.1}"
ALL="postgres mysql mariadb sqlserver mongodb redis cassandra neo4j memgraph clickhouse elasticsearch couchdb etcd trino influxdb solr flightsql"

CERT="${CERT:-ca}"
case "$CERT" in
  ca)         PREFIX=dbine-test-tls-;    OFF=0;   CRT=server.pem;     KEY=server.key;     CA=ca.pem;         P12=server.p12 ;;
  selfsigned) PREFIX=dbine-test-tls-ss-; OFF=500; CRT=selfsigned.pem; KEY=selfsigned.key; CA=selfsigned.pem; P12=selfsigned.p12 ;;
  *) echo "CERT must be ca or selfsigned" >&2; exit 2 ;;
esac

port() { echo $(( $1 + OFF )); }
name() { echo "${PREFIX}$1"; }

# Remove one of our containers (never anything outside dbine-test-tls-*).
rm_ours() {
  local n="$1"
  case "$n" in dbine-test-tls-*) ;; *) echo "refusing to touch $n" >&2; exit 1 ;; esac
  if docker inspect "$n" >/dev/null 2>&1; then docker rm -f "$n" >/dev/null; fi
}

# docker run with the common bits: name, label, the cert dir read-only at /tls.
run() {
  local n="$1"; shift
  rm_ours "$n"
  docker run -d --name "$n" --label dbine.tls-test=1 \
    --add-host host.docker.internal:host-gateway \
    -v "$TLS:/tls:ro" "$@" >/dev/null
  echo "started $n"
}

up_postgres() {
  run "$(name postgres)" -p "$BIND:$(port 26010):5432" -e POSTGRES_PASSWORD="$PW" \
    --entrypoint bash postgres:16 -c "
      install -o postgres -m 600 /tls/$KEY /etc/srv.key
      install -o postgres -m 644 /tls/$CRT /etc/srv.crt
      exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/etc/srv.crt -c ssl_key_file=/etc/srv.key"
}

up_mysql() {
  run "$(name mysql)" -p "$BIND:$(port 26011):3306" -e MYSQL_ROOT_PASSWORD="$PW" \
    mysql:8 --ssl-ca=/tls/$CA --ssl-cert=/tls/$CRT --ssl-key=/tls/$KEY
}

up_mariadb() {
  run "$(name mariadb)" -p "$BIND:$(port 26012):3306" -e MARIADB_ROOT_PASSWORD="$PW" \
    mariadb:11 --ssl-ca=/tls/$CA --ssl-cert=/tls/$CRT --ssl-key=/tls/$KEY
}

up_sqlserver() {
  # forceencryption = 0: clients may still choose Optional/Mandatory/Strict.
  # Strict (TDS 8.0) needs the certificate configured, which this does.
  local conf="[network]
tlscert = /tls/$CRT
tlskey = /tls/$KEY
tlsprotocols = 1.2
forceencryption = 0"
  run "$(name sqlserver)" -p "$BIND:$(port 26013):1433" -m 2g --platform linux/amd64 --hostname localhost \
    -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD="$PW" -e MSSQL_CONF="$conf" \
    --entrypoint /bin/bash mcr.microsoft.com/mssql/server:2022-latest -c '
      printf "%s\n" "$MSSQL_CONF" > /var/opt/mssql/mssql.conf
      exec /opt/mssql/bin/sqlservr'
}

up_mongodb() {
  # preferTLS: TLS and plain on the same port.
  run "$(name mongodb)" -p "$BIND:$(port 26020):27017" \
    -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD="$PW" \
    --entrypoint bash mongo:7 -c "
      cat /tls/$CRT /tls/$KEY > /etc/mongo-tls.pem; chmod 644 /etc/mongo-tls.pem
      exec docker-entrypoint.sh mongod --bind_ip_all --tlsMode preferTLS \
        --tlsCertificateKeyFile /etc/mongo-tls.pem --tlsCAFile /tls/$CA \
        --tlsAllowConnectionsWithoutCertificates"
}

up_redis() {
  run "$(name redis)" -p "$BIND:$(port 26030):6380" -p "$BIND:$(port 26031):6379" redis:7 \
    redis-server --requirepass "$PW" --port 6379 --tls-port 6380 \
      --tls-cert-file /tls/$CRT --tls-key-file /tls/$KEY --tls-ca-cert-file /tls/$CA \
      --tls-auth-clients no
}

up_cassandra() {
  # optional: true -> TLS and plain on the same native port.
  run "$(name cassandra)" -p "$BIND:$(port 26040):9042" -m 1600m \
    -e MAX_HEAP_SIZE=512M -e HEAP_NEWSIZE=128M -e CASSANDRA_PEM="/tls/$CRT /tls/$KEY" \
    --entrypoint bash cassandra:5 -c '
      Y=/etc/cassandra/cassandra.yaml
      cat $CASSANDRA_PEM > /etc/cassandra/tls.pem; chmod 644 /etc/cassandra/tls.pem
      awk "/^client_encryption_options:/{skip=1; next} skip && /^[a-z_]+:/{skip=0} !skip" $Y > /tmp/c.yaml
      cat >> /tmp/c.yaml <<EOF
client_encryption_options:
  enabled: true
  optional: true
  require_client_auth: false
  keystore: /etc/cassandra/tls.pem
  ssl_context_factory:
    class_name: org.apache.cassandra.security.PEMBasedSslContextFactory
EOF
      sed -i "s/^authenticator: AllowAllAuthenticator/authenticator: PasswordAuthenticator/" /tmp/c.yaml
      cp /tmp/c.yaml $Y
      exec docker-entrypoint.sh cassandra -f'
}

up_neo4j() {
  # Bolt: tls_level OPTIONAL (bolt and bolt+s on one port). HTTPS and HTTP both on.
  run "$(name neo4j)" -p "$BIND:$(port 26050):7687" -p "$BIND:$(port 26051):7473" -p "$BIND:$(port 26052):7474" -m 1200m \
    -e NEO4J_AUTH="neo4j/$PW" \
    -e NEO4J_server_memory_heap_max__size=512m -e NEO4J_server_memory_pagecache_size=128m \
    -e NEO4J_dbms_ssl_policy_bolt_enabled=true -e NEO4J_dbms_ssl_policy_bolt_base__directory=/ssl \
    -e NEO4J_dbms_ssl_policy_bolt_client__auth=NONE -e NEO4J_server_bolt_tls__level=OPTIONAL \
    -e NEO4J_dbms_ssl_policy_https_enabled=true -e NEO4J_dbms_ssl_policy_https_base__directory=/ssl \
    -e NEO4J_dbms_ssl_policy_https_client__auth=NONE -e NEO4J_server_https_enabled=true \
    --entrypoint bash neo4j:5 -c "
      mkdir -p /ssl/trusted /ssl/revoked
      cp /tls/$KEY /ssl/private.key; cp /tls/$CRT /ssl/public.crt; cp /tls/$CA /ssl/trusted/ca.crt
      chown -R neo4j:neo4j /ssl
      exec /startup/docker-entrypoint.sh neo4j"
}

up_memgraph() {
  # Memgraph (neo4j driver crate) serves Bolt TLS only once a cert is set.
  run "$(name memgraph)" -p "$BIND:$(port 26055):7687" memgraph/memgraph \
    --bolt-cert-file=/tls/$CRT --bolt-key-file=/tls/$KEY --also-log-to-stderr=false
}

up_clickhouse() {
  local xml="<clickhouse>
  <https_port>8443</https_port>
  <tcp_port_secure>9440</tcp_port_secure>
  <openSSL><server>
    <certificateFile>/tls/$CRT</certificateFile>
    <privateKeyFile>/tls/$KEY</privateKeyFile>
    <caConfig>/tls/$CA</caConfig>
    <verificationMode>none</verificationMode>
    <loadDefaultCAFile>false</loadDefaultCAFile>
    <cacheSessions>true</cacheSessions>
    <preferServerCiphers>true</preferServerCiphers>
  </server></openSSL>
</clickhouse>"
  run "$(name clickhouse)" -p "$BIND:$(port 26060):8443" -p "$BIND:$(port 26061):9440" \
    -p "$BIND:$(port 26062):8123" -p "$BIND:$(port 26063):9000" --ulimit nofile=262144:262144 \
    -e CLICKHOUSE_USER=default -e CLICKHOUSE_PASSWORD="$PW" -e TLS_XML="$xml" \
    --entrypoint bash clickhouse/clickhouse-server:latest -c '
      printf "%s\n" "$TLS_XML" > /etc/clickhouse-server/config.d/dbine-tls.xml
      exec /entrypoint.sh'
}

up_elasticsearch() {
  local c=/usr/share/elasticsearch/config/certs
  rm_ours "$(name elasticsearch)"
  docker run -d --name "$(name elasticsearch)" --label dbine.tls-test=1 \
    -p "$BIND:$(port 26070):9200" -m 1200m -v "$TLS:$c:ro" \
    -e discovery.type=single-node -e ES_JAVA_OPTS="-Xms512m -Xmx512m" \
    -e ELASTIC_PASSWORD="$PW" -e xpack.security.enabled=true \
    -e xpack.security.http.ssl.enabled=true \
    -e xpack.security.http.ssl.certificate="$c/$CRT" -e xpack.security.http.ssl.key="$c/$KEY" \
    -e xpack.security.http.ssl.certificate_authorities="$c/$CA" \
    -e xpack.security.transport.ssl.enabled=false \
    docker.elastic.co/elasticsearch/elasticsearch:8.15.3 >/dev/null
  echo "started $(name elasticsearch)"
}

up_couchdb() {
  local ini="[ssl]
enable = true
cert_file = /tls/$CRT
key_file = /tls/$KEY
cacert_file = /tls/$CA"
  run "$(name couchdb)" -p "$BIND:$(port 26080):6984" -p "$BIND:$(port 26081):5984" \
    -e COUCHDB_USER=admin -e COUCHDB_PASSWORD="$PW" -e TLS_INI="$ini" \
    --entrypoint bash couchdb:3 -c '
      printf "%s\n" "$TLS_INI" > /opt/couchdb/etc/local.d/zz-dbine-tls.ini
      exec tini -- /docker-entrypoint.sh /opt/couchdb/bin/couchdb'
}

up_etcd() {
  run "$(name etcd)" -p "$BIND:$(port 26090):2379" -p "$BIND:$(port 26091):2382" quay.io/coreos/etcd:v3.5.17 \
    etcd --name tls --data-dir /tmp/etcd \
      --listen-client-urls https://0.0.0.0:2379,http://0.0.0.0:2382 \
      --advertise-client-urls https://localhost:2379 \
      --cert-file /tls/$CRT --key-file /tls/$KEY
}

up_trino() {
  local props="http-server.https.enabled=true
http-server.https.port=8443
http-server.https.keystore.path=/tmp/etc/https.pem"
  run "$(name trino)" -p "$BIND:$(port 26100):8443" -p "$BIND:$(port 26101):8080" -m 1600m \
    -e TLS_PROPS="$props" -e PEM="/tls/$CRT /tls/$KEY" \
    --entrypoint bash trinodb/trino:latest -c '
      cp -r /etc/trino /tmp/etc
      sed -i "s/^-Xmx.*/-Xmx1G/" /tmp/etc/jvm.config
      cat $PEM > /tmp/etc/https.pem
      printf "\n%s\n" "$TLS_PROPS" >> /tmp/etc/config.properties
      exec /usr/lib/trino/bin/launcher run --etc-dir /tmp/etc'
}

up_influxdb() {
  run "$(name influxdb)" -p "$BIND:$(port 26110):8086" \
    -e DOCKER_INFLUXDB_INIT_MODE=setup -e DOCKER_INFLUXDB_INIT_USERNAME=admin \
    -e DOCKER_INFLUXDB_INIT_PASSWORD="$PW" -e DOCKER_INFLUXDB_INIT_ORG=dbine \
    -e DOCKER_INFLUXDB_INIT_BUCKET=dbine -e DOCKER_INFLUXDB_INIT_ADMIN_TOKEN=dbine-tls-token \
    -e INFLUXD_TLS_CERT=/tls/$CRT -e INFLUXD_TLS_KEY=/tls/$KEY influxdb:2
}

up_solr() {
  run "$(name solr)" -p "$BIND:$(port 26120):8983" -m 900m -e SOLR_HEAP=512m \
    -e SOLR_SSL_ENABLED=true -e SOLR_SSL_KEY_STORE=/tls/$P12 -e SOLR_SSL_KEY_STORE_TYPE=PKCS12 \
    -e SOLR_SSL_KEY_STORE_PASSWORD=dbine-test -e SOLR_SSL_TRUST_STORE=/tls/$P12 \
    -e SOLR_SSL_TRUST_STORE_TYPE=PKCS12 -e SOLR_SSL_TRUST_STORE_PASSWORD=dbine-test \
    -e SOLR_SSL_NEED_CLIENT_AUTH=false -e SOLR_SSL_WANT_CLIENT_AUTH=false \
    -e SOLR_SSL_CHECK_PEER_NAME=false solr:9 solr-precreate dbine
}

up_flightsql() {
  # GizmoSQL: --tls <cert> <key> (gRPC over TLS only on this port).
  run "$(name flightsql)" -p "$BIND:$(port 26130):31337" -e TLS_ENABLED=0 \
    -e GIZMOSQL_USERNAME=gizmosql_username -e GIZMOSQL_PASSWORD="$PW" -e PRINT_QUERIES=0 \
    --entrypoint gizmosql_server gizmodata/gizmosql:latest \
    --database-filename /tmp/dbine.duckdb --tls /tls/$CRT /tls/$KEY
}

# ---------------------------------------------------------------- verify --

# TLS handshake from the host with openssl. Prints the verify result with the
# test trust anchor, then with a wrong anchor and a wrong host name (both must fail).
sclient() {  # label port [extra s_client args...]
  local label="$1" p="$2"; shift 2
  local good bad host
  good=$(echo | openssl s_client -connect "127.0.0.1:$p" -servername localhost "$@" \
    -CAfile "$TLS/$CA" -verify_hostname localhost 2>/dev/null | grep -E 'Verify return code' | head -1 | sed 's/^ *//')
  local other=ca.pem; [ "$CA" = ca.pem ] && other=selfsigned.pem
  bad=$(echo | openssl s_client -connect "127.0.0.1:$p" "$@" -CAfile "$TLS/$other" \
    2>/dev/null | grep -E 'Verify return code' | head -1 | sed 's/^ *//')
  host=$(echo | openssl s_client -connect "127.0.0.1:$p" "$@" -CAfile "$TLS/$CA" \
    -verify_hostname wrong.example 2>/dev/null | grep -E 'Verify return code' | head -1 | sed 's/^ *//')
  printf '%-28s %-5s right CA: %s | wrong CA: %s | wrong host: %s\n' "$label" "$p" "${good:-NO TLS}" "${bad:-NO TLS}" "${host:-NO TLS}"
}

xc() { docker exec "$@" 2>&1 | tr -d '\r' | grep -v -i -E 'warning|deprecated' | tail -3 | tr '\n' ' '; echo; }

v_postgres() {
  local n; n=$(name postgres); local p; p=$(port 26010)
  sclient postgres "$p" -starttls postgres
  printf '  psql verify-full via host.docker.internal: '; xc -e PGPASSWORD="$PW" "$n" psql "host=host.docker.internal port=$p user=postgres dbname=postgres sslmode=verify-full sslrootcert=/tls/$CA" -tAc 'select ssl, version from pg_stat_ssl where pid = pg_backend_pid()'
  printf '  psql sslmode=disable (plain): '; xc -e PGPASSWORD="$PW" "$n" psql "host=host.docker.internal port=$p user=postgres dbname=postgres sslmode=disable" -tAc 'select ssl from pg_stat_ssl where pid = pg_backend_pid()'
}
v_mysql() {
  local n; n=$(name mysql); local p; p=$(port 26011)
  sclient mysql "$p" -starttls mysql
  printf '  mysql VERIFY_IDENTITY: '; xc "$n" mysql -h host.docker.internal -P "$p" -uroot -p"$PW" --ssl-mode=VERIFY_IDENTITY --ssl-ca=/tls/$CA -N -e "SHOW SESSION STATUS LIKE 'Ssl_version'"
  printf '  mysql DISABLED (plain): '; xc "$n" mysql -h host.docker.internal -P "$p" -uroot -p"$PW" --ssl-mode=DISABLED -N -e "SHOW SESSION STATUS LIKE 'Ssl_version'"
}
v_mariadb() {
  local n; n=$(name mariadb); local p; p=$(port 26012)
  sclient mariadb "$p" -starttls mysql
  printf '  mariadb --ssl-verify-server-cert: '; xc "$n" mariadb -h host.docker.internal -P "$p" -uroot -p"$PW" --ssl --ssl-ca=/tls/$CA --ssl-verify-server-cert -N -e "SHOW SESSION STATUS LIKE 'Ssl_version'"
  printf '  mariadb --skip-ssl (plain): '; xc "$n" mariadb -h host.docker.internal -P "$p" -uroot -p"$PW" --skip-ssl -N -e "SHOW SESSION STATUS LIKE 'Ssl_version'"
}
v_sqlserver() {
  local n; n=$(name sqlserver); local p; p=$(port 26013)
  # TLS runs inside TDS prelogin, so openssl s_client can't do the handshake;
  # sqlcmd (ODBC 18) checks the chain and the host name once the CA is trusted.
  local q="select encrypt_option from sys.dm_exec_connections where session_id = @@SPID"
  docker exec -u root "$n" bash -c "rm -f /usr/local/share/ca-certificates/dbine-test-ca.crt && update-ca-certificates --fresh >/dev/null 2>&1"
  printf '  sqlcmd -N, CA not trusted (must fail): '; xc "$n" /opt/mssql-tools18/bin/sqlcmd -S "host.docker.internal,$p" -U sa -P "$PW" -N -h -1 -Q "set nocount on; $q"
  docker exec -u root "$n" bash -c "cp /tls/$CA /usr/local/share/ca-certificates/dbine-test-ca.crt && update-ca-certificates >/dev/null 2>&1"
  printf '  sqlcmd -N, CA trusted (verified): '; xc "$n" /opt/mssql-tools18/bin/sqlcmd -S "host.docker.internal,$p" -U sa -P "$PW" -N -h -1 -Q "set nocount on; $q"
  local ip; ip=$(docker exec "$n" getent hosts host.docker.internal | awk '{print $1}')
  printf '  sqlcmd -N, CA trusted, IP not in SAN (must fail): '; xc "$n" /opt/mssql-tools18/bin/sqlcmd -S "$ip,$p" -U sa -P "$PW" -N -h -1 -Q "set nocount on; $q"
  printf '  sqlcmd -N -C (encrypt + trust cert): '; xc "$n" /opt/mssql-tools18/bin/sqlcmd -S "host.docker.internal,$p" -U sa -P "$PW" -N -C -h -1 -Q "set nocount on; $q"
  printf '  sqlcmd -No (optional / plain login path): '; xc "$n" /opt/mssql-tools18/bin/sqlcmd -S "host.docker.internal,$p" -U sa -P "$PW" -No -h -1 -Q "set nocount on; $q"
}
v_mongodb() {
  local n; n=$(name mongodb); local p; p=$(port 26020)
  sclient mongodb "$p"
  printf '  mongosh tls + CA: '; xc "$n" mongosh --quiet "mongodb://root:$PW@host.docker.internal:$p/?tls=true&tlsCAFile=/tls/$CA" --eval 'db.runCommand({ping:1}).ok'
  printf '  mongosh plain: '; xc "$n" mongosh --quiet "mongodb://root:$PW@host.docker.internal:$p/?tls=false" --eval 'db.runCommand({ping:1}).ok'
}
v_redis() {
  local n; n=$(name redis); local p; p=$(port 26030)
  sclient 'redis (rediss)' "$p"
  printf '  redis-cli --tls --cacert: '; xc "$n" redis-cli -h host.docker.internal -p "$p" --tls --cacert /tls/$CA -a "$PW" --no-auth-warning ping
  printf '  redis-cli plain :%s: ' "$(port 26031)"; xc "$n" redis-cli -h host.docker.internal -p "$BIND:$(port 26031)" -a "$PW" --no-auth-warning ping
}
v_cassandra() {
  local n; n=$(name cassandra); local p; p=$(port 26040)
  sclient cassandra "$p"
  printf '  cqlsh --ssl (validate, CA): '; xc -e SSL_CERTFILE=/tls/$CA -e SSL_VALIDATE=true "$n" cqlsh --ssl -u cassandra -p cassandra host.docker.internal "$p" -e 'select release_version from system.local'
  printf '  cqlsh plain: '; xc "$n" cqlsh -u cassandra -p cassandra host.docker.internal "$p" -e 'select release_version from system.local'
}
v_neo4j() {
  local n; n=$(name neo4j)
  sclient 'neo4j bolt+s' "$(port 26050)"
  sclient 'neo4j https' "$(port 26051)"
  printf '  cypher-shell bolt+ssc: '; xc "$n" cypher-shell -a "bolt+ssc://host.docker.internal:$(port 26050)" -u neo4j -p "$PW" 'RETURN 1 AS ok'
  printf '  cypher-shell bolt (plain): '; xc "$n" cypher-shell -a "bolt://host.docker.internal:$(port 26050)" -u neo4j -p "$PW" 'RETURN 1 AS ok'
}
v_memgraph() { sclient 'memgraph bolt+s' "$(port 26055)"; }
v_clickhouse() {
  sclient 'clickhouse https' "$(port 26060)"
  sclient 'clickhouse native TLS' "$(port 26061)"
  printf '  curl https --cacert: '; curl -sS --cacert "$TLS/$CA" -u "default:$PW" "https://localhost:$(port 26060)/?query=select%20version()"
  printf '  curl http (plain): '; curl -sS -u "default:$PW" "http://localhost:$(port 26062)/?query=select%201"
}
v_elasticsearch() {
  sclient 'elasticsearch https' "$(port 26070)"
  printf '  curl --cacert: '; curl -sS --cacert "$TLS/$CA" -u "elastic:$PW" "https://localhost:$(port 26070)/_cluster/health?filter_path=status"; echo
}
v_couchdb() {
  sclient 'couchdb https' "$(port 26080)"
  printf '  curl https --cacert: '; curl -sS --cacert "$TLS/$CA" -u "admin:$PW" "https://localhost:$(port 26080)/_up"
  printf '  curl http (plain): '; curl -sS -u "admin:$PW" "http://localhost:$(port 26081)/_up"
}
v_etcd() {
  sclient 'etcd https' "$(port 26090)"
  printf '  curl https --cacert: '; curl -sS --cacert "$TLS/$CA" "https://localhost:$(port 26090)/version"; echo
  printf '  curl http (plain): '; curl -sS "http://localhost:$(port 26091)/version"; echo
}
v_trino() {
  sclient 'trino https' "$(port 26100)"
  printf '  curl https --cacert: '; curl -sS --cacert "$TLS/$CA" "https://localhost:$(port 26100)/v1/info" | cut -c1-90; echo
  printf '  curl http (plain): '; curl -sS "http://localhost:$(port 26101)/v1/info" | cut -c1-90; echo
}
v_influxdb() {
  sclient 'influxdb https' "$(port 26110)"
  printf '  curl https --cacert: '; curl -sS --cacert "$TLS/$CA" "https://localhost:$(port 26110)/health" | cut -c1-90; echo
}
v_solr() {
  sclient 'solr https' "$(port 26120)"
  printf '  curl https --cacert: '; curl -sS --cacert "$TLS/$CA" "https://localhost:$(port 26120)/solr/admin/info/system?wt=json" | grep -o '"solr-spec-version":"[^"]*"'
}
v_flightsql() { sclient 'flightsql (grpc+tls)' "$(port 26130)" -alpn h2; }

# ------------------------------------------------------------------ main --

cmd="${1:-up}"
case "$cmd" in up|down|verify|status) shift || true ;; *) cmd=up ;; esac
engines="${*:-$ALL}"

case "$cmd" in
  up)     for e in $engines; do "up_$e"; done ;;
  down)   for e in $engines; do rm_ours "$(name "$e")"; echo "removed $(name "$e")"; done ;;
  verify) for e in $engines; do "v_$e" || true; done ;;
  status) docker ps -a --filter "name=^dbine-test-tls-" --format '{{.Names}}\t{{.Status}}\t{{.Ports}}' ;;
esac
