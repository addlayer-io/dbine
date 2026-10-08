# Soporte por motor

Este documento sigue la regla principal de `AGENTS.md`: toda función de DBine
tiene que estar en todos los motores. Acá se listan las excepciones, con su
motivo, y el estado de verificación de cada motor. Cuando se agregue una
función, se suma su sección.

## Estructura, diseño y operaciones de base

Esta sección cubre varias funciones que comparten el mismo contrato: el diseñador (nueva tabla, colección, índice o key), el DDL en el lenguaje del motor, la estructura con claves foráneas e índices (para el diagrama ER y el generador), las plantillas de otros objetos, crear y borrar bases, y los scripts de inserción (para copiar, importar y volcar datos).

Hay tres funciones que dependen solo de la app, no de cada motor, así que funcionan en todos:

- la **copia de resultados** en 10 formatos;
- **revelar una pestaña** en el explorador (doble clic) e **"Ir a la base"**;
- la **importación**: los archivos se leen en la app y cada motor recibe su propio script de inserción.

**Probado de punta a punta en la app:**

- **SQLite:**
  - estructura con clave foránea y diagrama;
  - script completo (tablas, trigger, vista y 20.500 filas) restaurado en otra base, con los mismos conteos y totales;
  - importación de CSV y de Excel de 20.000 filas;
  - diseñador que crea una tabla;
  - revelar la pestaña e "Ir a la base".
- **SQL Server 2022:**
  - crear y borrar bases;
  - DDL con identidad, comentarios, índice filtrado y clave foránea con CASCADE;
  - copia de la base con datos, cuya estructura quedó idéntica;
  - la identidad sigue bien después de la carga (`IDENTITY_INSERT`).
- **PostgreSQL 16:** copia con datos y vista; las secuencias quedan después de lo cargado y la estructura es idéntica.

### Por motor

| Motor | Diseñador crea | Estructura (claves foráneas e índices) | Crear/borrar base | Notas |
|---|---|---|---|---|
| SQL Server | tabla | sí | sí | Comentarios como `MS_Description`; `IDENTITY_INSERT` al volcar datos. Los nombres de las restricciones DEFAULT no se conservan. |
| PostgreSQL, TimescaleDB, YugabyteDB, KingbaseES | tabla | sí | sí | Secuencias resincronizadas después de los datos. |
| CockroachDB | tabla | sí | sí | `unique_rowid()` queda como valor por defecto. |
| Redshift | tabla | sí (informativas) | sí | Opciones DISTSTYLE/DISTKEY/SORTKEY; no tiene índices. |
| Greenplum | tabla | sin claves foráneas | sí | DISTRIBUTED BY. |
| Denodo | — | sin claves foráneas | no | Es una capa virtual: no hay diseñador ni DDL. |
| MySQL, MariaDB, TiDB, OceanBase | tabla | sí | sí | Los índices funcionales se omiten. |
| SingleStore, StarRocks, Doris, Databend, GreptimeDB | tabla | sin claves foráneas | sí | StarRocks y Doris tienen modelo de claves, distribución y buckets. |
| Manticore | tabla | sin claves foráneas | no | — |
| SQLite | tabla | sí | no | Una base es un archivo; las claves foráneas vuelven sin nombre. |
| Oracle | tabla | sí | sí (como schema/usuario, 18c+) | Identidad reubicada después de los datos. Al copiar datos hacia Oracle, un texto o binario vacío en una columna que no es CLOB/BLOB detiene la carga: Oracle lo guardaría como NULL. Las regiones horarias (`America/Argentina/Buenos_Aires`) y los escalares del JSON extendido solo se conservan de Oracle a Oracle; hacia otros motores viajan como desplazamiento (el mismo instante) y como JSON estándar. Las fechas antes del año 1 viajan como texto con ` BC`. |
| Firebird | tabla | sí | sí (vía API) | En los scripts va el tipo base, no el dominio. |
| SAP HANA | tabla | sí | sí (como schema) | — |
| Aurora DSQL | tabla | sin claves foráneas | no | Una sola base. |
| Cloud Spanner | tabla (con interleaving) | sí | sí (API de administración) | El orden descendente de la clave primaria no se lee de vuelta. |
| DuckDB | tabla | sí (índices ART) | sí (ATTACH/DETACH de archivos) | Autoincremento con secuencias; las claves foráneas van dentro del CREATE TABLE. La librería se descarga en la primera conexión (13 a 41 MB según la plataforma). |
| ClickHouse, Timeplus | tabla / stream | índices de salto; sin claves foráneas | sí en ClickHouse | ENGINE, ORDER BY, PARTITION BY, TTL y codecs; Timeplus con modos de stream. |
| Trino, Presto, Starburst | tabla | sin claves foráneas ni índices | no (los catálogos se configuran en el servidor) | Las propiedades `WITH` no se leen de vuelta. Starburst sin probar. |
| Athena | tabla (Iceberg o externa) | sin claves foráneas ni índices | sí | Solo tests unitarios. |
| BigQuery | tabla | sí (claves NOT ENFORCED) | sí (datasets) | Sin índices ni autoincremento. |
| Snowflake | tabla | sí | sí | Tablas transitorias y clustering key. |
| Databricks | tabla (Delta) | sí | sí (catálogos) | Sin índices; liquid clustering no se lee de vuelta. |
| Phoenix | tabla | sin claves foráneas | no | Salting, familias de columnas e índices locales y globales. |
| ksqlDB | stream / tabla | — | no | Las ventanas de las tablas con ventana no se leen de vuelta. |
| IoTDB | dispositivo | — | sí | Codificación y compresión por medida. |
| InfluxDB 1 / 2 / 3 | — | medidas, tags y campos | sí (bases o buckets) | Las medidas se crean al escribir datos: no hay diseñador ni INSERT. |
| MongoDB | colección (con validador `$jsonSchema`) | índices; sin claves foráneas | sí | Colecciones limitadas (capped), series temporales, colecciones agrupadas y TTL. |
| CouchDB | — (sin esquema) | índices Mango | sí | Vistas e índices salen de plantillas. |
| Cosmos DB | contenedor | claves únicas e índices compuestos | sí | Extensión: `CREATE CONTAINER` / `DROP CONTAINER`. |
| DynamoDB | tabla (claves, GSI y LSI) | índices; sin claves foráneas | no (no hay bases) | Extensión: `CREATE TABLE {json}`, `CREATE INDEX`, `DROP`. |
| Elasticsearch, OpenSearch | índice (mappings) | campos del mapping | no (no hay bases) | Inserción por `_bulk`. |
| Solr | colección / core | campos | no | Extensión: `PUT` / `DELETE /solr/<nombre>`. En modo standalone los cores comparten el configset. |
| Redis, Valkey, Dragonfly | key (tipo, valores, TTL) | — (las keys no son tablas) | no | Las bases son un conjunto fijo numerado. |
| Cassandra, ScyllaDB | tabla (partición y clustering) | índices; sin claves foráneas | sí (keyspaces) | Las vistas materializadas y las UDF requieren habilitarse en cassandra.yaml. |
| ODBC (39 presets) | tabla | sí en la mayoría | según el preset | Solo el preset genérico se probó contra un servidor (SQL Server vía ODBC). NetSuite no tiene diseñador. |
| AlloyDB, Cloud SQL y Aurora PostgreSQL, EDB, Fujitsu | tabla | sí | sí | Como PostgreSQL. Los administrados vienen con TLS y aceptan un certificado de CA. |
| openGauss | tabla | sí | sí | Sin columnas de identidad (van como `serial`). Solo entra con contraseñas MD5: tokio-postgres no implementa SHA-256 ni SM3. |
| Cloudberry, Greengage | tabla | sin claves foráneas | sí | DISTRIBUTED BY, como Greenplum. |
| Materialize, RisingWave | tabla | sin claves foráneas | sí | Sin identidad; Materialize sin PRIMARY KEY. |
| CrateDB | tabla (shards y réplicas) | sin claves foráneas ni índices | no | Sin COMMENT. |
| Yellowbrick, H2 (modo `-pg`) | tabla | sí | sí en Yellowbrick | H2 sin TLS. |
| Aurora MySQL, Cloud SQL para MySQL | tabla | sí | sí | Como MySQL, con TLS y certificado de CA. Cloud SQL sin certificados de cliente: usar el Cloud SQL Auth Proxy. |
| VeloDB | tabla | sin claves foráneas | sí | Como Doris. |
| Azure SQL Database | tabla | sí | sí | Login SQL o Microsoft Entra ID. |
| Fabric Warehouse | tabla | claves `NOT ENFORCED`, sin índices | no | Solo Entra ID; los almacenes se crean en el portal; sin triggers. |
| Babelfish | tabla | sí | sí | T-SQL sobre PostgreSQL. |
| Oracle Autonomous | tabla | sí | como Oracle | Wallet con `ewallet.pem` o cadena TLS; `cwallet.sso` y `.p12` no. |
| Archivos CSV / Parquet / JSON | — (cada archivo es una vista) | — | no | Vía DuckDB; Excel no (necesita una extensión que no viene incluida). |
| libSQL / Turso | tabla | sí | no (API de la plataforma) | Sin cancelación: Hrana HTTP no tiene pedido de cancelación. |
| Azure Databricks | tabla (Delta) | sí | sí (catálogos) | Como Databricks, con token o service principal de Entra ID. |
| Apache Calcite Avatica | — | — | no | Sin diseñador, DDL, definiciones ni monitor: dependen de la base detrás del servidor. |
| FerretDB | colección | índices | sí | Sin cancelación (`killOp` no existe en FerretDB 2). |
| Amazon DocumentDB | colección | índices | sí | TLS con el bundle de AWS; `retryWrites` desactivado. |
| Amazon Keyspaces | tabla | — | sí | Sin vistas materializadas, índices, UDF ni opciones de compactación. Credenciales específicas del servicio: el cliente CQL no tiene SigV4. |
| Open Distro | índice | campos del mapping | no | Sin data streams ni `flat_object`. |
| TimechoDB | dispositivo | — | sí | Como IoTDB. |
| Neo4j, Memgraph | índice / restricción | índices y restricciones | sí en Neo4j Enterprise | Cypher. En Neo4j Community no se crean bases. |
| Amazon Neptune | — | — | no | openCypher por HTTPS; el motor no tiene índices, restricciones ni bases de usuario. |
| OrientDB | clase (vértice, arista o documento) | índices; LINK como clave foránea | sí | Gremlin solo con las imágenes `-tp3`. |
| Couchbase | colección (índice primario e índices) | índices; sin claves foráneas | sí (buckets) | Esquema `bucket.scope`; `maxTTL` solo en Enterprise. |
| TDengine | tabla / supertabla (tags) | sin claves foráneas ni índices | sí | Vistas solo en Enterprise. |
| Apache Drill | — (solo CREATE TABLE AS) | sin claves foráneas ni índices | no (workspaces del plugin) | Sin INSERT: no hay script de inserción. |
| Dremio | tabla Iceberg (PARTITION BY, LOCALSORT) | sin claves foráneas ni índices | sí (espacios) | No conserva NOT NULL. |
| etcd | clave (valor y TTL) | — | no (un solo espacio de claves) | Comandos de etcdctl; `watch`, `lock` y `elect` se rechazan. |
| Arrow Flight SQL | — | según el motor | no | Genérico (GizmoSQL, Dremio, InfluxDB 3, Doris). Sin "confiar en el certificado": tonic no lo permite. |

## Exportación de resultados

Soportada en **todos los motores**, sin excepciones.

**Formatos:** JSON, JSON Lines/NDJSON, SQL (INSERT), CSV, CSV con punto y
coma, CSV para Excel (UTF-8 con BOM, `;` y CRLF), TSV, Excel (.xlsx) y XML.

**Dónde:**
- **Menú "Exportar"** de cada resultado: escribe al instante las filas
  cargadas.
- **Exportación avanzada (⌘E):** agrega las opciones de cada formato y la
  opción **todas las filas**, que vuelve a ejecutar la consulta y escribe el
  archivo a medida que llegan las filas. En una tabla, "todas" es la tabla
  entera.

**Cómo funciona "todas las filas":**
- Todos los drivers entregan sus filas por `QueryOutcome::push_row`, y la
  exportación conecta ahí un destino (`RowSink`), sin cambios en los drivers.
  Los drivers que arman un resultado local usan `out.fork()` / `out.merge()`.
- La reejecución corre en una **sesión de solo lectura**, así que una
  exportación nunca escribe en la base.
- Se puede cancelar, y el archivo a medio escribir se borra.
- Medido: 3 millones de filas de PostgreSQL, un CSV de 218 MB, sin que suba
  la memoria de la app.

**Limitaciones:**
- Excel admite hasta 1.048.576 filas por hoja; para más, conviene CSV.
- En Elasticsearch/OpenSearch, "todas las filas" de un índice está limitado
  por `index.max_result_window` del servidor (10.000 por defecto).

## Gráficos de resultados

Soportado en **todos los motores**, sin excepciones: los gráficos trabajan
sobre la grilla de resultados, que tiene el mismo formato en todos los drivers.
Cada resultado tiene un selector "Tabla | Gráfico".

Tipos de gráfico: barras, barras horizontales, barras apiladas, líneas, área,
torta y dispersión. Además, "Dividir por" arma una serie por cada valor de una
columna, y hay agregaciones (suma, promedio, cantidad, mínimo, máximo) y
exportación a PNG.

## Planes de ejecución

En el editor hay tres acciones:

| Acción | Atajo | Qué hace |
|---|---|---|
| **Plan estimado** | ⌘L | Muestra el plan sin ejecutar nada, tampoco las escrituras. |
| **Ejecutar + plan** | ⌘⇧L | Ejecuta el script y muestra los resultados más el plan con cifras reales. |

Reglas comunes a todos los motores:

- Las escrituras nunca se ejecutan dos veces.
- Si el motor solo puede dar cifras reales ejecutando la sentencia otra vez
  (`EXPLAIN ANALYZE`), la repetición se hace únicamente con lecturas. Las
  escrituras muestran el plan estimado.

El diagrama sigue el estilo de SSMS: la sentencia a la izquierda, los
operadores a la derecha y las flechas más gruesas cuanto más filas pasan.

**Leyenda de la columna "Verificado":**
- **vivo:** probado contra un servidor real o un emulador en Docker.
- **fixture:** probado con planes grabados, sin servidor.
- **emulador sin planes:** hay emulador, pero no genera planes reales.

### Relacionales

| Motor | Estimado | Real | Verificado |
|---|---|---|---|
| SQL Server | `SHOWPLAN_XML` | `STATISTICS XML` (ejecuta una vez) | vivo |
| PostgreSQL, TimescaleDB, YugabyteDB, Greenplum, KingbaseES | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE`, solo en lecturas | vivo (PostgreSQL) |
| CockroachDB | `EXPLAIN (VERBOSE)` | `EXPLAIN ANALYZE` | vivo |
| Redshift | `EXPLAIN` en texto | estimado + resultados | fixture |
| MySQL 8 | `FORMAT=TREE` | `EXPLAIN ANALYZE` | vivo |
| MariaDB | `EXPLAIN FORMAT=JSON` | `ANALYZE FORMAT=JSON` | vivo |
| TiDB | `EXPLAIN FORMAT='brief'` | `EXPLAIN ANALYZE` | vivo |
| MySQL 5.7, OceanBase, SingleStore, StarRocks, Doris, Databend, GreptimeDB | `EXPLAIN` tabular o en texto | estimado + resultados | fixture |
| SQLite | `EXPLAIN QUERY PLAN` (sin costos) | estimado + resultados | vivo |
| Oracle | `EXPLAIN PLAN` | cursor recién ejecutado; necesita `SELECT_CATALOG_ROLE` | vivo |
| Firebird 3+ | `MON$EXPLAINED_PLAN` (solo prepara la sentencia) | estimado + resultados | vivo (Firebird 5) |
| SAP HANA | `EXPLAIN PLAN` | estimado + resultados | fixture |
| Aurora DSQL | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE`, solo en lecturas | vivo (vía PostgreSQL) |
| Cloud Spanner | `queryMode: PLAN` | `queryMode: PROFILE` | emulador sin planes + fixture |
| Denodo, Manticore | **no** | **no** | Ninguno expone planes por SQL en un formato utilizable. |
| openGauss, Cloudberry, Greengage, AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE`, solo en lecturas | vivo (openGauss, Cloudberry, Greengage; el resto vía PostgreSQL) |
| RisingWave, Materialize, CrateDB, H2 | `EXPLAIN` en texto | estimado + resultados | vivo |
| Babelfish | plan de PostgreSQL en texto | real | vivo |
| Azure SQL, Fabric | como SQL Server | como SQL Server | fixture (el plan real de Fabric sin verificar) |
| Neo4j, Memgraph | `EXPLAIN` | `PROFILE` | vivo |
| Amazon Neptune | `explain=static` | `explain=dynamic` | fixture |
| OrientDB | `EXPLAIN` | `PROFILE` | vivo |

### Analíticas y en la nube

| Motor | Estimado | Real | Verificado |
|---|---|---|---|
| DuckDB | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE` | vivo |
| ClickHouse, Timeplus | `EXPLAIN json=1, indexes=1` (solo SELECT) | estimado + resultados | vivo (ClickHouse) |
| Trino, Presto, Starburst | `EXPLAIN (FORMAT JSON)` | `EXPLAIN ANALYZE` | vivo (Trino) |
| BigQuery | dry run: bytes y tablas. Todavía no hay árbol de operadores, porque BigQuery arma las etapas recién al ejecutar. | etapas del job | emulador sin planes + fixture |
| Athena | `EXPLAIN (FORMAT JSON)` | `GetQueryRuntimeStatistics` | fixture |
| Snowflake | `EXPLAIN USING JSON` (sin costos ni filas estimadas) | `GET_QUERY_OPERATOR_STATS` | fixture |
| Databricks | `EXPLAIN FORMATTED` | solo totales de la query; no hay métricas por operador fuera de la Spark UI | fixture |
| ksqlDB | `EXPLAIN` | estimado + resultados | vivo |
| Phoenix | `EXPLAIN` | estimado + resultados | vivo |

### Documentos, búsqueda, clave-valor y series de tiempo

| Motor | Estimado | Real | Verificado |
|---|---|---|---|
| MongoDB | `explain` con `queryPlanner` | `explain` con `executionStats` (sin aplicar escrituras) | vivo |
| CouchDB | `_explain` | `execution_stats` | vivo |
| Cosmos DB | plan de consulta del gateway | métricas de consulta e índice | emulador (solo la forma de la respuesta) + fixture |
| Elasticsearch, OpenSearch | `_validate/query?explain` y `_sql/translate` | `profile: true` | vivo |
| Solr | `debug=query` con `rows=0`. Solr no tiene plan sin buscar, así que la búsqueda corre pero no devuelve documentos. | `debug=timing` | vivo |
| Cassandra, ScyllaDB | camino de acceso según las claves (partición, índice, `ALLOW FILTERING`) | `TRACING` | vivo |
| DynamoDB | plan **deducido** de las claves: GetItem, Query o Scan. No es un plan del motor, porque DynamoDB no los tiene. | capacidad consumida | vivo (DynamoDB Local) |
| InfluxDB 1 | `EXPLAIN` | `EXPLAIN ANALYZE` | vivo |
| InfluxDB 2 (Flux) | **no**: Flux no tiene plan estimado | paquete `profiler` | vivo |
| InfluxDB 3 | `EXPLAIN` | `EXPLAIN ANALYZE` | vivo |
| Redis | **no** | **no** | El motor no tiene planes de ejecución. |
| IoTDB | **no** | **no** | La API REST (v1 y v2) rechaza `EXPLAIN`; solo el cliente CLI (Thrift) los devuelve. |
| Couchbase | `EXPLAIN` (JSON) | `profile: timings` (solo Enterprise; en Community, estimado + métricas) | vivo (Community 8.0) |
| TDengine | `EXPLAIN VERBOSE` | `EXPLAIN ANALYZE`, solo en lecturas | vivo |
| Apache Drill, Dremio | `EXPLAIN PLAN INCLUDING ALL ATTRIBUTES` | perfil de la consulta | vivo |
| Arrow Flight SQL | `EXPLAIN (FORMAT JSON)` en DuckDB; texto en los demás | `EXPLAIN ANALYZE`, solo en lecturas | vivo (GizmoSQL) |
| Apache Calcite Avatica | `EXPLAIN PLAN FOR` | estimado + resultados | fixture |
| etcd, FerretDB (real) | **no** | **no** | etcd no tiene planes; FerretDB devuelve solo `queryPlanner`. |

### Presets ODBC

Solo el preset genérico se probó contra un servidor: SQL Server vía Microsoft
ODBC Driver 18. Los demás se escribieron a partir de la documentación de cada
fabricante y necesitan su driver ODBC instalado.

| Preset | Estimado | Real | Verificado |
|---|---|---|---|
| genérico (SQL Server) | `SHOWPLAN_ALL` | `STATISTICS PROFILE` | vivo |
| genérico (otros servidores) | `EXPLAIN` en texto, si el servidor lo acepta | estimado + resultados | — |
| Db2 LUW | `EXPLAIN PLAN` + tablas de explain (si faltan, el error indica cómo crearlas) | estimado + resultados | fixture |
| Db2 z/OS | `PLAN_TABLE` / `DSN_STATEMNT_TABLE` (a mejor esfuerzo) | estimado + resultados | fixture |
| Sybase ASE | `SHOWPLAN` + `NOEXEC` | showplan durante la ejecución | fixture |
| SQL Anywhere | `EXPLANATION()` | estimado + resultados | **sin probar** |
| Hive, Impala, Teradata, Vertica, Netezza, Dameng | `EXPLAIN` del motor | estimado + resultados | fixture |
| Ocient | `EXPLAIN` en texto | estimado + resultados | **sin probar** |
| Exasol | **no**: no tiene `EXPLAIN` | profiling de la sesión | fixture |
| CUBRID | **no**: el plan estimado solo existe en `csql` | `SET TRACE ON` | fixture |
| Informix, GBase 8s | **no** | **no** | `SET EXPLAIN` escribe el plan en un archivo del servidor, no por ODBC. |
| Altibase | **no** | **no** | El plan solo sale por su cliente iSQL. |
| Db2 for i | **no** | **no** | No tiene `EXPLAIN` por SQL; los planes se ven con Visual Explain. |
| Ingres, Mimer, Caché, Zen, Access, dBase, NetSuite | **no** | **no** | El motor no devuelve el plan por ODBC. |
| OpenEdge | **no** | `_Sql_Qplan` después de ejecutar | fixture |
| Ignite 3 | `EXPLAIN PLAN FOR` (sintaxis sin verificar) | estimado + resultados | **sin probar** |

### Pendientes de verificación

Están implementados pero falta probarlos contra un servidor real:

- **Cloud sin emulador:** Athena, Snowflake y Databricks. Hace falta una cuenta.
- **Emuladores que no generan planes reales:** BigQuery y Spanner.
- **Motores con imagen de Docker pesada o sin imagen:** Redshift, HANA,
  StarRocks, Doris, OceanBase y SingleStore.
- **Presets ODBC:** necesitan el driver ODBC de cada fabricante.
- **Servicios sin imagen:** Aurora, AlloyDB, Cloud SQL, Azure SQL, Fabric,
  Oracle Autonomous, DocumentDB, Keyspaces, Neptune y Azure Databricks (solo
  tests unitarios; los que comparten motor se probaron contra ese motor).
- **Flight SQL** contra Dremio e InfluxDB 3; **TLS** en Couchbase, TDengine,
  Drill, Dremio, etcd y Flight SQL.

## Monitor del servidor

Clic derecho sobre la conexión → **Monitor**. Abre una pestaña que consulta
el servidor cada 2, 5, 10 o 30 segundos (solo mientras está visible) con su
propia sesión. Muestra:

- tarjetas por grupo (CPU, memoria, conexiones, actividad, red, disco, caché,
  almacenamiento, bloqueos, replicación);
- los contadores acumulados como tasa por segundo;
- medidores cuando hay un tope, con aviso "alto" (75 %) y "crítico" (90 %);
- tablas de sesiones, consultas en curso, bloqueos, esperas, bases, objetos
  más grandes, réplicas y nodos, según el motor;
- abajo, lo que el motor no puede informar y por qué.

Todos los motores lo tienen, salvo los de la tabla de excepciones. Una parte
que falla por permisos se omite con una nota: el resto del tablero sale igual.

### Qué no informa cada motor

| Motor | Qué falta | Motivo |
|---|---|---|
| PostgreSQL y su familia, MySQL, MariaDB, Cloud SQL, SingleStore, OceanBase, Doris/VeloDB, Databend, Manticore, Babelfish, Firebird | CPU del servidor | El motor no lo expone por SQL. En los administrados está en CloudWatch, Cloud Monitoring o la consola. |
| Aurora, AlloyDB, Cloud SQL, DocumentDB, Keyspaces, Neptune, Aurora DSQL, DynamoDB | casi todas las métricas de recursos | Solo están en CloudWatch o Cloud Monitoring. DynamoDB no consulta CloudWatch: agrega una dependencia y se cobra por métrica en cada lectura. |
| Azure Cosmos DB, Fabric | CPU, memoria, RU consumidas | Solo en Azure Monitor o la app Fabric Capacity Metrics. |
| Athena, BigQuery, Databricks, Snowflake | CPU y memoria | Servicios sin servidor visible: se informan jobs, bytes escaneados, slots, warehouses y créditos. El monitor de Snowflake no despierta un warehouse suspendido; el de Databricks no ejecuta SQL, para que el warehouse pueda apagarse solo. |
| Cloud Spanner | todo en el emulador | El emulador no tiene `SPANNER_SYS` ni Cloud Monitoring. |
| Cassandra, ScyllaDB | CPU y memoria de la JVM / throughput | Solo por JMX (Cassandra) o Prometheus en el puerto 9180 (ScyllaDB). |
| CouchDB, Solr | CPU del host (CouchDB), conexiones abiertas, consultas en curso | La API no los expone. |
| Redis, MongoDB, InfluxDB | CPU del host | Solo informan la CPU del propio proceso. |
| IoTDB, TimechoDB | CPU, memoria, disco y red | Hace falta activar el endpoint Prometheus del DataNode (`dn_metric_reporter_list=PROMETHEUS`); el formulario tiene el campo "URL de métricas". |
| SQLite, DuckDB, libSQL, archivos | CPU y sesiones | No hay servidor: se informa tamaño, páginas, caché y memoria. |
| Neo4j Community, Memgraph Community | hit rate de caché, QPS, actividad | Solo en las ediciones Enterprise. |
| OrientDB | CPU, memoria, QPS | Solo con el profiler de Enterprise. |
| FerretDB | memoria, conexiones, red, `top`, réplicas | FerretDB 2 no implementa esas secciones de `serverStatus`. |
| **Sin monitor:** Spark Thrift, Kyuubi, Actian Zen, Mimer, NetSuite, Apache Calcite Avatica, ODBC genérico con un motor desconocido | todo | Las métricas solo están en la UI o la API propia del motor (Spark UI, Zen Monitor, `sqlmonitor`), o el protocolo no las expone. |

### Probado contra servidores reales

PostgreSQL 16, CockroachDB, TimescaleDB, YugabyteDB, openGauss, Cloudberry,
Greengage, CrateDB, RisingWave, Materialize, H2, MySQL 8.4, MariaDB 11.8, TiDB,
Manticore, GreptimeDB, StarRocks, ClickHouse, Timeplus Proton, SQL Server 2022
(nativo y vía ODBC), Babelfish, Oracle 23ai, Firebird 5, libSQL, MongoDB 7,
FerretDB, Redis, Valkey, Dragonfly, Cassandra 5, ScyllaDB, CouchDB,
Elasticsearch, OpenSearch, Open Distro, Solr (standalone y cloud), InfluxDB 1,
2 y 3, IoTDB 1.3 y 2.0, DynamoDB Local, emulador de Cosmos DB, Trino, Presto,
Phoenix, ksqlDB, emulador de BigQuery, Neo4j (Community y Enterprise),
Memgraph, OrientDB, Couchbase, TDengine, Drill, Dremio, etcd y Flight SQL
(GizmoSQL). SQLite, DuckDB y los archivos, con archivos reales.

Solo tests unitarios: Redshift, Yellowbrick, Denodo, KingbaseES, OceanBase,
SingleStore, Doris, VeloDB, Databend, HANA, Snowflake, Athena, Databricks,
Neptune y los presets ODBC sin driver en la máquina.

## Motores que todavía no están

Motores conocidos que DBine no soporta, y por qué.

| Motor | Estado | Motivo |
|---|---|---|
| Google Firestore, Google Bigtable, Amazon Timestream, Salesforce, Salesforce Data Cloud | pendiente | Cada uno necesita un driver propio (REST o gRPC) y autenticación de la nube; falta decidir si se hacen. |
| Apache Kylin | pendiente | La API REST es simple, pero la imagen de prueba pide unos 10 GB de RAM. |
| Huawei GaussDB (administrado) | no | Solo ofrece autenticación SHA-256 o SM3, que el cliente de PostgreSQL no implementa. openGauss sí está, con contraseñas MD5. |
| Teiid | no | Proyecto discontinuado y sin imagen para probarlo. |
| HSQLDB, Apache Derby, H2 embebido | no | Solo tienen cliente en Java. H2 está por su modo servidor PostgreSQL (`-pg`). |
| DolphinDB | no | No tiene driver ODBC propio. |
| Raima RDM | por DSN | Su driver ODBC no documenta conexión sin DSN: se usa el preset genérico con un DSN. |
| Windows Management Instrumentation | no | Solo existe en Windows y no es una base de datos. |
| Jennifer, SnappyData, GemFire XD | no | Jennifer es un APM; SnappyData y GemFire XD están discontinuados y solo tienen JDBC. |

## Filtros por columna en los datos de una tabla

La fila de filtros bajo los encabezados (`Driver::filtered_browse`) se aplica
en el servidor: el driver agrega los filtros a su propia consulta de
exploración. Cuando un motor no puede aplicar algún filtro, la UI filtra las
filas ya cargadas (hasta el tope de filas elegido) y lo avisa en la barra de
filtros con el motivo.

| Motor | Cómo filtra | Lo que se filtra sobre las filas cargadas |
|---|---|---|
| PostgreSQL y variantes, DSQL, SQLite, libSQL, DuckDB, SQL Server, Oracle, SAP HANA, Firebird, Athena, Trino, Flight SQL, Dremio | `WHERE` estándar (`ILIKE` en PostgreSQL) | — |
| MySQL y variantes, ClickHouse, TDengine, Drill, BigQuery, Spanner, Databricks, Snowflake, Phoenix, InfluxDB 3, IoTDB | `WHERE` con las comillas y literales del dialecto | IoTDB: en `Time` solo comparaciones |
| Manticore | `WHERE` con `REGEX()` para texto | "no contiene" |
| Presets ODBC | `WHERE` según el preset | — |
| ksqlDB | `WHERE` antes de `EMIT CHANGES` | tópicos (`PRINT`) |
| Cassandra, ScyllaDB, Keyspaces | `WHERE … ALLOW FILTERING` con `fromJson` | `<>`, NOT IN, texto (necesita un índice SASI/SAI), nulos |
| DynamoDB | PartiQL con `begins_with` / `contains` | "termina con" |
| Cosmos DB | `CONTAINS` / `STARTSWITH` / `ENDSWITH` sin distinguir mayúsculas | — |
| Couchbase | SQL++ (`IS VALUED` para nulos) | — |
| OrientDB | `WHERE` con `.left()` / `.right()` / `.indexOf()` | — |
| MongoDB | filtro de `find` con operadores y `$regex` | condiciones SQL |
| CouchDB | selector Mango | vistas, condiciones SQL |
| Elasticsearch, OpenSearch | `bool.filter` | condiciones SQL |
| Solr | un `fq` por filtro | condiciones SQL |
| InfluxDB 1 (InfluxQL) | regex para texto | nulos |
| InfluxDB 2 (Flux) | `filter()` después del pivot | condiciones SQL |
| Neo4j, Memgraph, Neptune | `WHERE n.prop …` antes del `RETURN` | la grilla muestra el nodo entero en una columna, así que en la práctica se filtra sobre las filas cargadas; condiciones SQL |
| Redis, etcd | — | todo: se leen claves o prefijos enteros y no hay valores por los que filtrar |

Las condiciones SQL ("Condición SQL…") no se pueden evaluar sobre las filas
cargadas: en los motores que las filtran en la grilla, se ignoran.

**Probado contra servidores reales:** MySQL, Manticore, ClickHouse, Cassandra
5, CouchDB, MongoDB, Neo4j, OrientDB, Solr, InfluxDB 2 y 3, TDengine, IoTDB,
DynamoDB Local, el emulador de Spanner, Elasticsearch 8, Couchbase y Drill.
**Pendiente de verificar con servidor:** BigQuery (el emulador evalúa mal el
LIKE), Databricks, Snowflake, Cosmos DB, ksqlDB, Phoenix y los presets ODBC de
Hive y Spark.

## Edición de celdas (código de actualización)

Al editar celdas en la grilla de resultados, DBine genera el código que aplica
el cambio. **No lo ejecuta**: lo agrega a la query o lo abre en una nueva
(`Driver::update_script`).

- **Qué resultados se pueden editar:**
  - la pestaña de datos de un objeto;
  - una query que lee de una sola tabla (sin JOIN, UNION, GROUP BY ni WITH);
  - en MongoDB, un `db.<colección>.find(…)`.
- **Qué fila se actualiza:**
  - por la clave primaria, que tiene que estar en el resultado;
  - si la tabla no tiene clave, por todas las columnas, y la UI lo avisa.

Forma del código por motor:

| Motor | Código generado |
|---|---|
| SQL (todos los relacionales y analíticos con UPDATE) | `UPDATE … SET … WHERE <clave>;` con las comillas y los literales de cada motor (`N'…'` en SQL Server, literales tipados en Trino, etc.) |
| ClickHouse | `ALTER TABLE … UPDATE … WHERE …;` (mutación) |
| Timeplus | `ALTER STREAM … UPDATE …` (sintaxis de ClickHouse, sin probar contra un servidor) |
| Phoenix | `UPSERT INTO … (clave, columnas) VALUES (…);` |
| TDengine | `INSERT` con la misma marca de tiempo, que sobrescribe la fila |
| IoTDB | `INSERT` en el mismo `Time`; para un NULL, `DELETE` del punto |
| ksqlDB (tablas) | `INSERT` con la fila completa: reemplaza el valor de la clave |
| MongoDB | `db.getCollection(…).updateOne({ _id }, { $set: {…} })` |
| CouchDB | una función de actualización (`_design/dbine_update`) y un `PUT …/_update/set/<id>` por documento; no necesita `_rev` |
| Couchbase | `UPDATE … USE KEYS … SET …` |
| Cosmos DB | `UPDATE "c" SET {…} WHERE {"id": …}`, una sentencia propia de DBine que funde los campos y guarda con control de `_etag` |
| DynamoDB | PartiQL `UPDATE … SET … REMOVE … WHERE <clave>` |
| Elasticsearch / OpenSearch | `POST /<índice>/_update/<id>`; en data streams, `_update_by_query` |
| Solr | actualización atómica `{"id": …, "campo": {"set": …}}` |
| Redis / Valkey / Dragonfly | `HSET`/`HDEL`, `ZADD`/`ZREM` o un `EVAL` según el tipo de la key |
| etcd | `put` (y `del` + `put` si cambia la key) |
| Cassandra / ScyllaDB / Keyspaces | `UPDATE … SET … = fromJson(…) WHERE <clave primaria>;` |
| Neo4j / Memgraph / Neptune | `MATCH … SET n.prop = …` |
| OrientDB | `UPDATE Clase SET … WHERE @rid = …` |

**No soportado:**

| Motor | Motivo |
|---|---|
| InfluxDB 1/2/3 | Reescribir un punto es una escritura en line protocol, que ni Flux, ni InfluxQL, ni el SQL de v3 (de solo lectura) pueden expresar |
| Apache Drill | No tiene UPDATE: las tablas solo se recrean con CTAS |
| GreptimeDB | No tiene UPDATE: una fila se reemplaza insertándola completa de nuevo |
| NetSuite (ODBC) | SuiteAnalytics Connect es de solo lectura |
| ksqlDB (streams) | Los streams solo admiten agregar eventos |
| Streams de Redis | Las entradas de un stream no se pueden modificar |
| Vistas de CouchDB | Son resultados calculados, no documentos |

**Casos que el motor rechaza aunque se genere el código:**

- Manticore no admite asignar NULL.
- Cloud Spanner, Solr, TDengine e IoTDB necesitan la clave completa.
- Phoenix no permite cambiar la clave.
- En los atributos `@` de OrientDB y en las columnas de revisión y lease de
  etcd, el driver no genera el cambio y explica el motivo.
- Algunos motores solo actualizan ciertos tipos de tabla: Hive las ACID, Impala
  las de Kudu, y StarRocks y Doris las de clave primaria. Ahí el código se
  genera igual y el error lo da el motor al ejecutarlo.

### Eliminar filas

Con clic derecho sobre una fila (o sobre varias seleccionadas), o con
Supr/Retroceso sobre las filas seleccionadas, se marcan para eliminar: se ven
tachadas y el mismo ítem pasa a «Restaurar fila». Las ediciones de una fila
marcada no cuentan. «Guardar» muestra un solo script con los borrados y las
actualizaciones, en el lenguaje del motor (`Driver::delete_script`, el mismo
que usa la comparación de datos), y nada se ejecuta sin ese clic.

- **Qué fila se borra:** por la clave primaria; sin clave, por todas las
  columnas, con el mismo aviso que al editar. Las columnas binarias o largas
  (BLOB, bytea, CLOB, XML, geometrías…) quedan fuera de ese WHERE porque su
  valor no se puede comparar; si la clave primaria tiene uno de esos valores,
  la fila no se puede borrar desde la grilla. Un NULL en la clave va como
  `IS NULL`.
- **Orden:** primero los borrados y después las actualizaciones. Las filas no
  se superponen (una fila marcada pierde sus ediciones), y borrar primero
  evita que un UPDATE deje una fila igual a otra que se iba a borrar (sin
  clave, el DELETE se llevaría las dos) o choque con un valor único que está
  por desaparecer.
- Se aplican las mismas reglas que para editar: conexiones de solo lectura y
  resultados no editables.

Motores que no borran filas (el ítem aparece deshabilitado con el motivo):

| Motor | Motivo |
|---|---|
| Apache Drill | No tiene DELETE: las tablas solo se recrean con CTAS |
| ksqlDB | No tiene DELETE: una fila de una tabla se borra con un tombstone escrito directamente en el topic de Kafka, y los streams solo admiten agregar eventos |
| InfluxDB 2 (Flux) | Flux no borra puntos: se borran con la API `/api/v2/delete`, por rango de tiempo y predicado sobre los tags |
| InfluxDB 3 (SQL) | Su SQL es de solo lectura y no borra puntos sueltos |
| NetSuite (ODBC) | SuiteAnalytics Connect es de solo lectura |

**Casos que el motor rechaza por fila** (el motivo aparece en la barra de
cambios): Manticore con una clave NULL; TDengine necesita la marca de tiempo
como única clave (y `tbname` en una supertabla); Solr, Elasticsearch,
CouchDB, Cassandra, Cosmos DB y DynamoDB necesitan la clave o el id completo
del documento.

### Agregar filas y documentos

El botón «Agregar fila» de la barra de resultados, o el ítem del mismo nombre
en el menú contextual, suma una fila nueva al final de la grilla. Su número de
fila muestra «+» y la fila se resalta. Las celdas que no se tocan muestran
«(predeterminado)» y quedan fuera del INSERT, así que la columna toma su valor
por defecto o lo genera el motor (identidad, auto-incremento, el `_id` de
MongoDB). «Establecer NULL» pone un NULL explícito. «Quitar la fila nueva», o
Supr con la fila seleccionada, la descarta sin más.

Los motores de documentos (MongoDB y sus compatibles, como FerretDB, Amazon
DocumentDB o Cosmos DB for MongoDB, además de CouchDB, Elasticsearch/OpenSearch
y Solr) muestran «Agregar documento». Abre un editor JSON con los
campos del resultado ya cargados: un objeto agrega un documento y un arreglo
agrega varios. También sirve en una colección vacía, que no tiene columnas en
la grilla. Cualquier motor cuyo resultado no tenga columnas recibe el mismo
diálogo, titulado «Agregar fila».

El código de las filas nuevas va junto con los cambios pendientes, en la misma
barra que las ediciones y los borrados. «Guardar» muestra el script completo y
lo ejecuta solo con ese clic; «Agregar a la query» lo manda a una query. Está
en el lenguaje de inserción de cada motor, el mismo que usan copiar e
importar: `INSERT` en los SQL, `insertMany`, `_bulk`, comandos de Redis,
`CREATE` de Cypher, etc.

- **Orden:** primero los DELETE, después los UPDATE y al final los INSERT.
- **Lotes:** las filas que fijan las mismas columnas van en un solo INSERT.
- **Identidad explícita:** si una fila nueva fija una columna de identidad, el
  script de SQL Server la envuelve en `SET IDENTITY_INSERT … ON/OFF`, y
  PostgreSQL mueve la secuencia más allá del valor insertado.
- **IoTDB:** la fila nueva necesita la columna Time.
- Se aplican las mismas reglas que para editar: las conexiones de solo lectura
  se comportan igual que con la edición de celdas.

Motores que no agregan filas (el botón aparece deshabilitado y el tooltip da
el motivo):

| Motor | Motivo |
|---|---|
| Apache Drill | No tiene INSERT: las tablas solo se recrean con CTAS |
| InfluxDB 1 / 2 / 3 | Los puntos se escriben con line protocol, y ningún lenguaje de consulta del motor lo expresa |

## Vista JSON en árbol

El botón «{ }» de la barra de resultados, junto a tabla y gráfico, muestra el
resultado como árbol: un nodo por fila (un documento), con sus campos debajo y
los objetos y arrays anidados hasta el último nivel. Está en todos los motores:
en los de documentos los campos anidados llegan como JSON y se despliegan, y en
los SQL se despliegan las columnas `json` / `jsonb` (y cualquier texto que sea
un objeto o un array JSON). En los motores de documentos DBine recuerda la
última vista elegida, tabla o árbol.

- **Grandes resultados:** solo se dibujan los nodos visibles y los hijos de un
  nodo se arman al abrirlo. Los arrays de más de 100 elementos se abren por
  grupos (`[0 … 99]`, `[100 … 199]`…). «Expandir todo» se detiene en 20.000
  nodos visibles y avisa.
- **Tipos:** cada campo muestra su tipo (String, Int, Double, Boolean, Null,
  Object, Array, ObjectId, Date). En el primer nivel sale del tipo de la
  columna; en los niveles anidados, de la forma del valor.
- **Búsqueda** en claves y valores: quedan solo los documentos con
  coincidencias, abiertos hasta cada una y resaltados; Enter y ⇧Enter recorren
  las coincidencias.
- **Teclado:** flechas para moverse, → abre y ← cierra o sube al padre,
  ⌘C copia el valor.
- **Menú contextual:** copiar el valor, la ruta (`cliente.contacto.email`) o el
  documento entero en JSON, ver el valor completo, expandir todo debajo y,
  en los campos de primer nivel, filtrar los datos por ese valor.
- **Edición**, con las mismas reglas que la grilla y compartiendo sus cambios
  pendientes: doble clic, Enter, F2 o empezar a escribir edita un valor, también
  uno anidado; «Editar como JSON…» reemplaza un objeto o un array; «Quitar este
  campo» saca un campo o un elemento anidado; Supr marca el documento para
  eliminar. Un número sigue siendo número y un booleano, booleano. El `_id` no
  se edita.
- **Cómo se guarda un cambio anidado:** se reescribe el campo de primer nivel
  entero (por ejemplo, `cliente` con su `contacto.email` nuevo), así el código
  de actualización de cada motor lo aplica igual. En los motores de documentos
  va como objeto, también cuando se edita el campo desde la tabla.

## Asistente de IA

Funciona con **todos los motores**. El prompt le indica el lenguaje de consulta
de cada uno (SQL con su dialecto, CQL, JSON/Mongo, Redis, Flux o Cypher) para
que escriba en la sintaxis correcta.

La estructura de la base que recibe como contexto sale de `database_schema`.
En un motor que no lo implementa, el asistente responde sin estructura y se lo
dice al usuario; no hay más diferencias. Detalle en `docs/asistente-ia.md`.

## Formatear código

El botón **Formatear** de la query (⇧⌥F) formatea la selección, o todo si no
hay selección, en un solo paso que ⌘Z deshace. Los formateadores se cargan la
primera vez que se usan (`web/src/composables/formatCode.ts`).

| Lenguaje | Motores | Cómo |
|---|---|---|
| SQL | todos los SQL | `sql-formatter` con el dialecto del motor: T-SQL (SQL Server, Sybase; los lotes con `GO` se formatean de a uno), PostgreSQL, MySQL, MariaDB, SQLite, PL/SQL (Oracle), Db2, Hive, Spark (Databricks), Trino, Snowflake, BigQuery, Redshift, N1QL (Couchbase), DuckDB, TiDB, SingleStore, ClickHouse; SQL genérico para el resto. Los `{{parámetros}}` de la biblioteca no se tocan |
| CQL | Cassandra, ScyllaDB, Keyspaces | `sql-formatter` genérico |
| JSON / MongoDB | MongoDB y compatibles, Elasticsearch, Cosmos DB y los motores con consultas JSON | JSON con sangría si es JSON; si no, `js-beautify` (el shell de MongoDB es JavaScript) |

**Sin formateador** (el botón queda deshabilitado y explica por qué): Redis
(un comando por línea, no hay nada que formatear), Flux (InfluxDB 2) y Cypher
(Neo4j, Memgraph, Neptune): no hay un formateador confiable para el navegador.

## Profiler

Clic derecho sobre una base › **Profiler** abre una pestaña con todas las
consultas que cualquier cliente ejecuta sobre esa base, en vivo, como el
Profiler de SQL Server: hora, duración, texto, base, usuario, cliente (host o
dirección), aplicación, filas y error. La aplicación sale de lo que informa el
motor (el program name de SQL Server, `application_name` de PostgreSQL, el
`appName` de MongoDB, el `program_name` de MySQL…); donde no lo informa, queda
vacía.

En la pestaña, la fila bajo los encabezados filtra lo que se captura: mientras
hay un filtro, solo se guardan las consultas que pasan. Duración y filas
aceptan `> 500`, `< 1s`; el texto, `contiene`, `!no contiene`, `=igual`;
usuario, cliente y aplicación se eligen de los valores vistos. Clic derecho
sobre una fila filtra por su usuario, cliente, aplicación o base, o por
**las consultas iguales**: el mismo texto con otros valores (sin literales,
números ni comentarios). Con ese filtro se muestra cuántas veces se ejecutó y
su duración promedio, mínima, p95 y máxima. El contrato está en `crates/dbine-driver/src/profiler.rs` y cada driver
lo implementa en su `src/profiler.rs`.

Hay dos modos:

- **Completo:** el motor registra cada consulta (Extended Events, el profiler
  de MongoDB, un historial o un log de consultas) y DBine lee lo nuevo cada
  segundo.
- **Muestreo:** el motor solo muestra lo que se está ejecutando, o la última
  consulta de cada conexión. DBine mira cada 100 ms y reporta cada consulta una
  vez, cuando termina. Las más cortas que ese intervalo pueden no aparecer; la
  pestaña lo avisa.

**Cambios en el servidor.** Si un motor necesita activar algo para capturar,
DBine lo activa al iniciar, la pestaña lo muestra en amarillo y lo restaura al
detener, al cerrar la pestaña o al cerrar la app. En conexiones de solo lectura
no cambia nada: usa lo que ya esté disponible o explica por qué no puede. Hay
un solo profiler activo por conexión, para que el que se detiene primero no
restaure lo que el otro todavía usa. Si la app se cierra de golpe, el próximo
inicio limpia la sesión de Extended Events que quedó; en los demás motores el
ajuste queda activado (el próximo profiler lo encuentra así y no lo toca) y hay
que volverlo atrás a mano.

**Arranque.** Una pestaña abierta desde el menú arranca sola. Una pestaña
restaurada al reabrir la app espera a «Iniciar», porque iniciar puede cambiar
la configuración del servidor.

### Por motor

| Motor | Modo | Fuente | Qué activa (y restaura) |
|---|---|---|---|
| SQL Server, Managed Instance | completo | Extended Events (lotes, RPC y errores), filtrado por base | Una sesión de Extended Events; necesita ALTER ANY EVENT SESSION. Sin ese permiso o en solo lectura: muestreo de `sys.dm_exec_requests` |
| Azure SQL Database | completo | Extended Events a nivel de base | Igual que SQL Server |
| Fabric Warehouse | muestreo | `sys.dm_exec_requests` | — (no tiene Extended Events) |
| Babelfish | muestreo | `pg_stat_activity` + `sys.dm_exec_sessions` | — |
| PostgreSQL y compatibles (Aurora, AlloyDB, Cloud SQL, EDB, Fujitsu, KingbaseES, openGauss, TimescaleDB, YugabyteDB, Greenplum, Cloudberry, Greengage) | muestreo | `pg_stat_activity` | — (sin superusuario o `pg_read_all_stats`, solo las consultas propias) |
| CockroachDB | muestreo | `crdb_internal.cluster_queries` (todo el clúster) | — |
| Redshift | completo | `sys_query_history` | — |
| Yellowbrick | completo | `sys.log_query` | — |
| CrateDB | completo | `sys.jobs_log` | — |
| Materialize | completo | `mz_internal.mz_recent_activity_log` | Sube el muestreo del statement logging a 1; solo el rol `mz_system` puede. Si no, explica por qué no captura |
| H2 | muestreo | `INFORMATION_SCHEMA.SESSIONS` | — |
| MySQL, Aurora MySQL, Cloud SQL | completo | `performance_schema.events_statements_history_long` | Los consumers de Performance Schema que estaban apagados. En solo lectura: las últimas 10 consultas por conexión |
| MariaDB | muestreo | `information_schema.PROCESSLIST` | — (Performance Schema solo se activa reiniciando) |
| TiDB | completo | `CLUSTER_SLOW_QUERY` | `tidb_slow_log_threshold` en 0. En solo lectura: muestreo de `CLUSTER_PROCESSLIST` |
| OceanBase | completo | `GV$OB_SQL_AUDIT` | — |
| StarRocks, Doris, VeloDB | muestreo | `SHOW FULL PROCESSLIST` (solo el frontend conectado) | — |
| SingleStore | muestreo | PROCESSLIST | — |
| Databend | muestreo | `system.processes` | — |
| Manticore | muestreo | `SHOW THREADS` (búsquedas de más de ~100 ms) | — |
| GreptimeDB | muestreo | `information_schema.process_list` | — |
| Oracle, Autonomous Database | muestreo | `V$SESSION` + `V$SQL` + `V$SQLSTATS` | — (necesita SELECT_CATALOG_ROLE; ASH/AWR no se usan por su licencia) |
| Firebird | muestreo | `MON$STATEMENTS` | — |
| SAP HANA | completo | `M_EXPENSIVE_STATEMENTS` | El trace de expensive statements (`global.ini`), con umbral de 1 µs. Si no se puede: muestreo de `M_ACTIVE_STATEMENTS` |
| ClickHouse, Timeplus Proton | completo | `system.query_log` | — (pide `SYSTEM FLUSH LOGS`; sin ese permiso las consultas llegan con hasta ~7,5 s de demora) |
| Trino, Presto, Starburst | completo | lista de consultas del coordinador (`/v1/query`, ~100 recientes) | — |
| TDengine | muestreo | `performance_schema.perf_queries` (todo el servidor; solo consultas de más de ~1 s) | — |
| MongoDB | completo | `system.profile` | Nivel de profiling 2 y `sampleRate` 1. En solo lectura lee lo que ya registra, o muestrea `currentOp` si está apagado |
| MongoDB Atlas (tier compartido), Cosmos DB, mongos, DocumentDB | muestreo | `currentOp` | — |
| Redis, Valkey, Dragonfly | completo | `MONITOR` (sin duraciones; tiene costo en servidores con mucha carga) | — |
| Cassandra 4.0+ | muestreo | `system_views.queries` de cada nodo (todos los keyspaces) | — |
| ScyllaDB | completo | `audit.audit_log` | Las categorías y el keyspace de auditoría; requiere Scylla iniciado con `audit: table` |
| Couchbase | completo | `system:completed_requests` | `queryCompletedThreshold` en 0 |
| Elasticsearch, OpenSearch, Open Distro | muestreo | `_tasks` de búsquedas en curso | — |
| Neo4j | muestreo | `SHOW TRANSACTIONS` | — |
| Memgraph | muestreo | `SHOW TRANSACTIONS` (todo el servidor) | — |
| Neptune | muestreo | páginas de estado de openCypher, Gremlin y SPARQL | — |
| InfluxDB 1.x | muestreo | `SHOW QUERIES` | — |
| InfluxDB 3 | completo | `system.queries` (todo el servidor) | — |
| IoTDB, TimechoDB | muestreo | `SHOW QUERIES` | — |
| Dremio | completo | `sys.jobs_recent` | — |
| Apache Drill | completo | perfiles de la API REST | — |
| Snowflake | completo | `INFORMATION_SCHEMA.QUERY_HISTORY` (cada 3 s; mantiene encendido el warehouse) | — |
| BigQuery | completo | API `jobs.list` (necesita `bigquery.jobs.listAll` para ver a todos) | — |
| Athena | completo | `ListQueryExecutions` del workgroup (sin usuario) | — |
| Databricks | completo | Query History API (todo el workspace) | — |
| Cloud Spanner | muestreo | `SPANNER_SYS.OLDEST_ACTIVE_QUERIES` | — |

### CPU, lecturas y escrituras

Cuando el motor las informa, la pestaña suma las columnas **CPU**, **Lecturas**
y **Escrituras**. Aparecen solo si el motor reporta esa cifra, y la unidad va
en el tooltip del encabezado (cada motor cuenta distinto: páginas, filas,
bytes, documentos). Con **las consultas iguales** el resumen muestra
promedio, mínimo, p95 y máximo de cada cifra, además de la duración.

| Motor | CPU | Lecturas | Escrituras |
|---|---|---|---|
| SQL Server (Extended Events) | sí | `logical_reads` (páginas) | páginas |
| SQL Server (muestreo, DMVs) | sí, mientras corre | sí, mientras corre | sí, mientras corre |
| MySQL 8.0.28+ | sí: enciende el consumer `events_statements_cpu` mientras perfila y lo apaga al detener; en solo lectura no se ve CPU | filas examinadas | no |
| MariaDB | no | filas examinadas | no |
| TiDB | no | claves (`Process_keys`) | claves (`Write_keys`) |
| Databend | no | bytes leídos | bytes escritos |
| Oracle | sí | `BUFFER_GETS` (bloques) | `DIRECT_WRITES`: solo escrituras directas (el DML normal lo escribe DBWR después) |
| SAP HANA | sí | no | no |
| Firebird | no | page fetches (páginas) | page marks (páginas) |
| ClickHouse, Timeplus Proton | sí | filas leídas | filas escritas |
| Snowflake | no | bytes escaneados | bytes escritos |
| BigQuery | "CPU" es el tiempo de slot (puede superar la duración) | bytes procesados | no |
| Databricks | "CPU" es el tiempo total de tareas | bytes leídos | bytes escritos |
| Athena | no | bytes escaneados | no |
| Trino | sí | filas procesadas | bytes escritos |
| Presto | sí | filas | no |
| Dremio | sí | filas escaneadas | no |
| InfluxDB 3 | tiempo de cómputo | no | no |
| MongoDB | sí (solo servidores Linux, 6.3+) | documentos examinados | documentos escritos |
| MongoDB en muestreo (`currentOp`) | no | no | no |
| Couchbase | sí | documentos leídos | documentos escritos |
| OpenSearch | sí | no | no |
| Neo4j | si `db.track_query_cpu_time` está activo | páginas | no |

**Sin cifras**, y por qué:

| Motor | Motivo |
|---|---|
| PostgreSQL y compatibles | `pg_stat_activity` no tiene cifras por sentencia; `pg_stat_statements` agrega por forma de consulta, no por ejecución |
| Babelfish | Muestrea `pg_stat_activity`: mismo caso que PostgreSQL |
| Cloud Spanner | `OLDEST_ACTIVE_QUERIES` no las tiene; `QUERY_STATS` es por minuto y por forma de consulta |
| Apache Drill | Estarían en el perfil completo de cada consulta: hace falta un pedido extra por sentencia |
| TDengine | `performance_schema.perf_queries` solo tiene el tiempo transcurrido, la cantidad de subconsultas y datos de la conexión |
| IoTDB | `SHOW QUERIES` solo devuelve el id, el DataNode, el tiempo transcurrido y la sentencia |
| InfluxDB 1.x | `SHOW QUERIES` solo da la duración y el estado |
| Elasticsearch | La API de tareas no tiene CPU ni E/S (OpenSearch sí informa CPU) |
| Cassandra | `system_views.queries` solo tiene el tiempo en cola y el tiempo corriendo |
| ScyllaDB | El log de auditoría no tiene costos, ni siquiera la duración |
| Memgraph, Neptune | `SHOW TRANSACTIONS` y las APIs de estado no tienen CPU ni E/S |
| Redis, Valkey, Dragonfly | `MONITOR` solo da el comando |

### Motores sin profiling

| Motor | Motivo |
|---|---|
| SQLite, DuckDB, libSQL local | Son bases embebidas: las consultas de otros procesos no pasan por ningún servidor que las muestre |
| libSQL remoto (Turso) | Su protocolo no expone las consultas de otros clientes |
| Aurora DSQL | No tiene `pg_stat_activity` ni historial de consultas |
| InfluxDB 2 (Flux) | No implementa `SHOW QUERIES` y el log de consultas solo va a los archivos del servidor |
| FerretDB | No tiene el comando `profile` y su `currentOp` no muestra la consulta, el cliente ni el usuario |
| Amazon Keyspaces | Las consultas de otros clientes solo quedan en CloudTrail |
| DynamoDB, Cosmos DB (NoSQL) | Solo en CloudTrail o Azure Monitor, fuera del protocolo |
| CouchDB, etcd, OrientDB, Phoenix, Solr, ksqlDB, Flight SQL | Su protocolo no tiene forma de ver las consultas de otros clientes |
| Cassandra 3.x | `system_views` existe desde la 4.0 |
| Presets ODBC | Depende del motor detrás de cada preset: pendiente, preset por preset |

### Probado contra servidores reales

Con dos sesiones: una captura y la otra ejecuta una consulta lenta y una
rápida. La prueba verifica que cada una aparezca una sola vez, la lenta con su
duración, y que no se cuelen las consultas del propio profiler. Cuando el
driver activa algo en el servidor, también verifica que quede restaurado.

- **Pasaron:** PostgreSQL 16, CockroachDB, YugabyteDB, CrateDB, H2, SQL Server
  (Extended Events y muestreo en solo lectura), Babelfish, Oracle, Firebird,
  MySQL 8.4 (y en solo lectura), MariaDB 11, TiDB, StarRocks, Manticore,
  GreptimeDB, ClickHouse, Proton, Trino, Presto, TDengine, MongoDB 7 (y el
  muestreo de `currentOp`), Redis 7, Valkey, Dragonfly, Cassandra 5,
  ScyllaDB, Couchbase 8, Elasticsearch 8, OpenSearch 2, Open Distro, Neo4j 5
  (Community y Enterprise), Memgraph, InfluxDB 1.8 y 3, IoTDB 1.3 y 2.0,
  Dremio 26, Drill 1.22 y el emulador de BigQuery.
- **Materialize:** el contenedor no permite activar el statement logging
  (hace falta `mz_system`); se probó que lo explica.
- **Sin probar** (sin contenedor o sin cuenta): Azure SQL, Fabric, HANA,
  Redshift, Yellowbrick, OceanBase, SingleStore, Doris, VeloDB, Databend,
  Starburst, DocumentDB, Neptune, TimechoDB, Snowflake, Athena, Databricks y
  Cloud Spanner (su emulador no tiene `SPANNER_SYS`).

## Comparar esquemas

Comparar funciona en todos los motores que leen su estructura
(`database_schema`). Aplicar los cambios, es decir "Sincronizar"
(`Driver::sync_script`), depende de lo que cada motor permite cambiar con DDL.
Lo que un motor no puede hacer queda como aviso en el script, no como una
sentencia que falla. Cómo se usa: `docs/comparacion-de-esquemas.md`.

### Qué se compara en cada motor

Además de columnas, claves e índices por columnas, cada motor compara lo
siguiente.

**PostgreSQL y compatibles.** Índices con `INCLUDE`/`STORING`, cualquier
método (`gin`, `gist`, `brin`, `hash`, `spgist`, `bitmap`, `ubtree`, `hnsw`…),
opciones (parámetros de almacenamiento, `NULLS NOT DISTINCT`, `DESC`, `NULLS`,
clase de operadores, `COLLATE`), índices de expresión (incluidos los de
texto completo con `tsvector`) y `EXCLUDE` como tipo de índice. `CHECK`.
Secuencias (no las identidad ni `serial`), tipos enum, dominio, compuesto y
rango, sinónimos en openGauss, EDB y KingbaseES. Materialize: tipos lista,
mapa y registro. H2: secuencias y dominios. Yugabyte: `HASH`, `DESC`, `ybgin`.
RisingWave: `DESC`, `INCLUDE`, `DISTRIBUTED BY`. CrateDB: índices de texto
completo (con analizador y `INDEX OFF`) y `CHECK`. DSQL: `CHECK` (se agrega
`NOT VALID` y `ASYNC VALIDATE`), `WHERE`, `NULLS NOT DISTINCT`, dominios y
secuencias.

**SQL Server.** Todos los tipos de índice (agrupado, no agrupado, único,
filtrado, columnstore, XML, espacial, hash), `INCLUDE`, opciones (`FILLFACTOR`,
`PAD_INDEX`, `IGNORE_DUP_KEY`, `STATISTICS_NORECOMPUTE`, bloqueos de fila y
página, `OPTIMIZE_FOR_SEQUENTIAL_KEY`, `DATA_COMPRESSION`, `COMPRESSION_DELAY`,
`BUCKET_COUNT`, espaciales), índices de texto completo (`KEY INDEX`, catálogo,
seguimiento de cambios, lista de palabras irrelevantes, lista de propiedades de
búsqueda, idioma, columna de tipo y semántica estadística por columna),
`CHECK`, restricción `UNIQUE` frente a índice, secuencias, sinónimos, tipos
alias y de tabla, catálogos de texto completo y listas de palabras
irrelevantes (se modifican en el lugar). Babelfish: solo tipos. Fabric:
ninguno.

**MySQL y compatibles.**
- MySQL, Aurora, Cloud SQL: `CHECK` (incluido `NOT ENFORCED`), prefijos,
  `DESC`, índices funcionales, `INVISIBLE`, `COMMENT`, `FULLTEXT WITH PARSER`,
  `SPATIAL`, `USING HASH/BTREE`, SRID espacial.
- MariaDB: `CHECK` de tabla y de columna (estos últimos como parte del tipo de
  la columna), `DESC`, `IGNORED`, `COMMENT`, `FULLTEXT`, `SPATIAL`, único
  `HASH`, secuencias.
- TiDB: clave primaria agrupada como opción de tabla, prefijos, índices de
  expresión, `INVISIBLE`, `COMMENT`, secuencias, `CHECK` (necesita
  `tidb_enable_check_constraint`).
- StarRocks, Doris, VeloDB: índices de `SHOW INDEX` (`BITMAP`, `NGRAMBF`,
  `GIN`/invertido), `bloom_filter_columns` y rollups (tipo `ROLLUP`).
- GreptimeDB: índices `INVERTED`, `FULLTEXT` y `SKIPPING`, y las opciones
  `ttl`, `append_mode` y `compaction`.
- Manticore: ajustes de la tabla (`morphology`, `min_infix_len`…).
- SingleStore: `SHARD KEY`, `SORT KEY` y tipo de tabla, como opciones de tabla.
- OceanBase y Databend: secuencias. Databend además índices invertidos, ngram
  y de agregación.

**Oracle (y Autonomous).** `CHECK` (no los `NOT NULL`), índices normales,
bitmap, de función y de dominio (Oracle Text como `FULLTEXT`, Spatial como
`SPATIAL`, el resto como `DOMAIN` con `INDEXTYPE` y `PARAMETERS`), `REVERSE`,
`COMPRESS`, `INVISIBLE`, particionado `LOCAL`/`GLOBAL`, secuencias (el valor
inicial es `LAST_NUMBER`, así que una secuencia usada difiere), sinónimos
(los públicos bajo `PUBLIC`) y tipos objeto, `TABLE OF` y `VARRAY`.

**SAP HANA.** `CHECK`, índices `FULLTEXT` con todos sus ajustes, `CPBTREE` e
`INVERTED`, secuencias, sinónimos (privados y públicos) y tipos de tabla.

**Snowflake.** Secuencias y la clave de clustering (`CLUSTER BY` / `DROP
CLUSTERING KEY`).

**SQLite y libSQL.** `CHECK` de tabla y de columna (un cambio reconstruye la
tabla), `DESC` y collation en índices, y tablas virtuales (FTS5, FTS4, R\*Tree)
como tipo `virtual_table`, que ya no se sincroniza como una tabla común.

**DuckDB.** `CHECK` (sin nombres), índices de expresión, secuencias y tipos
(`ENUM`, `STRUCT`, alias, `LIST`, `MAP`, `UNION`).

**Firebird.** `CHECK` (los `INTEG_n` sin nombre), índices inactivos, dominios
(como tipo) y secuencias (el valor actual no las hace diferir).

**ClickHouse.** Índices de salteo (todos, incluido el de texto), `CHECK`,
`ASSUME` (opción de tabla `assume:<nombre>`), proyecciones (tipo `PROJECTION`)
y diccionarios (tipo `dictionary`).

**Nube.**
- Spanner: `CHECK`, `STORING`, `DESC`, índices filtrados, `NULL_FILTERED`,
  entrelazados, de búsqueda y vectoriales, columnas ocultas y secuencias.
- BigQuery: índices de búsqueda y vectoriales.
- Databricks: restricciones `CHECK` de Delta.
- Trino, Dremio: nada que comparar.

**Presets ODBC.**
- Db2 LUW: `CHECK`, `INCLUDE`, secuencias, alias, tipos distintos y
  estructurados.
- Db2 for z/OS: `CHECK`, secuencias, alias y tipos distintos.
- Db2 for i: `CHECK`, secuencias y alias.
- Informix, GBase: `CHECK`, secuencias y sinónimos.
- SQL Anywhere: `CHECK` y secuencias.
- ASE, Teradata: `CHECK`.
- Vertica: `CHECK` y secuencias.
- MonetDB: secuencias.
- Netezza: secuencias y sinónimos.
- Dameng: `CHECK`, secuencias, sinónimos y tipos objeto.
- Altibase: `CHECK`, secuencias y sinónimos.
- CUBRID: seriales y sinónimos (desde 11.2).
- Ingres: `CHECK`, secuencias y sinónimos.
- Mimer: `CHECK`, secuencias, sinónimos y dominios.
- Exasol, Hive, Impala, Spark: nada.

**Cassandra, ScyllaDB, Keyspaces.** Opciones de los índices SAI, SASI y
personalizados, tipos, vistas materializadas y funciones.

**MongoDB.** El validador `$jsonSchema` como un `CHECK` de nombre
`validator`, índices de texto como `FULLTEXT`, `2dsphere`, `2d`, `hashed` y
wildcard, y las opciones TTL, `sparse`, `hidden`, collation y filtro parcial.
Vistas.

**Neo4j, Memgraph.** Restricciones `UNIQUE`, `EXISTS`, `KEY` y `TYPE` como
tipos de índice, e índices `FULLTEXT`, `VECTOR` y `POINT` con su configuración.

**Elasticsearch, OpenSearch.** Los ajustes de análisis (`analysis`). Para
aplicarlos, la sincronización cierra y vuelve a abrir el índice.

**Couchbase.** Índices GSI (antes no se leían).

**CouchDB.** `validate_doc_update` como `CHECK`.

**OrientDB.** `COLLATE` y motor y metadatos de Lucene. Las secuencias ya no
difieren por el valor actual.

**Cosmos DB.** Índices espaciales, de texto completo y vectoriales.

**DynamoDB.** Proyecciones de los índices (`INCLUDE`, `KEYS_ONLY`).

**TDengine.** Índices de tags.

### Qué falta comparar

Qué se probó contra un servidor real:

- **Con servidor real:** PostgreSQL 16, TimescaleDB, CockroachDB 26.3,
  openGauss 7.0, H2 2.1, Materialize 26.43, YugabyteDB, RisingWave y CrateDB
  (contenedor temporal); SQL Server (contenedor con texto completo); Oracle
  (salvo los índices Oracle Text y Spatial, que la imagen liviana no trae);
  Spanner (emulador); DSQL (con PostgreSQL como sustituto); SQLite, libSQL,
  DuckDB, Firebird y ClickHouse; MySQL 8.4, MariaDB 11.8, TiDB 7.5,
  StarRocks 4.1, GreptimeDB 1.2.1 y Manticore; MongoDB, Cassandra 5,
  ScyllaDB, Neo4j 5, Memgraph, OpenSearch, Couchbase, CouchDB, OrientDB,
  DynamoDB Local, TDengine y el emulador de Cosmos DB (que no guarda
  políticas de índices propias: los índices espaciales, de texto completo y
  vectoriales de Cosmos DB tienen solo pruebas unitarias). Elasticsearch se
  probó a través de OpenSearch.
- **Solo pruebas unitarias, implementado con la documentación del proveedor y
  sin servidor de prueba:** SAP HANA (las columnas `CHECK_CONDITION` y las de
  las vistas de texto completo), Snowflake, BigQuery, Databricks, OceanBase,
  Databend, los índices Oracle Text y Spatial, y todos los presets ODBC (Db2,
  Informix, GBase, SQL Anywhere, ASE, Teradata, Vertica, MonetDB, Netezza,
  Dameng, Altibase, CUBRID, Ingres y Mimer). En los ODBC el SQL de catálogo
  no se pudo verificar contra un servidor, y algunas columnas quedan sin
  confirmar: las vistas de Netezza, `V$SEQ`/`SYS_SYNONYMS_` de Altibase,
  `db_serial`/`db_synonym` de CUBRID e `iisynonyms` de Ingres.

| Motor | Qué falta | Motivo |
|---|---|---|
| SQL Server | Clave primaria (agrupada y opciones), collation de columna, almacenamiento de la tabla (compresión, filegroups, particiones, memory-optimized, temporal), índices deshabilitados y restricciones `NOCHECK`, filegroup del texto completo, listas de propiedades de búsqueda como objetos, estadísticas, tipos CLR, colecciones de esquemas XML, funciones de partición | El contrato de comparación no tiene campo para representarlos. Pendiente explícito: agregarlos al contrato. |
| SQL Server | Recrear una tabla memory-optimized | No está soportado. Reemplazar una secuencia usada por un default, o un tipo usado por una columna, falla en el `DROP`. |
| Babelfish, Fabric | Casi todo | Babelfish compara solo tipos; Fabric no tiene nada que comparar. |
| PostgreSQL y compatibles | Tablespaces de índices, parámetros de almacenamiento de la clave primaria | Pendiente explícito: no se leen. |
| PostgreSQL y compatibles | Reemplazar un tipo que una columna usa | `DROP` + `CREATE` falla mientras la columna lo use. |
| PostgreSQL y compatibles | `OWNED BY` de una secuencia de una tabla nueva | Se omite en el script; la comparación siguiente lo muestra. |
| openGauss | `DROP DOMAIN` | El motor no lo permite. |
| Greenplum, openGauss | `INCLUDE` | Greenplum: no se conoce la versión que lo soporta. openGauss: solo tiene índices `ubtree`. |
| H2 | Orden `NULLS` de los índices | Pendiente explícito. |
| Redshift, Denodo | Todo lo de esta lista | No hay nada que comparar. |
| Yellowbrick | Todo salvo secuencias | Pendiente explícito. |
| CrateDB | Agregar un índice de texto completo a una tabla existente | El motor no lo permite: se avisa que hay que recrear la tabla. |
| MySQL, Aurora, Cloud SQL | `KEY_BLOCK_SIZE`, `ENGINE_ATTRIBUTE` | Pendiente explícito. |
| MariaDB | Quitar un `CHECK` de columna con `DROP CONSTRAINT` | Va como parte del tipo de la columna. |
| TiDB | `DESC` en índices, cambiar la clave primaria agrupada | TiDB no guarda el `DESC`. La clave agrupada no se puede cambiar con `ALTER`: se avisa. |
| SingleStore | Cambiar `SHARD KEY`, `SORT KEY`, tipo de tabla | No se pueden cambiar con `ALTER`: se avisa. |
| GreptimeDB | Cambiar `analyzer` y `case_sensitive` de un índice | El motor no los cambia: se avisa. |
| StarRocks, Doris, VeloDB | Vistas materializadas de la sincronización de rollups | Fuera del alcance de esta tanda: pendiente explícito. Los cambios de índice corren como trabajos en segundo plano: un único `ALTER` que se reintenta cada 2 s mientras la tabla está ocupada (máximo 10 minutos, se puede cancelar). |
| Oracle | Índices vectoriales (23ai), particionado de tablas, tablespace y almacenamiento de índices | Pendiente explícito. |
| SAP HANA | — | Sin faltantes conocidos; pendiente de probar contra un servidor. |
| Snowflake | Índices, búsqueda optimizada, índices de tablas híbridas, `CHECK`, sinónimos, tipos | Las tablas estándar no tienen índices, la búsqueda optimizada es un servicio, y Snowflake no tiene `CHECK` ni sinónimos ni tipos. Los índices de tablas híbridas: pendiente explícito, no se leen. |
| SQLite, libSQL | Secuencias, tipos | El motor no los tiene. |
| DuckDB | Claves `DESC`, sinónimos, qué tipo de usuario usa una columna | DuckDB descarta el `DESC` y no tiene sinónimos. Pendiente explícito: qué tipo de usuario usa una columna (DuckDB informa el tipo expandido). |
| Firebird | Qué dominio usa una columna, dominios de arreglo; reemplazar un dominio usado | Pendiente explícito; el reemplazo falla y necesita `ALTER DOMAIN`. |
| ClickHouse, Timeplus | Colecciones con nombre (ClickHouse); restricciones y proyecciones (Timeplus) | Las colecciones con nombre son del servidor, no de una base. Timeplus no tiene restricciones ni proyecciones. |
| Spanner | Columnas `DESC` de la clave primaria, change streams, property graphs, modelos, proto bundles | Pendiente explícito. |
| BigQuery | `CHECK`, secuencias, sinónimos | El motor no los tiene. |
| DSQL | Nada conocido | — |
| Databricks | Índices bloom filter | Son metadatos de columna y Databricks los da por obsoletos. |
| Trino, Dremio | Todo | No hay nada que comparar. |
| Db2 for i, Informix, GBase | Tipos definidos por el usuario | Db2 for i: no se leen. Informix y GBase: distinct y row types pendientes. |
| Db2 LUW | Tipos array y row | Pendiente explícito. |
| SQL Anywhere, ASE, Teradata, Vertica | Dominios (SQL Anywhere), tipos de `sp_addtype` (ASE), UDTs (Teradata), proyecciones (Vertica) | Pendiente explícito: no se leen. |
| Netezza | `CHECK`, tipos de usuario | El motor no tiene `CHECK` ni UDTs. |
| CUBRID | `CHECK`, tipos de usuario | El motor no tiene `CHECK` ni UDTs. |
| Altibase, Ingres | Tipos de usuario | No tienen UDTs SQL. |
| Dameng | Sinónimos públicos | Pendiente explícito. |
| Exasol, Hive, Impala, Spark | Todo | No hay nada que comparar. |
| MongoDB | — | Sin faltantes conocidos en esta tanda. |
| Cassandra, ScyllaDB, Keyspaces | — | Sin faltantes conocidos en esta tanda. |
| Neo4j, Memgraph | Capacidad de índices vectoriales de Memgraph | El servidor informa un valor que varía. |
| Elasticsearch, OpenSearch | Plantillas (templates) y pipelines de ingesta | Pendiente explícito. |
| Couchbase | Índices de Search (FTS) | Viven en la API del servicio Search, no en la de índices GSI. |
| CouchDB | Vistas; cambiar o borrar un `validate_doc_update` | Las vistas: pendiente explícito. Cambiar o borrar necesita `_rev`, así que la sincronización solo avisa. |
| OrientDB | `min`, `max` y `regexp` de propiedades | Pendiente explícito. |
| Cosmos DB | Aplicar cambios de índices espaciales, de texto completo y vectoriales | No hay sentencia para modificar un contenedor: la sincronización solo avisa. |
| Solr | Tipos de campo y `copyField` | Pendiente explícito. |
| InfluxDB | Sincronización | No tiene DDL: los measurements y campos aparecen al escribir. |
| IoTDB | Índices y `CHECK` | El motor no los tiene. |
| Redis | Todo | No tiene esquema. |

### Sincronización: motores relacionales

| Motor | Cómo cambia una columna | Límites | Probado contra servidor |
|---|---|---|---|
| PostgreSQL y compatibles (CockroachDB, Timescale, Yugabyte, AlloyDB, Aurora, EDB, Kingbase…) | `ALTER COLUMN … TYPE … USING`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT`. Las vistas que usan la columna se recrean alrededor del cambio. | — | PostgreSQL 16 |
| Redshift | `ALTER COLUMN … TYPE`, solo para el largo de un varchar | nulabilidad | no |
| CrateDB, RisingWave, Materialize | solo agregar y quitar columnas | tipos | no |
| Denodo | — | no tiene: las vistas base se definen en Denodo | — |
| H2 | `SET DATA TYPE` | — | no |
| DSQL | `ADD COLUMN`, `DROP COLUMN`, `DROP NOT NULL`, defaults, índices `ASYNC` | tipos, NOT NULL, clave primaria, claves foráneas | sí, con PostgreSQL como sustituto |
| SQL Server, Azure SQL, Babelfish | `ALTER COLUMN … [NOT] NULL`. El default (restricción con nombre) y los índices sobre la columna se sacan y se reponen. | — | SQL Server 2022 (el script completo, sentencia por sentencia) |
| Fabric | solo agregar y quitar columnas | tipos | no |
| MySQL, MariaDB, TiDB, OceanBase, SingleStore, Aurora/Cloud SQL | `MODIFY COLUMN` con la columna completa | — | no |
| StarRocks, Doris, VeloDB | `MODIFY COLUMN` | cambios asincrónicos (se avisa) | no |
| Manticore | solo agregar y quitar columnas | tipos | no |
| Oracle, Dameng | `MODIFY (…)` | — | Oracle |
| Firebird | `ALTER COLUMN … TYPE`, `SET/DROP NOT NULL`. Las claves sin nombre (`INTEG_n`) se buscan y se borran. | — | sí |
| SAP HANA | `ALTER (<columna>)` | — | no |
| SQLite, libSQL | La tabla se reconstruye: tabla nueva, copia, borrado y renombre. Agregar columnas se hace en el lugar. | — | libSQL |
| DuckDB | `ALTER COLUMN … TYPE`, nulabilidad y defaults en el lugar | Claves y restricciones: la tabla se reconstruye, y falla si otras tablas la referencian. | sí |
| Snowflake | `SET DATA TYPE` (solo agrandar), nulabilidad | Defaults de columnas existentes: no. Índices: no (solo UNIQUE). | no |
| BigQuery | `SET DATA TYPE` (solo agrandar), `DROP NOT NULL`, defaults | Columnas NOT NULL nuevas (entran como NULLABLE), pasar a NOT NULL, índices | no (el emulador no aplica los ALTER) |
| Databricks | `ALTER COLUMN … TYPE` (solo ampliar) | Columnas nuevas NOT NULL o identidad. Borrar columnas necesita *column mapping* (se avisa). | no |
| Athena | Iceberg: `ADD COLUMNS`, `DROP COLUMN`, `CHANGE COLUMN`. Externas: `ADD/CHANGE COLUMN`, borrado solo en CSV. | NOT NULL, defaults, claves, índices, columnas de partición | no |
| Spanner | `ALTER COLUMN <columna completa>` | Clave primaria y sus columnas. Tipos: solo STRING↔BYTES y largos. | emulador |
| Trino, Starburst | `SET DATA TYPE`, `DROP NOT NULL`, `ADD/DROP COLUMN` (depende del conector) | pasar a NOT NULL, defaults | Trino |
| Presto | solo agregar y quitar columnas | tipos, nulabilidad, defaults | sí |
| ClickHouse | `MODIFY COLUMN` (nulabilidad = `Nullable(T)`), índices de salteo (skip indexes) con `MATERIALIZE` | motor, ORDER BY, PARTITION BY, clave | sí |
| Timeplus | agregar columnas, índices, comentario y TTL | borrar o cambiar columnas | sí |
| TDengine | `ADD/DROP/MODIFY COLUMN` y tags (MODIFY solo para agrandar) | primera columna de tiempo, subtablas | sí |
| Dremio (Iceberg) | `ADD COLUMNS`, `DROP COLUMN`, `ALTER COLUMN` (solo ampliar) | particiones, fuentes que no son Iceberg | sí |
| Phoenix | solo agregar y quitar columnas | tipos, clave (row key) | sí |
| Presets ODBC | Según el motor: Db2 (`SET DATA TYPE`, con `REORG`), Sybase y Zen (`MODIFY`), Informix (`MODIFY (…)`), Teradata, Exasol, Vertica, MonetDB, Hive/Impala/Spark, etc. Los que no modifican columnas solo agregan y quitan. | Db2 for z/OS no cambia la nulabilidad. Hive y Spark tienen límites propios. | no |
| NetSuite | — | no tiene: SuiteAnalytics Connect es de solo lectura | — |

### Sincronización: otros motores

| Motor | Qué sincroniza | Límites | Probado contra servidor |
|---|---|---|---|
| Cassandra, ScyllaDB, Keyspaces | tablas, columnas (agregar y quitar), índices, opciones | tipo de columna, clave primaria (hay que recrear la tabla) | Cassandra y ScyllaDB |
| MongoDB | colecciones, vistas, índices, validador, TTL | capped, time series y clustered se fijan al crear | sí |
| Couchbase | colecciones, índices GSI | maxTTL | sí |
| Cosmos DB | crear y borrar contenedores | Partition key, unique keys, política de índices, TTL y RU/s: se cambian desde el portal o la CLI de Azure. | no |
| CouchDB | índices Mango y design docs nuevos | borrar o cambiar un índice o design doc | sí |
| DynamoDB | tablas, índices globales | Esquema de claves: hay que recrear la tabla. Índices locales: no. | DynamoDB Local |
| OrientDB | clases, propiedades, índices | EXTENDS | sí |
| Solr | colecciones, campos (con aviso de reindexar) | uniqueKey, configSet, shards | standalone y SolrCloud |
| ksqlDB | streams, tablas, agregar columnas | borrar o cambiar columnas, clave | sí |
| Elasticsearch, OpenSearch | campos nuevos, réplicas, alias | Tipo de campo, campos borrados y shards: hay que reindexar. | sí |
| Neo4j, Memgraph | índices y restricciones | las propiedades no se declaran | sí |
| IoTDB | series nuevas y borradas | tipo, codificación, compresión | sí |

Sin sincronización (la comparación funciona igual):

- **Redis, etcd:** no tienen esquema, solo claves con valores. Para llevar
  datos se usa la copia o la exportación.
- **Flight SQL:** es un protocolo, no un motor; conviene conectarse con el
  driver propio de la base que está detrás.
- **Drill:** consulta archivos; sus tablas se crean con `CREATE TABLE AS` y no
  tienen columnas que modificar.
- **InfluxDB:** los measurements y sus campos aparecen al escribir y no se
  modifican con DDL.
- **Neptune:** no tiene esquema definido por el usuario.
- **Denodo y NetSuite:** los motivos están en la tabla de motores relacionales.

### Eliminar en la comparación

"Eliminar" borra un índice, columna, clave foránea, `CHECK`, clave primaria,
tabla u objeto (vista, procedimiento, función, trigger, secuencia…) de un lado
sin tener que pasar el cambio desde el otro. Como las flechas, solo modifica la
copia en memoria; el `DROP` sale en el script de "Sincronizar". Antes de
ejecutar, el diálogo busca qué depende de cada objeto que se borra, en el lado
donde se borra y solo en los motores con `supports_dependencies`. La búsqueda
es asíncrona y no bloquea el botón. Si falla, el diálogo dice "no se pudieron
revisar las dependencias" y deja ejecutar igual. Los dependientes confirmados y
los probables cuentan como roturas; los de SQL dinámico se muestran aparte, y
si algo no se pudo leer, la lista se marca como incompleta.

Qué se probó: la prueba arma los cambios como los arma la pantalla, ejecuta el
script, vuelve a leer ambos lados y verifica que la diferencia desapareció.

| Motor | Probado contra servidor | Qué no se puede borrar | Motivo |
|---|---|---|---|
| PostgreSQL 16 | sí | nada de lo probado | índice, columna, FK, `CHECK`, PK, tabla referenciada, vistas, triggers, funciones sobrecargadas, procedimientos |
| CockroachDB 26.3 | sí | clave primaria | exige que toda tabla tenga una: rechaza `DROP CONSTRAINT` de la PK sin agregar otra en la misma transacción. El script la conserva y avisa "CockroachDB no deja una tabla sin clave primaria". |
| SQL Server | sí (2022) | vista o función `WITH SCHEMABINDING` que usa la columna o tabla; columna usada por una columna calculada | SQL Server rechaza el `DROP`. Lo primero lo debería mostrar la revisión de dependencias; lo segundo no lo cubre el generador. Babelfish y Fabric no se probaron. |
| MySQL 8, MariaDB 11 | sí | índice que una FK necesita; columna de un `CHECK` de varias columnas | MySQL da el error 3959 y MariaDB el 1054; el servidor rechaza el `DROP`. La pantalla quita solo la columna, así que el script falla. Un `CHECK` de una sola columna lo quita el servidor con ella. Columna usada por una FK: sin probar. |
| Oracle (Free 23) | sí | paquetes; índices de dominio de Oracle Text y Spatial (sin probar) | los paquetes no se cargan en la comparación ni tienen `DROP`. La imagen `slim` no trae Text ni Spatial. Los `CHECK` y FK sin nombre se borran buscando el nombre de sistema (`SYS_C…`) con un bloque PL/SQL. |
| SQLite | sí | rutinas (no existen) | borrar una columna, `CHECK` o FK reconstruye la tabla (con aviso). Un trigger de una tabla reconstruida se pierde: pendiente explícito, hay que recrearlo en la capa de comparación o avisar. libSQL usa la misma reconstrucción y no se revisó. |
| MongoDB | sí | campos; claves foráneas, triggers y rutinas no existen; la PK `_id` no se cambia | los documentos no tienen esquema fijo, borrar un campo solo da un aviso. Una vista aparece dos veces en el modelo (como tabla de tipo `view` y como objeto); borrar cualquiera quita las dos. |
| Elasticsearch | sí (8.15.3); OpenSearch sin probar | campos del mapping; análisis personalizado | requiere reindexar, queda como aviso. No tiene índices internos, PK, FK, `CHECK`, vistas, rutinas ni triggers. Se puede borrar el índice entero y la descripción. |
| Resto de los motores | no | sin verificar | el `DROP` sale del generador común o del `drop_other` de cada motor. No hay prueba contra servidor. |

Pendientes explícitos:

- **`CHECK` e índices de una columna borrada** en el generador común
  (`alter.rs`, contrato compartido): solo SQL Server los quita junto con la
  columna. MySQL y Oracle probablemente fallan igual con un `CHECK`; sin probar.
- **Vistas dependientes de una tabla que pierde una columna:** se borran y
  recrean, y si la vista usa esa columna la recreación falla.
- **Clave primaria de CockroachDB:** cambiarla genera `DROP CONSTRAINT` +
  `ADD PRIMARY KEY` en dos sentencias, que probablemente falla igual. Su forma
  es `ALTER PRIMARY KEY USING COLUMNS`; sin probar.
- **Explorador y scripts generados:** "Eliminar" del explorador y la sección
  `DROP` de los scripts generados todavía escriben el `DROP` viejo de triggers
  y funciones sobrecargadas en PostgreSQL.
- **Elasticsearch:** quitar la descripción reemplaza todo `_meta`.

### Sincronización: comentarios de tablas y columnas

Un comentario que se agrega, cambia o quita en el origen se lleva al destino,
también el de una columna que la misma sincronización agrega. Cada motor lo
escribe con su sintaxis (el contrato: `alter::sync_script_with_comments`;
sin ella, `COMMENT ON`).

| Motor | Cómo se escribe | Probado contra servidor |
|---|---|---|
| SQL Server, Azure SQL, Babelfish | Propiedad extendida `MS_Description`: `sp_addextendedproperty` o `sp_updateextendedproperty` si ya existe; `sp_dropextendedproperty` al quitarlo | SQL Server 2022; Babelfish (las tres sentencias) |
| PostgreSQL y compatibles, Oracle, Firebird, DuckDB, Snowflake, SAP HANA, Db2, Exasol y los demás con `COMMENT ON` | `COMMENT ON TABLE` / `COMMENT ON COLUMN` | PostgreSQL 16 |
| MySQL, MariaDB, TiDB, OceanBase, SingleStore, Aurora/Cloud SQL, StarRocks, Databend | Columna: `MODIFY COLUMN … COMMENT`. Tabla: `ALTER TABLE … COMMENT =` | MySQL 8, MariaDB 11, StarRocks |
| Doris, VeloDB | Tabla: `ALTER TABLE … MODIFY COMMENT` | no |
| GreptimeDB | `COMMENT ON TABLE` / `COMMENT ON COLUMN` (su `MODIFY COLUMN` solo cambia el tipo) | sí |
| CUBRID (ODBC) | Columna: `MODIFY`. Tabla: `ALTER TABLE … COMMENT =` | no |
| Hive, Impala, Spark (ODBC) | Columna: `CHANGE COLUMN` (Spark: `ALTER COLUMN … COMMENT`). Tabla: `SET TBLPROPERTIES ('comment' = …)` | no |
| Vertica (ODBC) | Tabla: `COMMENT ON TABLE` | no |
| Cassandra, ScyllaDB | Opción `comment` de la tabla; al quitarlo, `comment = ''` | Cassandra 5 |
| ClickHouse, Trino, Athena, BigQuery, Databricks, TDengine, Elasticsearch | Los propios de cada motor (ver su fila en las tablas de arriba) | según el motor |

Sin sincronización de comentarios:

- **Fabric Warehouse:** no tiene propiedades extendidas.
- **Manticore, Spanner, Phoenix, SQLite, libSQL:** el motor no tiene comentarios
  que se puedan escribir con SQL. Phoenix muestra `REMARKS` del catálogo, pero
  no tiene sentencia para cambiarlos.
- **Vertica (ODBC), columnas:** `COMMENT ON COLUMN` comenta columnas de
  proyecciones, no de tablas.
- **Presets ODBC sin `COMMENT ON` ni comentarios en línea** (Sybase ASE,
  Informix, Ocient, Virtuoso, IRIS, Zen, OpenEdge, Machbase, SQream, Access,
  dBase, NuoDB, HeavyDB, Ignite, ODBC genérico): no se escriben. Pendiente
  explícito para los que tengan sintaxis propia: hay que confirmarla contra
  cada servidor, y no hay contenedores para hacerlo.

## Búsqueda de claves en el explorador

En los motores clave-valor, una base puede tener millones de claves. DBine no
las lista todas: el nodo **Claves** busca en el servidor, una página a la vez.
El contrato está en `crates/dbine-driver/src/keys.rs` (`Driver::key_search` y
`Session::scan_keys`).

- **Buscador** en la primera fila de **Claves**. Enter busca en el servidor y
  Esc vuelve a mostrar todas. Guarda las últimas búsquedas de cada conexión.
- **Páginas** de 500 claves con **Cargar más**. El nodo muestra cuántas hay en
  total. Una búsqueda muy selectiva sigue pidiendo páginas sola unos segundos,
  para no empezar con una lista vacía.
- **Carpetas por namespace** según el separador del motor (`user:1:cart` queda
  en `user › user:1`). Un prefijo con una sola clave muestra la clave sin
  carpeta. Clic derecho en una carpeta › **Buscar en el servidor** muestra
  todas las claves de ese prefijo, no solo las cargadas.
- **Tipo y TTL** de cada clave, cuando el motor los tiene.

| Motor | Búsqueda | Separador | Filtro por tipo | TTL |
|---|---|---|---|---|
| Redis, Valkey, Dragonfly | `SCAN … MATCH` con comodines (`*`, `?`, `[…]`); texto sin comodines = las claves que lo contienen, con la clave exacta primero. Distingue mayúsculas. | `:` | En el servidor con `SCAN … TYPE` (Redis 6 o superior); en versiones anteriores lo filtra DBine | `PTTL` |
| etcd | Prefijo, dentro del prefijo de la conexión; las páginas siguen el orden de las claves | `/` | No tiene tipos | El del lease de la clave |

El total sale de `DBSIZE` en Redis y del `count` del rango en etcd. Cada página
de Redis revisa como mucho 50.000 claves y devuelve lo que encontró, así una
búsqueda sobre millones de claves no bloquea la sesión.

### Motores sin búsqueda de claves

- **DynamoDB:** el explorador lista tablas, no ítems. Los ítems se ven con
  "Ver datos" y se filtran con los filtros por columna.
- **Los demás motores:** sus objetos son tablas, colecciones o índices, que se
  listan enteros y se filtran con el cuadro "Filtrar" del explorador.

### Probado contra servidores reales

`key_search` en `crates/drivers/redis/tests/integration.rs` (Redis 7, Valkey,
Dragonfly) y en `crates/drivers/etcd/tests/integration.rs` (etcd 3.5):
paginación con total y cursor, comodines, texto sin comodines, filtro por tipo
y TTL.

## Carga masiva

Motores sin carga masiva propia (`bulk_load`), o con una parcial. La
migración usa ahí la inserción por lotes genérica.

| Motor | Qué falta | Motivo |
|---|---|---|
| Amazon Neptune | Carga masiva | openCypher por HTTPS toma cada pedido como una transacción propia y no hay transacciones de varios pedidos: una carga cancelada con un pedido en vuelo lo confirmaría igual, después de terminar. La inserción genérica tiene el mismo límite. El cargador masivo de Neptune lee de S3 y necesita un rol IAM, fuera de lo que ve una conexión. |
| Neo4j, Memgraph | Carga de relaciones | La carga crea nodos (`UNWIND … CREATE`); una relación necesita sus nodos de origen y destino. |
| Neo4j, Memgraph | Segundos intercalares (`:60`) | Los valores temporales de Cypher no los tienen: la carga los rechaza. |
| Memgraph | Fechas fuera de los años 0 a 9999, arreglos de bytes | Memgraph no las guarda (la carga las rechaza); los bytes van como texto `0x…`. |
| Flight SQL: GizmoSQL (DuckDB) | Cargar solo algunas columnas de una tabla cuando entre ellas hay `STRUCT`, `LIST`, `MAP` o `UNION` | Esa carga usa un `INSERT` preparado, y GizmoSQL no acepta valores anidados como parámetros (la carga lo rechaza). Cargando todas las columnas se usa la carga masiva (`CommandStatementIngest`), que sí los acepta. |
| Flight SQL: GizmoSQL (DuckDB) | Cargar solo algunas columnas cuando un `HUGEINT` tiene más de 38 dígitos | GizmoSQL pasa cada parámetro del `INSERT` preparado por su texto, y ahí un `HUGEINT` de 39 dígitos falla (la carga lo rechaza). Cargando todas las columnas no hay límite. |
| Flight SQL: GizmoSQL (DuckDB) | `UHUGEINT`, `BIT`, `BIGNUM` o `TIMETZ` dentro de un tipo anidado (como `UHUGEINT[]` o `STRUCT(a BIT)`), en lectura y en carga | Por Arrow llegan como los bits crudos de un `DECIMAL(38,0)`, los bytes internos de DuckDB o una hora sin su zona. Sueltos se leen y cargan como texto, convertido en el servidor; dentro de un tipo anidado no hay forma de hacerlo, y se rechazan. |
| Flight SQL sin transacciones | Deshacer una ventana de commit que falla | Cada lote (o cada tanda del `INSERT` preparado) se confirma por separado: lo ya confirmado queda, y el error lo dice. |

## Transferencia masiva (migrar datos)

Cómo mueve los datos la migración (`docs/transferencia-masiva.md`) en cada
motor. Todos los motores leen y cargan por el motor de transferencia; lo que
cambia es la vía:

- **Carga masiva:** la vía de carga propia del motor (`bulk_load`). Sin ella,
  la migración escribe `INSERT` por lotes (`insert_script`).
- **Copia directa:** entre dos bases del mismo motor, las filas no pasan por
  la app (`copy_native`).
- **Lectura tipada:** las celdas salen con su tipo exacto (decimales con todos
  los dígitos, binarios completos, fechas con su precisión), no como texto de
  la grilla. Salvo que se aclare, todos los motores de la lista la tienen.
- **Clonado fiel** y **sincronizar solo lo que cambió** solo existen entre
  bases del mismo motor.

Probado contra servidores reales (contenedores `dbine-test-*`): todos los que
figuran sin marca. Lo que dice "sin probar en vivo" no tiene contenedor o
emulador y sigue la documentación del fabricante; está abajo, en las
limitaciones.

| Motor | Carga masiva | Copia directa | Clonado fiel | Sincronizar |
|---|---|---|---|---|
| PostgreSQL, TimescaleDB, KingbaseES, AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu | `COPY … FROM STDIN` binario (texto si una columna no tiene codificador: arreglos, `interval`, `money`, enums) | sí (un `COPY TO` → `COPY FROM`, solo con los mismos tipos nativos) | sí | sí |
| YugabyteDB | `COPY` binario | sí | no | sí |
| openGauss | `COPY` binario | sí | no | no |
| Greenplum, Cloudberry, Greengage | `COPY FROM` en texto | no | no | no |
| CockroachDB, Redshift, CrateDB, H2, Denodo, RisingWave, Yellowbrick, Materialize | no tienen (`INSERT` por lotes) | no | no | no |
| SQL Server, Azure SQL | `INSERT BULK` con `TABLOCK` y paquetes TDS de 32.767 bytes | sí (filas como bytes TDS, sin decodificar) | sí | sí |
| Fabric, Babelfish | no tienen (`INSERT` por lotes) | no | no | no |
| MySQL, MariaDB, Aurora MySQL, Cloud SQL MySQL | `LOAD DATA LOCAL INFILE` desde memoria (`INSERT` preparado multifila si el servidor tiene `local_infile` apagado) | no | no | no |
| TiDB | `INSERT` preparado multifila | no | no | no |
| OceanBase, SingleStore | `LOAD DATA LOCAL` (sin probar en vivo) | no | no | no |
| StarRocks, Doris, VeloDB, Databend, GreptimeDB | `INSERT … VALUES` multifila | no | no | no |
| Manticore | no tiene (`insert_script`) | no | no | no |
| Oracle, Oracle Autonomous | DML por arreglos (`INSERT` con binds en lote) | sí (con regiones horarias y JSON extendido) | no | no |
| SQLite | `INSERT` preparado en una transacción por ventana | sí (`ATTACH` de solo lectura + `INSERT … SELECT` por rangos de `rowid`) | no | no |
| DuckDB | Appender | sí, solo dentro de la misma instancia | no | no |
| libSQL / Turso | `INSERT` preparado multifila por HTTP | no | no | no |
| ClickHouse, Timeplus | `INSERT … FORMAT RowBinary` | sí (RowBinary de uno a otro) | no | no |
| Firebird | `EXECUTE BLOCK` con muchos `INSERT` | no | no | no |
| SAP HANA | `INSERT` preparado en lotes (sin probar en vivo) | no | no | no |
| Db2, Sybase, SQL Anywhere, Informix, Teradata, Vertica, Access, dBase y ODBC genérico | `INSERT` preparado con arreglos de parámetros | no | no | no |
| Hive, Impala, Spark, Kyuubi, Cloudera (ODBC) | no tienen (`INSERT` multifila) | no | no | no |
| NetSuite | solo lectura | — | — | — |
| Flight SQL (GizmoSQL…) | `CommandStatementIngest` (o `INSERT` preparado) | no | no | no |
| Phoenix, Avatica | `UPSERT` / `INSERT` preparado en lotes | no | no | no |
| Aurora DSQL | `INSERT` multifila, ventanas de hasta 3.000 filas en varias conexiones (sin probar en vivo contra DSQL real) | no | no | no |
| Snowflake | `INSERT … SELECT` desde una tabla de trabajo (sin probar en vivo) | no | no | no |
| BigQuery | trabajos de carga desde JSON por líneas | no | no | no |
| Databricks | `INSERT … VALUES` por ventana (sin probar en vivo) | no | no | no |
| Athena | `INSERT … VALUES`, solo tablas Iceberg (sin probar en vivo) | no | no | no |
| Trino, Presto, Starburst | `INSERT … VALUES` (una transacción por sentencia) | no | no | no |
| Dremio | `INSERT … SELECT CAST … FROM (VALUES …)`, solo tablas con DML (Iceberg) | no | no | no |
| Drill | no tiene: solo lectura | — | — | — |
| Cloud Spanner | mutaciones `insert` | no | no | no |
| Cassandra, ScyllaDB | `INSERT` preparado, 256 en vuelo | no | no | no |
| Amazon Keyspaces | igual que Cassandra (sin probar en vivo) | no | no | no |
| MongoDB, FerretDB, DocumentDB | `insertMany` desordenado | sí, entre los tres (BSON crudo) | no | no |
| Cosmos DB (NoSQL) | lotes transaccionales por clave de partición | sí, entre Cosmos DB | no | no |
| Couchbase | `INSERT` de SQL++ con parámetros | no | no | no |
| CouchDB | `_bulk_docs` | no | no | no |
| DynamoDB | `TransactWriteItems` con puts condicionales | no | no | no |
| Elasticsearch, OpenSearch, Open Distro | `_bulk` NDJSON (Open Distro sin probar en vivo) | no | no | no |
| Solr | `/update` con JSON | no | no | no |
| Redis, Valkey, Dragonfly | `HSET` en `MULTI`/`EXEC` | no | no | no |
| etcd | transacciones de puts | no | no | no |
| Neo4j, Memgraph | `UNWIND $rows CREATE (n:Etiqueta) SET n += r` | no | no | no |
| Amazon Neptune | no tiene (ver "Carga masiva") | no | no | no |
| OrientDB | scripts `BEGIN; …; COMMIT;` por `/batch` | no | no | no |
| InfluxDB 1, 2 y 3 | protocolo de línea | no | no | no |
| TDengine | `INSERT` multifila | no | no | no |
| IoTDB, TimechoDB | `insertTablet` en columnas | no | no | no |
| ksqlDB | `/inserts-stream` fila a fila | no | no | no |

### Velocidad

Órdenes de magnitud medidos en contenedores locales (una sola máquina, el
origen y el destino en la misma red): sirven para comparar vías, no
prometen nada en otro entorno ni contra un servidor remoto.

- **Millones de filas por segundo:** SQLite y DuckDB (lectura y carga), Flight SQL
  (contra DuckDB) y la copia directa de DuckDB, ClickHouse y PostgreSQL.
- **Cientos de miles:** PostgreSQL, SQL Server, MySQL y MariaDB, ClickHouse,
  MongoDB, Redis y compatibles, Oracle, TiDB (lectura) e InfluxDB.
- **Decenas de miles:** Cassandra, ScyllaDB, TiDB (carga), Firebird, Neo4j, Memgraph,
  Elasticsearch, OpenSearch, CouchDB, Couchbase, etcd, TDengine, IoTDB, libSQL
  y la lectura de Dremio.
- **Miles:** DynamoDB, Phoenix, ksqlDB, StarRocks, Dremio (carga, limitada por
  su planificador) y Cosmos DB contra su emulador (de 1.000 a 6.000).
- En SQL Server la vara de 700.000 filas/s no se pudo medir: el contenedor
  disponible es `amd64` emulado en una Mac `arm64` y el límite es el servidor.
  Dio 200.000 a 270.000 filas/s con copia directa.

### Limitaciones y rechazos

Un rechazo es una falla con causa, antes o durante la carga: la migración no
cambia el dato en silencio ni lo deja a medias sin decirlo.

**Relacionales**

- **PostgreSQL y derivados.** La copia directa solo va con los mismos tipos
  nativos en ambos lados y sin columnas atadas a su servidor (`money`, `oid`,
  `reg*`); si no, se lee y se carga. CockroachDB no carga por `COPY` (su
  protocolo extendido no lo admite), y Redshift, CrateDB, H2, Denodo,
  RisingWave y Yellowbrick no tienen `COPY FROM STDIN` binario (Redshift solo
  desde S3): escriben `INSERT`. Materialize podría cargar por `COPY` en texto;
  queda pendiente porque no hay contenedor para verificarlo. Redshift, Denodo,
  H2, CrateDB, los motores de streaming y Yellowbrick leen por el protocolo
  simple: todo llega como el texto del servidor. Una falla del destino (una
  restricción, disco lleno) recién aparece cuando termina su ventana; con
  la confirmación en 0 la tabla entera es una ventana.
- **PostgreSQL con «Solo protocolo simple».** Una conexión que usa solo el
  protocolo simple (un gateway que rechaza el extendido; ver
  [ejecución de scripts](ejecucion-de-scripts.md#protocolo-de-consultas-postgresql))
  no copia, no compara datos ni clona: esas operaciones necesitan el
  protocolo extendido y se rechazan con el motivo.
- **Clonado y sincronización en PostgreSQL.** YugabyteDB no clona (tablets y
  particionado por hash no están en el catálogo de PostgreSQL); openGauss tiene
  un catálogo de PostgreSQL 9.2 con opciones de almacenamiento propias y
  tampoco sincroniza (no tiene `hashtextextended`); Greenplum y derivados
  tienen políticas de distribución, no cargan `COPY` binario en tablas
  temporales y su versión 6 no trae `hashtextextended`; CockroachDB no tiene
  `COPY FROM` por el protocolo extendido. La sincronización pide PostgreSQL 11
  o más. Con PostgreSQL 15 y 16 usa `UPDATE` más `INSERT` (su `MERGE` no
  devuelve la acción); desde la 17, un solo `MERGE`. Los disparadores y las
  claves foráneas se apagan durante el aplicado con
  `session_replication_role = replica`; si el rol no puede, disparan y el
  registro lo avisa.
- **SQL Server.** `sql_variant` se rechaza (no se puede declarar en
  `INSERT BULK` ni pasar como bytes). Fabric y Babelfish no tienen carga
  masiva ni copia directa (no se pudo verificar `INSERT BULK`) y no clonan:
  Fabric no tiene *filegroups*, particiones, disparadores ni tablas
  temporales o en memoria, y el catálogo `sys` de Babelfish es una emulación
  parcial. `DBINE_SQLSERVER_NO_RAW=1` fuerza la vía decodificada en lugar de
  la copia con bytes crudos.
- **Sincronización en SQL Server.** Las columnas de la clave tienen que ser
  `NOT NULL` en los dos lados (una clave nula no cae en ningún grupo ni
  coincide en el `MERGE`): se rechaza con el motivo. Una columna de
  identidad que no es la primera de la clave, o con incremento negativo, no
  puede seguir al origen y se rechaza. Si las *collations* de la clave son
  distintas, se aplican todas las filas de los dos lados; si el destino
  considera iguales dos claves del origen, se rechaza.
- **MySQL y familia.** `LOAD DATA LOCAL` convierte valores inválidos y claves
  duplicadas en avisos: una ventana con avisos, o con menos filas que las
  enviadas, se deshace y falla. TiDB no usa `LOAD DATA` porque confirma solo,
  aun con `autocommit` apagado, y una ventana fallida no se podría deshacer.
  Los bytes hacia una columna de texto tienen que ser UTF-8 válido. Las
  claves únicas y foráneas quedan activas. El bloqueo de tabla solo existe en
  MySQL y MariaDB. StarRocks, Doris y VeloDB podrían cargar más rápido con
  Stream Load, pero eso es HTTP a otros puertos y el driver no tiene cliente
  HTTP; Manticore no tiene `LOAD DATA` ni NULL. OceanBase, SingleStore, Doris,
  VeloDB y Databend, sin probar en vivo (sin contenedor); GreptimeDB, solo
  ida y vuelta. La geometría es el valor interno de MySQL (SRID + WKB).
- **Oracle.** Un `VARCHAR2`, `CHAR` o `RAW` vacío se rechaza, porque Oracle
  lo guarda como NULL. Una identidad `GENERATED ALWAYS` con "conservar
  identidad" se rechaza y el mensaje dice el `ALTER … MODIFY … GENERATED BY
  DEFAULT` que hace falta: DBine nunca modifica la tabla. Con bloqueo de
  tabla (`APPEND_VALUES`) las tablas con LOB o JSON cargan por la vía normal
  (ORA-65501). Los valores de más de 32 KB van de a una fila. Oracle
  Autonomous, sin probar en vivo.
- **SQLite y libSQL.** SQLite rechaza `NaN` (lo guardaría como NULL). La copia
  directa no acepta una base en memoria como origen. Copiando por lotes dentro
  de un mismo archivo, la lectura abierta impide que el modo de diario por
  defecto confirme: la carga falla diciendo que el modo WAL lo evita. Una
  tabla sin `rowid` (vista, `WITHOUT ROWID`) se copia con una sola sentencia,
  y si tarda más de lo que el archivo puede estar bloqueado, se interrumpe y
  pasa a lote. libSQL no tiene copia directa (son dos servidores que no se
  ven) y `sqld` corta un valor o fila a los 5.000.000 de bytes.
- **DuckDB.** La copia directa solo funciona dentro de la misma instancia
  (mismo archivo o `:memory:`): dos archivos distintos son dos instancias y
  adjuntar uno abierto por otra no es seguro. `TIMESTAMPTZ`, `INTERVAL`,
  enums y `JSON` hacia `MAP` pasan por una tabla temporal y una conversión en
  SQL, por límites del Appender.
- **ClickHouse.** `Dynamic` y `Variant` no se leen (su texto pierde el subtipo
  y el NULL); `JSON` y `AggregateFunction` se leen como texto. La copia
  directa no vale con `AggregateFunction`. Un `DateTime` se emite en UTC:
  el nombre de la zona no viaja. Al cargar en `Date` se pierde la hora, en
  `DateTime` los decimales de segundo y en `DateTime64(n)` los dígitos más
  allá de `n`: queda pendiente rechazarlos cuando la parte perdida no es cero.
- **Firebird.** No hay API de lotes de Firebird 4 en el cliente Rust puro; los
  `INSERT` van en `EXECUTE BLOCK` de hasta 800 filas, y las tablas con BLOB de a
  una fila. Un `TIMESTAMP WITH TIME ZONE` con región se lee como texto del
  servidor. Sin copia directa.
- **SAP HANA.** Sin probar en vivo. Se rechaza el valor que el parámetro no
  guarda tal cual: decimales más allá de precisión o escala, `REAL` fuera de
  rango, `NaN` e infinito, fracciones de segundo que no caben, fechas y horas
  inexistentes, texto que no es UTF-8 y GeoJSON. Una sesión con autocommit
  apagado se rechaza (se confirmaría o se perdería su trabajo abierto). Una
  identidad `GENERATED ALWAYS` con "conservar identidad" detiene la carga.
- **ODBC (Db2, Sybase, SQL Anywhere, Informix, Teradata, Vertica, Access,
  dBase…).** La lectura falla en lugar de truncar; el bloqueo de tabla se
  ignora; "conservar identidad" activa `IDENTITY_INSERT` solo en Sybase y en
  SQL Server por ODBC (Db2 `GENERATED ALWAYS` rechaza el valor). Solo se probó
  en vivo el preset genérico contra SQL Server (ODBC 18); la vía fila por fila
  de los controladores sin arreglos de parámetros no se probó. Hive, Impala,
  Spark, Kyuubi y Cloudera leen fila por fila (sus controladores informan mal
  el largo de `STRING`) y no tienen carga masiva: cada `INSERT` es un trabajo
  que escribe un archivo, y la vía real (archivos a HDFS o S3 y `LOAD DATA`) no
  se alcanza por ODBC. NetSuite es de solo lectura.
- **Flight SQL.** Un `INTERVAL` de DuckDB va como texto, dentro de un tipo
  anidado se rechaza, y el máximo no se puede ida y vuelta. Sin transacciones en
  el servidor, cada lote se confirma solo (ver "Carga masiva"). GizmoSQL sin
  ingesta carga fila por fila (~1.500 filas/s).
- **Phoenix y Avatica.** Las horas y marcas de tiempo cruzan el protocolo en
  milisegundos: una fracción más fina hace fallar la carga. `FLOAT` viaja como
  `DOUBLE`, y los arreglos no pueden ser parámetros en serialización JSON. La
  carga nunca pisa una fila existente: una clave repetida falla al contar al
  final. La cota de tamaño de cada lectura se mide justo antes; un escritor
  concurrente puede agrandar un marco. La serialización JSON de Avatica solo
  tiene pruebas unitarias.
- **Aurora DSQL.** Sin probar en vivo contra DSQL real (se probó con
  PostgreSQL de sustituto). DSQL corta toda transacción a los 5 minutos y limita
  cada una a 3.000 filas y 10 MiB, índices incluidos: por eso no usa `COPY` y
  cada ventana es un `INSERT` propio. La lectura por páginas por clave son
  instantáneas separadas: las filas que cambian durante la copia pueden verse
  viejas o nuevas. Sin clave primaria, la tabla se lee en una sola
  sentencia.

**Nube y motores analíticos**

- **Snowflake.** Sin probar en vivo. La API SQL rechaza `PUT` y `GET`, así que
  no hay carga por etapas con archivos: se inserta en una tabla de trabajo
  transitoria junto al destino y al final va un único `INSERT … SELECT`
  atómico. Un valor `VARIANT` con `undefined`, `NaN` o `Infinity` se rechaza.
- **BigQuery.** La carga usa trabajos de carga (gratis, atómicos); el cupo es
  de 1.500 por tabla por día, es decir 150 millones de filas por tabla y día con
  ventanas de 100.000. Leer una tabla base no cuesta; una vista, una tabla
  externa o una lectura con filtro corre una consulta que se factura. Bytes
  que no son UTF-8 hacia `STRING` y texto que no es JSON hacia
  `JSON`/`STRUCT`/`ARRAY` se rechazan. No hay bloqueo de tabla ni identidad.
- **Databricks.** Sin probar en vivo. Carga por `INSERT` a tablas Delta; si
  falla después de un commit, la tabla vuelve a la versión anterior con
  `RESTORE TABLE`, solo cuando las versiones posteriores son exactamente las de
  esta carga. La carga por `COPY INTO` desde un volumen necesitaría un volumen
  que la conexión no configura.
- **Athena.** Sin probar en vivo. Solo cargan las tablas Iceberg (un `INSERT`
  es un commit atómico); una tabla Hive puede dejar archivos escritos en S3 sin
  forma de deshacerlos, y las vistas, Delta Lake y Hudi solo se leen: todas se
  rechazan de entrada con el motivo. Las columnas anidadas con `VARBINARY` se
  rechazan en los dos sentidos.
- **Trino, Presto y Starburst.** No hay API de carga masiva. Un decimal con más
  decimales que la columna, una hora con más fracción que la precisión, o un
  timestamp con hora hacia `DATE` se rechazan en lugar de redondearse. Los
  conectores Iceberg y Delta Lake reciben una sentencia a la vez (los commits
  concurrentes chocan).
- **Dremio.** Solo carga en tablas que aceptan DML (Iceberg). Las columnas
  `STRUCT`, `LIST` y `MAP` no se cargan. Una sentencia a la vez; la carga la
  limita el planificador (~2.000 filas/s).
- **Cloud Spanner.** Las escrituras nunca pisan filas (`insert`); un commit
  que Spanner rechaza por tamaño se parte por la mitad. Probado contra el
  emulador.
- **Drill.** Solo lectura: no tiene `INSERT`, solo `CREATE TABLE AS SELECT`
  desde lo que Drill mismo lee.

**Documentos, clave-valor, búsqueda, grafos y series de tiempo**

- **Cassandra, ScyllaDB, Keyspaces.** Las tablas con contadores no cargan
  (los contadores no admiten `INSERT`). Un valor más fino que el milisegundo
  en un `timestamp` se rechaza, y la conversión de esquemas todavía lo
  informa como pérdida de precisión en lugar de error: pendiente
  alinearlas. Un `datetime` hacia una columna `time` descarta la fecha en
  silencio: queda pendiente rechazarlo. Sin transacciones: la confirmación
  solo marca el ritmo del avance, y una carga cancelada puede dejar aterrizar
  hasta lo que tenía en vuelo. Amazon Keyspaces, sin probar en vivo.
- **MongoDB, FerretDB, DocumentDB.** Las fechas tienen precisión de
  milisegundo (una fracción más fina se corta); `Decimal128` guarda 34
  dígitos (uno más largo falla). Un campo NULL se omite. Por filas entre
  colecciones, una columna que mezcla `objectId` y `string`, o `int` y `long`,
  se rechaza porque no se distingue valor por valor; la copia directa (BSON
  crudo) no tiene ese límite. Un documento con un campo repetido o con claves
  `$` que Extended JSON leería como otro tipo falla en la lectura, con su
  `_id`. DocumentDB, sin probar en vivo.
- **Cosmos DB.** Probado solo contra el emulador (unas 1.000 a 2.000 filas/s
  de carga y 6.000 de lectura). Los ítems sin `id` no cargan, un `id` existente falla la carga, un
  ítem de más de 2 MB se rechaza, y un lote fallido no deja nada. Una respuesta
  perdida (408, 5xx) puede haber sido aplicada: el error lo dice. Un `null`
  explícito y un campo ausente son ambos NULL al pasar a otro motor; entre
  Cosmos DB y Cosmos DB la copia directa mueve los ítems y los distingue. La
  sincronización por filas de Cosmos a Cosmos no existe.
- **DynamoDB.** Un ítem de más de 400 KB, un número de más de 38 dígitos o fuera
  de 1E-130..9,99E+125 se rechaza antes de enviar. La carga nunca reemplaza:
  una clave existente o repetida cancela la transacción y falla la carga. No
  se carga en índices. Sin columnas pedidas, una primera pasada lee todo para
  conocer los atributos (lee dos veces).
- **Couchbase.** La carga va por SQL++ (el driver solo habla REST, sin KV);
  una clave existente falla la carga. Un documento que no es un objeto JSON
  falla la lectura. Un flotante entero (`1.0`) vuelve como entero.
- **CouchDB.** Los documentos de diseño se omiten; `_id` siempre es texto; un
  conflicto (`_id` existente) falla la carga.
- **Elasticsearch, OpenSearch, Open Distro.** Un campo anidado que se mueve a
  una clave con puntos (`metrics.mem`) no pierde nada pero cambia la forma.
  Los campos que el mapeo no lista y las claves con puntos viajan
  juntos en la columna `_source`; sin columnas pedidas, las demás salen del mapeo. Open Distro, sin probar en
  vivo (sin contenedor).
- **Solr.** Solo inserta (`_version_: -1`). Rechaza campos que el esquema no
  define, valores que no caben exactos (Solr trunca fracciones y desborda
  enteros sin avisar), listas vacías o con nulos, y un destino de `copyField`
  cuyo valor no es el que Solr copiaría.
- **Redis, Valkey, Dragonfly.** Sin transacciones que se deshagan: si un
  comando de un `EXEC` falla (`WRONGTYPE`), el resto de la ventana queda
  escrito, se cuenta y la carga falla nombrando las filas. El filtro no se
  admite (solo clave o patrón). Los tipos de módulos se leen con la lectura por
  defecto.
- **etcd.** El filtro no se admite. La carga nunca pisa una clave existente.
  Una clave o valor nulos se rechazan (etcd no tiene ninguno de los dos).
- **Neo4j, Memgraph.** Ver "Carga masiva" para relaciones, segundos
  intercalares, fechas y bytes de Memgraph. Los decimales y UUID van como texto
  y los mapas o listas mixtas como JSON en texto; solo se cargan etiquetas.
- **Amazon Neptune.** Sin probar en vivo. Solo lectura (HTTP JSON): ver
  "Carga masiva".
- **OrientDB.** Se lee y escribe cada clase por separado (no sus subclases).
- **InfluxDB.** Sin compresión en las cargas. Una fila se convierte en un
  punto: exige columna `time`/`_time`. Fallan con `Unsupported`, antes de perder
  nada: un punto sin campos, `NaN` o infinito, un entero sin signo mayor que
  `i64` en 1.x, nombres o valores de etiquetas que el protocolo de línea no
  puede llevar, y en 2.x los nombres que Flux reserva.
- **TDengine.** No usa el protocolo de línea (decide él los tipos). Una marca de
  tiempo más fina que la precisión de la base, `NaN` o un infinito se rechazan.
  Una sentencia que abarca varias subtablas no es atómica.
- **IoTDB y TimechoDB.** Solo el modelo de árbol. La API REST devuelve un
  `BLOB` como texto UTF-8, así que un binario que no es UTF-8 se altera al
  **leer** (escribirlo va por SQL con `X'…'`). Una marca de tiempo más fina que
  la precisión del servidor se rechaza (es la clave: dos filas colapsarían).
  Antes de 1.3.3 no hay `TIMESTAMP`, `DATE` ni `BLOB`. La versión 2.x se
  probó en vivo (`dbine-test-iotdb2`, puerto 27150), salvo la transferencia
  con los tipos nuevos de 2.x: esa prueba deja al contenedor sin memoria.
- **ksqlDB.** No hay reversa: las filas ya insertadas quedan, un reintento
  duplica, y si el servidor rechaza una petición de forma inesperada, las filas
  de esa petición quedan inciertas. `NaN` se rechaza. Una `CREATE TABLE` común
  no se lee hasta el final (solo responde consultas de empuje) y da
  `Unsupported`. Un `TIME` pierde los milisegundos.

**Compartido por todos**

- El origen es siempre de solo lectura, también en la copia directa.
- Las cargas nunca reemplazan filas existentes: una clave que ya está falla la
  carga en los motores que lo detectan (Cosmos, DynamoDB, Couchbase, CouchDB,
  etcd, Solr, Phoenix).
- Cuando la carga falla o se cancela, los pedidos que ya estaban en camino se
  esperan antes de devolver el control, así que no se confirma nada después.
  Donde el motor no tiene transacciones (Cassandra, Redis, MongoDB, ksqlDB,
  TDengine…), lo ya confirmado queda y el error lo dice.

## Túnel SSH

Todos los motores que se conectan por red pueden usar un túnel SSH
([`tuneles-ssh.md`](tuneles-ssh.md)): el túnel es un puerto local, así que no
depende del driver.

| Motor | Motivo |
|---|---|
| SQLite, DuckDB y otros motores de archivo local | No hay servidor: se abre un archivo de la máquina. |
| ODBC con DSN | La conexión la arma el driver ODBC del sistema a partir del DSN; DBine no ve el servidor para redirigirlo. Se puede usar un túnel con un DSN que apunte a `127.0.0.1`. |

## Bloqueos

El panel de bloqueos del Monitor ([`bloqueos.md`](bloqueos.md)). Tienen
bloqueos y permiten terminar sesiones: SQL Server, Azure SQL, PostgreSQL,
TimescaleDB, AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu,
KingbaseES, openGauss, Greenplum, Cloudberry, Greengage, YugabyteDB,
CockroachDB, H2, Redshift, Yellowbrick, MySQL, MariaDB, Aurora MySQL,
TiDB, Oracle, Oracle Autonomous, SAP HANA, MongoDB, Amazon DocumentDB,
Neo4j, Snowflake y, por ODBC, Db2 LUW, Sybase ASE y SQL Anywhere.

Redshift, Yellowbrick, KingbaseES, Fujitsu, SAP HANA, DocumentDB,
Snowflake, Db2, Sybase ASE y SQL Anywhere se implementaron según la
documentación del fabricante, sin servidor de prueba.

| Motor | Qué falta | Motivo |
|---|---|---|
| Informix, GBase 8s (ODBC) | Terminar sesiones | Terminar una sesión exige `task('onmode','z',…)` de la base sysadmin, que solo corre conectado a esa base. |
| Babelfish | Todo | Babelfish no implementa por TDS las vistas de SQL Server que informan bloqueos (`sys.dm_exec_requests.blocking_session_id`). |
| Microsoft Fabric Data Warehouse | Todo | El almacén no expone las vistas de bloqueos ni `KILL`. |
| Firebird | Todo | Las tablas MON$ no informan qué transacción o conexión bloquea a otra (solo que una sentencia está activa); sin eso no hay cadena de bloqueos. |
| Materialize | Todo | Materialize no tiene bloqueos entre sesiones: ordena lecturas y escrituras por marcas de tiempo, sin locks. |
| RisingWave | Todo | Es una base de streaming sin transacciones de escritura interactivas. |
| CrateDB | Todo | No tiene transacciones ni bloqueos de filas: ninguna sesión espera a otra. |
| Denodo | Todo | Es una capa de virtualización: los bloqueos ocurren en las fuentes de datos, no en Denodo. |
| OceanBase | Todo | No hay una vista documentada y estable de esperas de bloqueo accesible por SQL en modo MySQL. |
| SingleStore | Todo | No expone por SQL quién bloquea a quién de forma confiable. |
| StarRocks, Doris, VeloDB, Databend, Manticore, GreptimeDB | Todo | Motores analíticos o de búsqueda sin bloqueos de fila entre sesiones. |
| Db2 for i, Db2 for z/OS (ODBC) | Todo | Sus vistas de esperas de bloqueo difieren de Db2 LUW y no se pudieron validar. |
| Otros motores por ODBC | Todo | No hay una vista de esperas de bloqueo confiable para ese motor por ODBC. |
| FerretDB | Todo | No informa esperas por bloqueos ni permite terminar operaciones (no tiene `killOp`). |
| Memgraph | Todo | No hace esperar a una transacción por otra: la segunda escritura falla enseguida con un error de serialización. |
| Amazon Neptune | Todo | No informa bloqueos entre transacciones. |
| Azure Cosmos DB | Todo | Usa concurrencia optimista con ETags: no hay bloqueos entre sesiones. |
| Cloud Spanner | Todo | Los bloqueos solo se ven como estadísticas históricas (`SPANNER_SYS.LOCK_STATS_*`), sin quién bloquea a quién en vivo, y no se puede terminar la transacción de otro cliente. |
| Databricks | Todo | Delta usa concurrencia optimista: una escritura en conflicto falla, no espera. |
| ClickHouse | Todo | No hay transacciones ni bloqueos de fila; las esperas por bloqueos de tabla no dicen quién los retiene. |
| Trino, Presto, Starburst | Todo | Es un motor de consultas sin bloqueos entre consultas; las esperas son colas de grupos de recursos. |
| Redis, Valkey, Dragonfly, etcd | Todo | No hay transacciones que esperen a otras: cada comando se ejecuta solo. |
| Cassandra, ScyllaDB, Amazon Keyspaces | Todo | No hay bloqueos entre sesiones (las transacciones livianas usan Paxos, sin esperas visibles). |
| Elasticsearch, OpenSearch, Solr | Todo | No hay transacciones ni bloqueos entre clientes. |
| InfluxDB, IoTDB, TDengine, ksqlDB, BigQuery, Athena, Dremio, Drill, Phoenix, Couchbase, CouchDB, DynamoDB, Flight SQL, libSQL, Aurora DSQL | Todo | No informan esperas de bloqueo entre sesiones que se puedan consultar. |
| SQLite, DuckDB | Todo | Son bases de archivo sin servidor: no hay otras sesiones que ver. |

## Usuarios y permisos

La pestaña **Usuarios y permisos** ([`usuarios-y-permisos.md`](usuarios-y-permisos.md)).
La tienen:

- **Relacionales y compatibles:**
  - Familia SQL Server: SQL Server, Azure SQL, Microsoft Fabric Data Warehouse y Babelfish.
  - Familia PostgreSQL: PostgreSQL, TimescaleDB, YugabyteDB, openGauss, Cloudberry, Greengage, Greenplum, KingbaseES, EDB, Fujitsu, Yellowbrick, AlloyDB, Cloud SQL, Aurora PostgreSQL, Aurora DSQL, CockroachDB, Materialize, Redshift, CrateDB, H2 y RisingWave.
  - Familia MySQL: MySQL, Aurora MySQL, Cloud SQL for MySQL, MariaDB, TiDB, OceanBase, SingleStore, StarRocks, Apache Doris, VeloDB y Databend.
  - Otros: Oracle, SAP HANA y Firebird.
- **Analíticas y en la nube:** ClickHouse, Timeplus Proton, Snowflake, Databricks, BigQuery, Cloud Spanner, Trino, Presto, Starburst y Dremio.
- **Documentos, grafos y clave-valor:** MongoDB, FerretDB (solo usuarios), Amazon DocumentDB, Couchbase, CouchDB, Azure Cosmos DB, OrientDB, Neo4j, Memgraph, Cassandra, ScyllaDB, Redis, Valkey, Dragonfly y etcd.
- **Búsqueda y series de tiempo:** Elasticsearch, OpenSearch, Solr, InfluxDB 1.x, TDengine, IoTDB y TimechoDB.
- **Por ODBC:** Db2 LUW, Db2 for i, Db2 for z/OS, Hive/Cloudera, Impala, Vertica, Exasol, Teradata, Sybase ASE, SQL Anywhere, Informix, GBase 8s, Netezza, Altibase, Dameng, CUBRID, Zen, Mimer, MonetDB, IRIS/Caché, MaxDB, NuoDB, HeavyDB, SQream, Ingres, Virtuoso, OpenEdge, Machbase, Ignite y Ocient.

Probados contra servidores reales:

- SQL Server, Babelfish, PostgreSQL, TimescaleDB, YugabyteDB, openGauss, Cloudberry, Greengage, CockroachDB, Materialize, H2 y RisingWave.
- MySQL, MariaDB, TiDB, StarRocks, Apache Doris, Databend, ClickHouse y Oracle.
- MongoDB, Couchbase, Neo4j (Enterprise y Community), Memgraph Community, Cassandra, ScyllaDB, Redis y etcd.
- Elasticsearch, OpenSearch, Solr, InfluxDB, TDengine, IoTDB, OrientDB y Trino.
- Dremio: solo la edición OSS, donde los permisos son de Enterprise.
- Cosmos DB y Aurora DSQL: contra el emulador de Cosmos y, para DSQL, contra un PostgreSQL de prueba.
- Spanner y BigQuery: sus emuladores ejecutan los scripts pero no aplican los permisos.

El resto se implementó según la documentación del fabricante, con pruebas unitarias: HANA, Snowflake, Databricks, Fabric, SingleStore, OceanBase, Firebird, CouchDB y los presets ODBC. En los presets ODBC hay detalles de catálogo que falta confirmar contra un servidor: los bits de privilegios de Netezza, los códigos de Teradata y las columnas de SQream.

| Motor | Qué falta | Motivo |
|---|---|---|
| Neo4j Community, Memgraph Community | Roles y permisos | Son funciones de la edición Enterprise; se ven y administran los usuarios. |
| Neo4j, Memgraph, Cassandra, ScyllaDB | «Puede otorgarlo a otros» | Esos motores no tienen `WITH GRANT OPTION`. |
| Neo4j | Permisos a un usuario | Neo4j otorga permisos solo a roles. |
| CockroachDB, Redshift | Permisos sobre la base entera | El script no puede apuntar a la base actual (sin `DO` dinámico); se escribe el `GRANT` a mano. |
| Redis, Valkey, Dragonfly, etcd, InfluxDB 1.x | Roles (Redis, InfluxDB) / deshabilitar (etcd, InfluxDB) | El motor no los tiene: los permisos van por usuario (reglas ACL en Redis). |
| Elasticsearch, OpenSearch | Agregar o quitar un rol o privilegio suelto | La API reescribe el rol o el usuario entero; el script lo explica y se hace desde la consola. |
| MongoDB, CouchDB, Memgraph, OpenSearch (usuarios internos) | Deshabilitar el ingreso | El motor no tiene esa opción: se cambia la contraseña o se borra el usuario. |
| FerretDB | Roles y permisos | No los tiene: todo usuario que entra tiene acceso completo. |
| InfluxDB 2 y 3 | Todo | Autorizan con tokens de API, no con usuarios y permisos que se cambien por consultas. |
| Amazon Neptune, Amazon Keyspaces | Todo | El acceso se controla con IAM de AWS, no desde el motor. |
| Denodo | Todo | Administra usuarios y roles desde su servidor (VQL/consola), no por SQL de PostgreSQL. |
| Manticore | Todo | No tiene usuarios ni permisos. |
| GreptimeDB | Todo | Los usuarios salen de la configuración del servidor (proveedor estático), no de SQL. |
| SQLite, DuckDB | Todo | Son bases de archivo sin usuarios. |
| libSQL | Todo | Autoriza con tokens JWT, no con usuarios de SQL. |
| Drill, ksqlDB | Todo | Los usuarios vienen de la configuración del servidor (PAM, JAAS), no se administran por consultas. |
| Flight SQL | Todo | Es un protocolo genérico: los permisos dependen del servidor que está detrás. |
| Phoenix | Todo | Sus `GRANT`/`REVOKE` (solo con `phoenix.acls.enabled`) escriben ACL de HBase (`hbase:acl`) que no se leen por SQL: no hay `SHOW GRANTS`, ni tabla `SYSTEM`, ni metadato de Avatica con los permisos. Los usuarios y grupos son de HBase, Kerberos o LDAP, y Phoenix no los lista ni informa el usuario actual. |
| Snowflake | Otorgar permisos a un usuario | Snowflake otorga permisos solo a roles: el script lo hace sobre un rol y explica cómo sumar al usuario. |
| Databricks | Crear usuarios y grupos, contraseñas, membresías | Se administran en la consola de la cuenta o del área de trabajo (SCIM), no con SQL. Unity Catalog no tiene `WITH GRANT OPTION`. |
| Trino, Presto, Starburst | Usuarios, contraseñas | Trino no tiene usuarios propios (vienen del autenticador); los roles y permisos dependen del conector y del control de acceso configurado (por ejemplo, `hive.security=sql-standard`). |
| Aurora DSQL | Contraseñas | Se ingresa con tokens de IAM; el vínculo con un rol de IAM queda comentado en el script para completar el ARN. |
| Cloud Spanner | Usuarios, permisos sobre la base entera, `WITH GRANT OPTION` | Los usuarios son principales de IAM; Spanner no tiene esos permisos. |
| BigQuery | Usuarios, grupos, roles del proyecto | Son de Google Cloud IAM; los roles del proyecto necesitan el nombre del proyecto y otra API. Se ve por dataset. |
| Athena | Todo | Los permisos se administran en IAM y Lake Formation; no hay SQL para eso. |
| OrientDB | Usuarios del servidor, roles dentro de roles, `WITH GRANT OPTION` | Los usuarios del servidor están en su configuración; OrientDB no tiene lo demás. |
| Solr | Otorgar y quitar permisos, roles, deshabilitar | `set-permission` y `set-user-role` reemplazan la lista entera, así que se hacen desde la consola; los roles existen solo mientras algo los nombra; la autenticación básica no deshabilita usuarios. |
| TDengine | Roles | El motor no tiene roles. |
| IoTDB | Deshabilitar, roles dentro de roles | El motor no los tiene. |
| Couchbase | Miembros de grupos, permisos sobre un scope, bloquear usuarios | SQL++ reemplaza la lista de grupos entera y no tiene lo demás. Necesita Couchbase 8.0 o posterior; los grupos y la mayoría de los roles son de Enterprise. |
| Azure Cosmos DB | Roles, contraseñas, deshabilitar | Son usuarios y permisos del plano de datos (tokens de recurso); el control de acceso de Entra ID es del plano de control de Azure. |
| Dremio | Todo en la edición OSS; permisos sobre un space o source entero | Los permisos son de Enterprise y Cloud; desde el nombre no se sabe si va `ON SPACE` u `ON SOURCE`. |
| Microsoft Fabric Data Warehouse | Contraseñas, deshabilitar, borrar usuarios | Los usuarios son identidades de Entra ID y el acceso se da con roles del área de trabajo; `DROP USER` no está documentado. |
| Babelfish | `DENY`, permisos sobre la base entera, `ALTER`/`CONTROL`/`VIEW DEFINITION`, nombres con `]` | Babelfish los rechaza. |
| H2 | Deshabilitar, `WITH GRANT OPTION` | H2 no los tiene. Pasa los nombres de usuario a minúsculas. |
| RisingWave | Roles, membresías, permisos sobre la base actual sin nombre | RisingWave no tiene roles; los permisos sobre una base se dan eligiéndola por nombre. |
| StarRocks, Apache Doris, VeloDB, Databend | Deshabilitar el ingreso; en Doris y Databend, `WITH GRANT OPTION`; en Doris, roles dentro de roles | Los motores no los tienen. En Doris, los permisos sobre tablas todavía no se ofrecen desde la pestaña: solo se ven y se revocan. |
| SingleStore | Un rol otorgado directo a un usuario | Su modelo es usuario → grupo → rol. |
| DynamoDB | Todo | Los permisos son de IAM, un servicio aparte. |
| ODBC: Informix, GBase 8s, Db2 (LUW, i, z/OS), Hive, Impala | Crear usuarios y contraseñas | Los usuarios son del sistema operativo, LDAP, Kerberos o RACF. |
| ODBC: Spark, Kyuubi, Access, dBase, Ignite 3, NetSuite, ODBC genérico | Todo | Spark y Kyuubi autorizan desde el catálogo o Ranger; Access y dBase no tienen usuarios; Ignite 3 se configura en el clúster; NetSuite es de solo lectura; con ODBC genérico no se sabe qué motor hay detrás. |

## Nuevo esquema y borrar esquema

Crear y borrar esquemas desde el explorador ([`esquemas.md`](esquemas.md)).
Clic derecho sobre una base → «Nuevo esquema…» (con dueño y permisos al
crearlo, en un solo script que se ve antes de ejecutarlo) y sobre un esquema →
«Borrar esquema…». Solo en los motores cuyo explorador muestra esquemas.

La tienen:

- **Familia SQL Server:** SQL Server, Azure SQL, Microsoft Fabric Data Warehouse y Babelfish.
- **Familia PostgreSQL:** PostgreSQL, TimescaleDB, YugabyteDB, openGauss, Cloudberry, Greengage, Greenplum, KingbaseES, EDB, Fujitsu, Yellowbrick, AlloyDB, Cloud SQL, Aurora PostgreSQL, Aurora DSQL, CockroachDB, Materialize, Redshift, RisingWave y H2.
- **Analíticas y en la nube:** Snowflake, Databricks, Trino, Presto, Starburst, Dremio (carpetas) y Cloud Spanner.
- **Otros:** DuckDB, Arrow Flight SQL, Couchbase (scopes) y Apache Phoenix.
- **Por ODBC:** Db2 LUW, Db2 for i, Hive/Cloudera, Impala, Spark, Kyuubi, Vertica, Exasol, Netezza, Dameng, MonetDB, Mimer, MaxDB, NuoDB, Ignite 3, SQream y Ocient.

**Esquemas vacíos en el explorador.** Un esquema recién creado, sin objetos,
aparece en el árbol en todos los motores con «Nuevo esquema», porque listan
sus esquemas:

- **Familia SQL Server:** `sys.schemas`. En Babelfish solo trae `dbo`,
  `guest` y los esquemas de usuario.
- **Familia PostgreSQL:** `pg_namespace`; H2 y CrateDB, `information_schema.schemata`.
  En openGauss, el esquema personal de cada usuario se lista como esquema de
  usuario. Materialize lista solo la base actual y los `mz_*`.
- **Analíticas y en la nube:** Snowflake, Databricks
  (`<catálogo>.information_schema.schemata`), Trino, Presto, Starburst,
  Dremio (carpetas), Cloud Spanner y Aurora DSQL.
- **Otros:** DuckDB (sin el catálogo `system`), Flight SQL (`GetDbSchemas`;
  si el servidor no lo responde, los esquemas vuelven a salir de las tablas),
  Couchbase (scopes) y Phoenix.
- **Por ODBC:** Db2 LUW y Db2 for i, Vertica, Exasol, Dameng, MonetDB,
  Mimer, MaxDB, NuoDB, SQream y Hive/Cloudera/Impala/Spark/Kyuubi, con la
  consulta del catálogo de cada uno; Netezza, Ocient e Ignite 3, con
  `SQLTables` del driver ODBC.

Los esquemas del sistema se marcan y el árbol los oculta mientras no tengan
objetos: `sys`, `INFORMATION_SCHEMA`, `guest` y los de un rol fijo `db_*` en
SQL Server (por eso un esquema de usuario cuyo dueño es un rol `db_*` también
cuenta como del sistema), además de `queryinsights` en Fabric; `pg_catalog`,
`information_schema` y los propios de cada motor en la familia PostgreSQL
(`_timescaledb_*`, `timescaledb_information` y `timescaledb_experimental` en
TimescaleDB; `crdb_internal` y `pg_extension` en CockroachDB; `mz_*` en
Materialize; `rw_catalog` en RisingWave); `information_schema`, `pg_catalog`
y `sys` en Flight SQL; `_system` en Couchbase. En los presets ODBC, Db2,
Vertica y MonetDB usan la marca del propio catálogo, y Netezza, Ocient e
Ignite 3, la lista del preset.

En SAP HANA y Oracle los esquemas son las bases del explorador y se listan
aunque estén vacíos (`SYS.SCHEMAS` en HANA; `ALL_USERS` en Oracle, salvo en
11g, donde solo aparecen los usuarios con objetos). Denodo no tiene esquemas.
En los motores sin «Nuevo esquema» que muestran esquemas, un esquema aparece
cuando tiene su primer objeto, porque sale de la lista de objetos.

**Permiso para crear.** «Nuevo esquema…» se deshabilita cuando el servidor
dice que el usuario no puede crear esquemas: SQL Server y Azure SQL
(`HAS_PERMS_BY_NAME(…, 'CREATE SCHEMA')`), la familia PostgreSQL (`CREATE`
sobre la base), Cloud Spanner (`databases.updateDdl`), Aurora DSQL,
Db2 LUW (`DBADM`) y SAP HANA. Queda habilitado sin chequeo en Fabric,
Babelfish, RisingWave, H2 (salvo administradores: `ALTER ANY SCHEMA` no se
lee), Snowflake (salvo con los roles de sistema), Databricks, Trino y Dremio.

### Motores sin «Nuevo esquema»

| Motor | Motivo |
|---|---|
| Oracle | Un esquema es un usuario con contraseña: se crea y se borra desde «Usuarios y permisos». |
| Familia MySQL (MySQL, Aurora MySQL, Cloud SQL for MySQL, MariaDB, TiDB, OceanBase, SingleStore, StarRocks, Apache Doris, VeloDB, Databend) | El esquema es la base: se crea con «Nueva base». |
| SAP HANA | Los esquemas de HANA son las bases del explorador: se crean y borran con «Nueva base» y «Borrar base». Pendiente: elegir el dueño (`OWNED BY`) y los permisos desde «Nueva base». |
| Firebird | No tiene esquemas antes de la versión 6.0. Pendiente: mostrarlos y crearlos en Firebird 6.0. |
| ODBC genérico | No se sabe qué motor hay detrás. |
| ODBC: Sybase ASE, SQL Anywhere, Informix, GBase 8s, Altibase, Ingres, OpenEdge, Machbase | El esquema es el usuario dueño de los objetos: se crea con el usuario, desde «Usuarios y permisos». |
| ODBC: Teradata | Un esquema es una base con espacio propio (`CREATE DATABASE … PERM`), no un objeto simple. |
| ODBC: Db2 for z/OS, IRIS/Caché, Virtuoso, Ignite 2 | El esquema es un calificador implícito: aparece al crear el primer objeto que lo nombra. |
| ODBC: CUBRID, Zen, Access, dBase, HeavyDB | No tienen esquemas. |
| ODBC: NetSuite (SuiteAnalytics Connect) | Es de solo lectura. |
| Amazon Athena | El explorador no tiene nivel de esquema: las bases de Glue son las bases del explorador (Athena llama esquema a la base). |
| Google BigQuery | El explorador no tiene nivel de esquema: los datasets (lo que BigQuery llama esquema) son las bases del explorador. |
| SQLite, libSQL | No tienen esquemas: las bases adjuntas (`ATTACH`) son archivos aparte, no esquemas que se crean con SQL. |
| ClickHouse | Solo tiene bases, sin esquemas: se crean con «Nueva base». |
| DuckDB: consulta de archivos | No hay una base donde guardar un esquema: los archivos se leen en una base en memoria. |
| Calcite Avatica (Phoenix genérico) | El DDL es el del motor que hay detrás del servidor Avatica, y no se sabe cuál es. |
| CrateDB | No tiene `CREATE SCHEMA` ni `DROP SCHEMA`: un esquema existe mientras tenga alguna tabla. |
| Denodo | No tiene esquemas: una base virtual contiene sus vistas directamente. |
| Los demás motores de documentos, clave-valor, búsqueda y series de tiempo; Cassandra, ScyllaDB (keyspaces), Apache Drill (workspaces) | Su explorador no tiene nivel de esquema. |

### Diferencias por motor

| Motor | Qué falta | Motivo |
|---|---|---|
| SQL Server, Azure SQL, Microsoft Fabric, Babelfish | Borrar con su contenido | `DROP SCHEMA` de T-SQL no tiene `CASCADE` y se niega mientras el esquema tenga objetos: hay que borrarlos o moverlos antes. |
| SQL Server, Azure SQL | El permiso `UNMASK` sobre un esquema | Solo lo aceptan SQL Server 2022 y posteriores; no se ofrece. |
| SQL Server, Azure SQL | Cambiar el dueño después de otorgar | El dueño va en `CREATE SCHEMA … AUTHORIZATION`: `ALTER AUTHORIZATION ON SCHEMA` borra todos los permisos ya otorgados sobre el esquema (comprobado en vivo). Quien crea sin ser `db_owner` necesita `CREATE SCHEMA`, `IMPERSONATE` sobre el usuario dueño (o `ALTER` sobre el rol dueño) y ser miembro de `db_securityadmin` para otorgar sobre el esquema que cede. |
| Microsoft Fabric | Elegir el dueño | `AUTHORIZATION` no se pudo verificar sin un warehouse; el esquema queda a nombre de quien lo crea. |
| Babelfish | «Con opción de otorgar», nombres con `]` y permisos fuera de SELECT, INSERT, UPDATE, DELETE, REFERENCES y EXECUTE | Babelfish rechaza `GRANT … ON SCHEMA … WITH GRANT OPTION` y no lee `]]` dentro de corchetes. Tampoco tiene `ALTER AUTHORIZATION` sobre esquemas y rechaza `GRANT CREATE SCHEMA`: quien crea necesita `db_ddladmin` y `db_securityadmin`. |
| Materialize | «Con opción de otorgar» | Materialize no la tiene. Tampoco acepta `AUTHORIZATION`: el dueño se asigna después con `ALTER SCHEMA … OWNER TO`. |
| H2 | «Con opción de otorgar» | H2 no la tiene. Los permisos sobre un esquema son SELECT, INSERT, UPDATE y DELETE. Solo un administrador crea esquemas. |
| RisingWave, Redshift, H2 | Un rol o grupo como dueño | El dueño tiene que ser un usuario y la lista de dueños muestra solo usuarios (RisingWave no tiene roles; Redshift rechaza un grupo con un mensaje; en H2 2.1, un rol como dueño deja la base sin poder abrirse). |
| ODBC: Db2 LUW | Borrar con su contenido | `DROP SCHEMA` de Db2 solo acepta `RESTRICT` (esquema vacío). |
| ODBC: SQream, Ocient | Borrar con su contenido | `DROP SCHEMA` solo borra un esquema vacío. |
| ODBC: Db2 for i, Mimer, MaxDB, NuoDB, Ignite 3, SQream, Ocient, Spark, Kyuubi | Elegir el dueño | El esquema queda a nombre de quien lo crea (Spark y Kyuubi no tienen dueño). |
| ODBC: Db2 LUW | Permisos al crear, sin `ACCESSCTRL` | El dueño va en `CREATE SCHEMA … AUTHORIZATION` y los `GRANT … ON SCHEMA` siguientes necesitan `ACCESSCTRL` o `SECADM`: un `DBADM` sin `ACCESSCTRL` crea el esquema pero falla al otorgar. Pendiente: ceder el dueño al final (`TRANSFER OWNERSHIP`). Sin probar: no hay contenedor de Db2. |
| ODBC: Db2 LUW, Vertica, Netezza, Dameng | Un rol como dueño | Solo un usuario puede ser dueño de un esquema (`AUTHORIZATION` nombra un usuario): la lista de dueños muestra solo usuarios. |
| ODBC: Hive, Cloudera, Netezza, Dameng, MonetDB, Mimer, NuoDB, Ignite 3, Ocient, Spark, Kyuubi, Db2 for i | Permisos al crear | Pendiente: el script de permisos todavía no escribe `GRANT` sobre un esquema en estos motores (MonetDB no tiene permisos por esquema: se da el dueño). |
| ODBC: MaxDB, SQream, Exasol | «Con opción de otorgar» | Sus permisos sobre un esquema no la tienen (Exasol no otorga permisos sobre objetos con opción de otorgarlos). El formulario muestra la opción igual y el script la rechaza en la vista previa; pasa lo mismo en H2, Materialize, Babelfish y Cloud Spanner. Pendiente: que `SchemaSpec` diga si el motor la tiene, para ocultarla. |
| ODBC: Vertica, Impala | `ALL` junto con otros permisos | `ALL` ya los incluye; se elige solo. Impala escribe un `GRANT` por permiso. |
| ODBC: Hive, Cloudera, Impala | Nombres con espacios o símbolos | Los nombres de base solo admiten letras, números y `_`. |
| Presto | Elegir el dueño, permisos al crear y borrar con su contenido | Presto no acepta `AUTHORIZATION` ni `ON SCHEMA`, y `DROP SCHEMA … CASCADE` «is not yet supported»: solo borra un esquema vacío. |
| Trino, Starburst | Nada, pero depende del catálogo | Dueño, permisos y `CASCADE` los acepta o rechaza el conector. El dueño se cambia al final del script (`ALTER SCHEMA … SET AUTHORIZATION`); el catálogo `memory` rechaza ese cambio, los roles y los permisos («does not support permission management»). |
| Snowflake | Borrar solo si está vacío | `DROP SCHEMA` de Snowflake siempre borra el contenido (`RESTRICT` solo frena por claves foráneas de otros esquemas); se avisa en el script. |
| Snowflake | Un usuario como dueño | El dueño es siempre un rol (`GRANT OWNERSHIP … TO ROLE … COPY CURRENT GRANTS`, al final del script para conservar los permisos recién otorgados): la lista de dueños muestra solo roles. |
| Databricks | `WITH GRANT OPTION` | Unity Catalog no lo tiene: «con opción de otorgar» otorga además `MANAGE` sobre el esquema. |
| Dremio | Elegir el dueño y borrar con su contenido | Un esquema es una carpeta: no tiene cláusula de dueño (es el permiso `OWNERSHIP`) ni `CASCADE`. Solo se crean carpetas en orígenes de catálogo (Nessie, Iceberg REST, Arctic), no en espacios. |
| Dremio | Borrar una carpeta con punto en el nombre, dentro de un origen | El espacio u origen sale de la base donde se abrió el menú, entero aunque tenga puntos (`@ana.b`). Las carpetas siguientes las da INFORMATION_SCHEMA con puntos sin comillas (`origen.a.b`), igual para la carpeta `a.b` que para `a` › `b`, así que se toman como carpetas anidadas. Los espacios no admiten puntos en sus carpetas; en un origen, la carpeta `a.b` se borra escribiendo el script con comillas (`origen."a.b"`). |
| Dremio | «Con opción de otorgar» | Dremio no tiene `WITH GRANT OPTION`: se otorga además `MANAGE GRANTS` sobre la carpeta. Los permisos sobre carpetas son de Dremio Enterprise y Cloud; en la edición OSS fallan al ejecutarse. |
| Google Cloud Spanner | Elegir el dueño y borrar con su contenido | Los esquemas con nombre no tienen dueño y `DROP SCHEMA` solo borra uno vacío. |
| Google Cloud Spanner | Permisos al crear, en el emulador | Se ofrece `USAGE` (`GRANT USAGE ON SCHEMA … TO ROLE`, del control de acceso detallado); el emulador lo rechaza al ejecutarse. La prueba en vivo verifica ese rechazo; contra Spanner real se corre con `DBINE_TEST_SPANNER_SCHEMA_GRANTS=1` (sin probar: no hay instancia). Sin «con opción de otorgar»: Spanner no la tiene. |
| DuckDB | Elegir el dueño y permisos al crear | DuckDB no tiene usuarios ni permisos. |
| Arrow Flight SQL | Elegir el dueño y permisos al crear | Por Flight SQL no se pueden listar los usuarios del motor. |
| Arrow Flight SQL | Catálogo donde se abrió el menú, fuera de DuckDB | El script escribe el esquema con su catálogo (`"catálogo"."esquema"`). Solo está comprobado que lo acepta un servidor DuckDB (GizmoSQL); con otros motores (Doris, DataFusion, Dremio) no se probó. Con DuckDB detrás, la sesión además fija el catálogo con `USE` al conectar, así que si el catálogo ya no existe la conexión falla («no se pudo abrir el catálogo»). |
| Couchbase | Elegir el dueño y borrar solo si está vacío | Un esquema es un scope (`bucket.scope`; el nombre viene completado con el bucket donde se abrió el menú). No tiene dueño y `DROP SCOPE` siempre borra sus colecciones: hay que marcar «con su contenido». |
| Couchbase | Permisos al crear, en Community Edition | Los roles sobre un scope (`` GRANT … ON default:`bucket`.`scope` ``) son de Enterprise Edition: Community los rechaza al ejecutarse («Role … is not valid»). Sin probar en Enterprise (no hay contenedor): la sintaxis está cubierta por pruebas unitarias. |
| Apache Phoenix | Elegir el dueño y borrar con su contenido | Sin dueño y sin `CASCADE`: `DROP SCHEMA` solo borra un esquema vacío. Necesita `phoenix.schema.isNamespaceMappingEnabled`. |
| Apache Phoenix | Nombres con espacios, símbolos o letras fuera de ASCII | Un esquema es un namespace de HBase: solo letras ASCII, números y `_` (Phoenix tampoco admite `"` en un nombre). Pendiente: DBine todavía acepta letras con acento o `ñ`, que HBase rechaza al ejecutar. |
| Apache Phoenix | Permisos en esquemas con minúsculas | Los permisos son ACL de HBase (R, W, X, C, A; necesitan `phoenix.acls.enabled`) y `GRANT … ON SCHEMA` pasa el nombre a mayúsculas: solo se otorgan en esquemas con el nombre en mayúsculas. «Con opción de otorgar» agrega el permiso A (admin), que es el que deja otorgar en HBase. |
| Aurora DSQL | Borrar con su contenido | DSQL ejecuta una sentencia DDL por transacción y no borra objetos en cascada; se borra el esquema vacío. Sin probar en DSQL real (no hay emulador). |

Probados contra servidores reales (crear con dueño y permisos, comprobarlos en
el catálogo, rechazar el borrado de un esquema con objetos y borrar):

- SQL Server 2022 (con un creador que no es `db_owner` ni `sysadmin`) y
  Babelfish 5.4 (con un creador que solo está en `db_ddladmin` y
  `db_securityadmin`): dueño usuario y rol, permisos y esquemas vacíos en la
  lista, con los del sistema marcados.
- PostgreSQL 16, TimescaleDB, YugabyteDB, CockroachDB, openGauss, Greengage,
  Materialize, RisingWave y H2, con un creador que no es superusuario (en H2,
  un administrador), y el esquema vacío en la lista.
- Trino, Presto, Cloud Spanner (emulador) y Aurora DSQL (contra un PostgreSQL
  de prueba, con un creador que no es superusuario).
- Dremio OSS: el rechazo en `$scratch`, la sintaxis y una carpeta vacía de un
  espacio, listada y borrada; no hay un origen de catálogo en el contenedor.
- Oracle y SAP HANA (bases del explorador): Oracle lista los esquemas vacíos;
  HANA, solo con pruebas unitarias.
- DuckDB, Arrow Flight SQL (GizmoSQL) y Couchbase Community.

Sin probar contra un servidor, con pruebas unitarias según la documentación
del fabricante:

- Azure SQL, Fabric y Redshift.
- Snowflake, Databricks, Couchbase Enterprise y Spanner real.
- Arrow Flight SQL con motores que no son DuckDB.
- Apache Phoenix: la prueba en vivo existe, pero falta confirmarla contra el
  contenedor de prueba.
- Los presets ODBC: no hay contenedores ni drivers ODBC de esos motores.

## Crear bases: opciones

«Nueva base de datos» ([`crear-bases.md`](crear-bases.md)) ofrece opciones
avanzadas, con «Ver script» y sugerencias del servidor, en: SQL Server y
Azure SQL; la familia PostgreSQL (PostgreSQL, TimescaleDB, EDB, Fujitsu,
AlloyDB, Cloud SQL, Aurora, KingbaseES, Greenplum, Cloudberry, Greengage,
YugabyteDB, openGauss, CockroachDB, Redshift, RisingWave y Yellowbrick); la
familia MySQL (MySQL, MariaDB, TiDB, OceanBase, SingleStore, StarRocks, Doris,
VeloDB y GreptimeDB); Oracle, SAP HANA, Firebird, Sybase ASE y Netezza (por
ODBC); ClickHouse, Snowflake, BigQuery, Databricks, Athena y Cloud Spanner;
Cassandra, ScyllaDB, Amazon Keyspaces, Couchbase, CouchDB, OrientDB,
InfluxDB 1, 2 y 3, IoTDB, TDengine, Neo4j y Cosmos DB.

**Probado contra servidores reales** (`tests/create_database.rs` de cada
driver): SQL Server; PostgreSQL, TimescaleDB, openGauss, YugabyteDB,
Greengage, CockroachDB y RisingWave; MySQL, MariaDB, TiDB y GreptimeDB;
Oracle, Firebird, ClickHouse, Cassandra, Couchbase (Community), CouchDB,
OrientDB, InfluxDB 1, 2 y 3, IoTDB 1 y 2, TDengine y Neo4j Enterprise.
BigQuery y Spanner, contra sus emuladores. Materialize y StarRocks, solo la
creación sin opciones.

**Sin verificar contra un servidor real** (siguen la documentación del
fabricante): Azure SQL, Snowflake, Databricks, Athena, Redshift, Yellowbrick,
KingbaseES, Greenplum, Cloudberry, OceanBase, SingleStore, Doris, SAP HANA,
Sybase ASE, Netezza, Cosmos DB, ScyllaDB, Amazon Keyspaces y Timeplus. EDB,
Fujitsu, AlloyDB, Cloud SQL y Aurora usan el mismo código que PostgreSQL.

| Motor | Qué falta | Motivo |
|---|---|---|
| Materialize | opciones | `CREATE DATABASE` solo toma el nombre. |
| Databend | opciones | `CREATE DATABASE` toma un `ENGINE` con un solo valor útil. |
| Memgraph | opciones | `CREATE DATABASE` solo toma el nombre. |
| Timeplus Proton | opciones | `CREATE DATABASE` solo toma el nombre. |
| Dremio | opciones | `CREATE` solo toma el nombre. |
| Microsoft Fabric | opciones | Las bases se crean desde su portal. |
| Babelfish | opciones | T-SQL de Babelfish no tiene opciones de `CREATE DATABASE`. |
| Neptune | crear bases | Una sola base por clúster: no hay nada que crear. |
| MongoDB | crear bases | Las bases se crean solas al escribir el primer documento. |
| DuckDB | opciones | La base es un archivo; mostrar su ruta en «Ver script» exige cambiar el contrato. Ver pendientes. |
| ODBC genérico | opciones | No se sabe qué motor hay detrás, así que no se puede armar un `CREATE DATABASE` con opciones. |
| Trino | crear bases | No crea bases desde DBine. Ver pendientes (esquemas). |
| Apache Drill, Apache Phoenix | crear bases | No crean bases desde DBine. |
| Aurora DSQL | crear bases | No crea bases desde DBine. |
| Arrow Flight SQL | crear bases | No crea bases desde DBine. |
| Elasticsearch, Solr | crear bases | No crean bases desde DBine. |
| DynamoDB, ksqlDB, etcd, Redis | crear bases | No crean bases desde DBine. |
| SQLite, libSQL | crear bases | No crean bases desde DBine. |
| H2 | crear bases | Crea la base al conectarse a un nombre nuevo (con `-ifNotExists`). |
| CrateDB | crear bases | Una sola base por clúster: se organiza en esquemas. |
| Denodo | crear bases | Las bases se crean desde Denodo. |
| Manticore | crear bases | No tiene bases de datos. |
| Db2, Informix, Teradata | crear bases | No crean bases desde DBine. Ver pendientes. |

**Pendientes explícitos:**

- **Db2, Informix y Teradata:** hay que habilitar primero la creación de bases
  en sus drivers; recién después tiene sentido ofrecerles opciones.
- **DuckDB:** el contrato (`create_database_script`) tendría que poder
  mostrar la ruta del archivo que se va a crear.
- **Spanner con dialecto PostgreSQL:** el driver habla GoogleSQL, así que no
  podría usar la base que crea. Hace falta soporte del dialecto en el driver.
- **Esquemas de Trino:** sus opciones necesitan un contrato de opciones de
  esquema, que todavía no existe.
- **IoTDB 2:** al borrar una base quedan reglas de TTL huérfanas. Es un
  error conocido, sin resolver.

## Backups

La pestaña **Backups** ([`backups.md`](backups.md)).

Las **copias de DBine** funcionan en todos los motores. Son un script con la estructura y los datos en un archivo local, y se restauran ejecutándolo.

Los **backups del servidor** son los del propio motor. Los tienen los motores de la tabla; los que no están en ella tienen solo las copias de DBine.

| Motor | Backup | Restaurar | Borrar | Historial | Probado contra servidor |
|---|---|---|---|---|---|
| SQL Server (y Managed Instance) | `BACKUP DATABASE/LOG`: completo, diferencial o de log, a disco o a una URL | Sí, con otro nombre (mueve los archivos) o encima, en modo de un solo usuario | No | `msdb` | Sí (2022) |
| SAP HANA | `BACKUP DATA`: completo, diferencial o incremental; archivo o Backint | Sí, desde SYSTEMDB (`RECOVER DATA … CLEAR LOG`) | Sí (`BACKUP CATALOG DELETE`) | `M_BACKUP_CATALOG` | No |
| Oracle, Oracle Autonomous | Data Pump (`DBMS_DATAPUMP`) de un esquema | Sí, con cambio de esquema | Sí (`UTL_FILE.FREMOVE`) | Trabajos de Data Pump; en Autonomous, también los `.dmp` | Sí (23 Free) |
| CockroachDB | `BACKUP … INTO`: completo o incremental, de la base o del clúster | Sí, con otro nombre | No | Trabajos de backup | Sí |
| CrateDB | `CREATE SNAPSHOT` | Sí | Sí | `sys.snapshots` | Sí |
| H2 | `SCRIPT TO` / `BACKUP TO` | Sí, del formato `SCRIPT` (`RUNSCRIPT`) | No | No | Sí |
| MySQL | `CLONE LOCAL DATA DIRECTORY` | No | No | El último clon | Sí (8.4) |
| TiDB | `BACKUP DATABASE` | Sí, con el mismo nombre | No | `SHOW BACKUPS` | Sí |
| SingleStore | `BACKUP DATABASE`: completo o diferencial; local, S3, GCS o Azure | Sí, con otro nombre | No | `MV_BACKUP_HISTORY` | No |
| OceanBase | `ALTER SYSTEM BACKUP` (del tenant) | No | No | Trabajos de backup | No |
| StarRocks, Apache Doris | `BACKUP SNAPSHOT` a un repositorio | Sí | No | `SHOW SNAPSHOT` | StarRocks sí; Doris no |
| Manticore | `BACKUP TABLE` | No | No | No | Sí |
| GreptimeDB | `COPY DATABASE TO` | Sí, en tablas que ya existen | No | No | Sí |
| ClickHouse | `BACKUP DATABASE` a un disco, una ruta o S3; incremental | Sí, con otro nombre | No | `system.backups` | Sí |
| DuckDB | `EXPORT DATABASE` (Parquet o CSV) | Sí (`IMPORT DATABASE`) | No | No | Sí |
| SQLite | `VACUUM INTO` | No | No | No | Sí |
| Snowflake | Backup sets o clon instantáneo (`CLONE`) | Sí, en una base nueva o reemplazando la actual (`SWAP`) | Sí (en los backup sets, solo el más viejo) | Backup sets y clones | No |
| Databricks | `DEEP CLONE` de cada tabla Delta de un esquema, en un bloque de SQL scripting | Sí | Sí | Esquemas de backup | No |
| BigQuery | Snapshots de todas las tablas del dataset (`CREATE SNAPSHOT TABLE`) | Sí (`CLONE`) | Sí | `TABLE_SNAPSHOTS` | No |
| Cloud Spanner | `CREATE BACKUP` (sentencia de DBine; usa la API de administración) | Sí, en una base nueva | Sí | Lista de backups | No (el emulador no tiene backups) |
| DynamoDB | `CREATE BACKUP` (sentencia de DBine; usa la API de DynamoDB) | Sí, en una tabla nueva | Sí | Lista de backups | No (DynamoDB Local no tiene backups) |
| Amazon Keyspaces | Activa la recuperación a un punto en el tiempo de las tablas | Sí, en una tabla nueva | Sí (la desactiva) | Tablas con recuperación activa | No |
| Memgraph | `CREATE SNAPSHOT` | Sí (`RECOVER SNAPSHOT`; reemplaza todo) | No | `SHOW SNAPSHOTS` | Sí |
| Redis, Valkey, Dragonfly | `BGSAVE` / `SAVE` | No | No | El último guardado | Sí |
| etcd | `snapshot save`: el snapshot queda en esta máquina | No | No | No | Sí |
| Elasticsearch, OpenSearch | Snapshots | Sí, con otro nombre | Sí | Snapshots de cada repositorio | Sí (Open Distro no) |
| Solr | Colecciones (SolrCloud) o núcleos (standalone) | Sí | Sí | En standalone, solo el último | Sí |
| Db2 LUW (ODBC) | `ADMIN_CMD('BACKUP DATABASE …')` | No | No | `DB_HISTORY` | No |
| Sybase ASE (ODBC) | `DUMP DATABASE` | Sí (`LOAD` + `ONLINE`) | No | No | No |
| SQL Anywhere, MonetDB, Virtuoso, Machbase (ODBC) | El comando de backup de cada motor | No | No | No | No |
| Informix, GBase 8s (ODBC) | `task('ontape archive' / 'onbar')` | No | No | ON-Bar y archivos de nivel 0 | No |
| Db2 for i (ODBC) | `SAVLIB` a un archivo de salvado | Sí (`RSTLIB`) | Sí | `SAVE_FILE_INFO` | No |
| Vertica (ODBC) | `SAVE RESTORE POINT` (Eon 24.1+) | No | Sí | Puntos de restauración | No |
| Mimer, Dameng (ODBC) | El comando de backup de cada motor | No | No | Sí | No |

**Qué falta y por qué:**

| Motor | Qué falta | Motivo |
|---|---|---|
| PostgreSQL, TimescaleDB, EDB, Fujitsu, KingbaseES, openGauss | Backups del servidor | Se hacen con herramientas de cliente (`pg_dump`, `pg_basebackup`, `gs_basebackup`), no con SQL. |
| Redshift, Aurora PostgreSQL, Aurora MySQL, Aurora DSQL | Backups del servidor | Los snapshots se manejan desde la API o la consola de AWS. |
| Cloud SQL, AlloyDB | Backups del servidor | Se manejan desde la API o la consola de Google Cloud. |
| Azure SQL Database | Backups del servidor | El servicio hace sus propios backups; la restauración a un punto en el tiempo se hace desde el portal o la API. |
| Microsoft Fabric, Babelfish | Backups del servidor | No tienen `BACKUP` ni `RESTORE`. |
| YugabyteDB, Greenplum, Cloudberry, Greengage, Yellowbrick, RisingWave | Backups del servidor | Se hacen con herramientas propias (`yb-admin`, `gpbackup`, `ybbackup`, `risectl`). |
| Materialize | Backups del servidor | No guarda datos propios: su estado se reconstruye desde las fuentes. |
| Denodo | Backups del servidor | Exporta sus metadatos con una herramienta propia; no hay backup del servidor por SQL. |
| MariaDB, Databend | Backups del servidor | Se hacen con herramientas (`mariadb-backup`, `bendsave`). `BACKUP STAGE` de MariaDB solo bloquea el servidor para esas herramientas. |
| Cloud SQL for MySQL, VeloDB | Backups del servidor | Son del proveedor (consola o API). |
| MySQL, Manticore, OceanBase | Restaurar | MySQL: se reinicia el servidor apuntando `--datadir` a la copia. Manticore: `manticore-backup --restore` con el servidor detenido. OceanBase: restaurar crea un tenant nuevo desde el tenant sys. |
| TiDB | Restaurar con otro nombre | TiDB restaura con el nombre original y las tablas no pueden existir. |
| MySQL, TiDB, SingleStore, OceanBase, StarRocks, Doris, Manticore, GreptimeDB, ClickHouse, CockroachDB | Borrar un backup | No hay una sentencia para eso: se borra la carpeta o el objeto en el almacenamiento. |
| SQL Server | Borrar un backup | SQL no puede borrar un `.bak` (`xp_delete_file` no está documentado), y `sp_delete_database_backuphistory` borra todo el historial de la base. |
| SQL Server | Restaurar backups divididos en varios archivos | Se ven en el historial, pero no se restauran desde DBine. |
| Timeplus Proton | Backups del servidor | `BACKUP` escribe metadatos vacíos y `RESTORE` falla (probado en 3.0.31). |
| libSQL / Turso | Backups del servidor | El servidor rechaza `VACUUM INTO`; la restauración a un punto en el tiempo es de la API de la plataforma. |
| SQLite, Redis, etcd | Restaurar | No hay SQL ni comando para eso: se reemplaza el archivo con la base o el servidor detenidos. |
| Firebird | Backups del servidor | `gbak` y `nbackup` solo funcionan por la Services API, que el cliente del driver no implementa. |
| MongoDB, FerretDB, Amazon DocumentDB | Backups del servidor | No hay un comando del servidor: `mongodump` es una herramienta de cliente y los backups de Atlas y DocumentDB son del proveedor. |
| Neo4j | Backups del servidor | `neo4j-admin database backup` (Enterprise), fuera de Cypher. |
| Amazon Neptune | Backups del servidor | Se manejan desde la API de AWS. |
| Cassandra, ScyllaDB | Backups del servidor | Los snapshots se hacen con `nodetool` (JMX) o la API REST de ScyllaDB, no con CQL. |
| Memgraph | Borrar un snapshot | No hay un comando: los viejos se descartan según la retención configurada. |
| InfluxDB 1, 2, 3 | Backups del servidor | v1: `influxd backup` usa un puerto RPC aparte. v2: la API de backup son varias transferencias HTTP que arma el cliente `influx`, y Cloud no tiene API. v3: los datos son archivos Parquet en el almacenamiento de objetos. |
| CouchDB | Backups del servidor | No tiene API de backup. `_replicate` pide URLs con credenciales dentro del script, y DBine no pone contraseñas en scripts. |
| Couchbase | Backups del servidor | El servicio de backups es solo de la edición Enterprise, en su propio puerto; Community solo tiene `cbbackupmgr`. |
| TDengine | Backups del servidor | La edición abierta usa `taosdump`; la Enterprise, taosX. Ninguno es SQL. |
| IoTDB | Backups del servidor | No tiene una sentencia de backup: se copian los directorios de datos o se usan herramientas externas. |
| Dremio | Backups del servidor | Se hacen con `dremio-admin backup`, una herramienta de línea de comandos del servidor. |
| Azure Cosmos DB | Backups del servidor | Los backups y la restauración a un punto en el tiempo son del plano de control de Azure, al que no se llega con la clave de la cuenta. |
| OrientDB | Backups del servidor | `BACKUP` y `EXPORT DATABASE` son comandos de la consola; el SQL los rechaza. |
| Athena | Backups del servidor | Consulta datos en S3; protegerlos es tarea de S3 (versionado, replicación) o AWS Backup. |
| Trino, Presto, Starburst, Drill, Flight SQL | Backups del servidor | Son motores de consulta sin datos propios: los backups son del almacenamiento que está detrás. |
| Phoenix | Backups del servidor | Son snapshots de HBase (shell o API de administración), fuera del alcance de SQL. |
| ksqlDB | Backups del servidor | Su estado vive en tópicos de Kafka. |
| Db2 LUW, SQL Anywhere, Informix, Vertica, MonetDB, Virtuoso, Dameng, Machbase, Mimer (ODBC) | Restaurar | La restauración se hace con herramientas o con el servidor detenido (Db2: `RESTORE` es un comando CLP que `ADMIN_CMD` no ejecuta). |
| Db2 for z/OS, Teradata, Netezza, Exasol, IRIS, OpenEdge, Ingres, CUBRID, Zen, MaxDB, NuoDB, Ignite, Ocient, SQream, HeavyDB, Altibase (ODBC) | Backups del servidor | Se hacen con herramientas propias o fuera de SQL. En z/OS, `DSNUTILU` devuelve el resultado en un parámetro de salida que el ejecutor de scripts no enlaza. |
| Hive, Impala, Spark, Kyuubi (ODBC) | Backups del servidor | Los datos viven en HDFS o S3. Hive solo tiene `EXPORT TABLE`, de a una tabla. |
| Access, dBase, NetSuite, ODBC genérico | Backups del servidor | Access y dBase: se copia el archivo. NetSuite: es de solo lectura. ODBC genérico: no se sabe qué motor hay detrás. |

**Sin probar contra un servidor, falta confirmar:**
- **Databricks:** que la API de ejecución de sentencias acepte un bloque `BEGIN … END`.
- **ODBC:** la sintaxis de backup de los presets de la tabla, escrita según la documentación del fabricante.

### Progreso y tiempo restante

Un backup o una restauración del servidor corre como tarea en segundo plano (panel **Tareas**), con el tiempo transcurrido en todos los motores. El script se ejecuta como una sola sentencia, así que el avance sale de las vistas del propio motor: DBine las lee cada 2 s desde una segunda sesión mientras el script corre. Con un total conocido, la tarea muestra además un **estimado del tiempo restante**, calculado con el ritmo del último minuto (aparece a partir de los 5 s; si el avance se detiene, el estimado crece). Si la consulta de avance falla (por ejemplo, sin permiso sobre la vista), se deja de consultar y la tarea sigue sin porcentaje.

| Motor | Qué informa | De dónde sale | Cómo se reconoce la operación |
|---|---|---|---|
| SQL Server (y Managed Instance) | Backup y restauración, en %; la comprobación posterior al backup (`RESTORE VERIFYONLY`) aparece como una etapa propia, con su estimado | `percent_complete` de `sys.dm_exec_requests` | Por el `@@SPID` de la sesión que corre el script. Las lecturas rápidas del archivo antes de restaurar (`FILELISTONLY`, `HEADERONLY`) no se cuentan. |
| Oracle, Oracle Autonomous | Exportación e importación de Data Pump, en % | `percent_done` del trabajo (lo que muestra `expdp ATTACH=`), con `DBMS_DATAPUMP.ATTACH` y `DETACH` enseguida | El trabajo en ejecución del usuario, de exportación o importación. El porcentaje avanza al terminar cada objeto: un esquema que es una sola tabla grande salta de 0 a 99. `v$session_longops` no recibe filas de Data Pump en 23ai Free. En Autonomous Database se usa la misma consulta; sin probar contra un servidor. |
| CockroachDB | Backup y restauración, en % | `fraction_completed` de `SHOW JOBS` | El trabajo `BACKUP` o `RESTORE` del usuario creado desde que empezó el script. |
| MySQL | Backup (`CLONE LOCAL`), en bytes copiados sobre el estimado | `performance_schema.clone_progress` | El clon en curso que empezó después del script. |
| TiDB | Backup y restauración, en % | `SHOW BACKUPS` / `SHOW RESTORES` | Por el `CONNECTION_ID()` de la sesión que corre el script. |
| SAP HANA | Backup (`BACKUP DATA`), en bytes transferidos sobre el total | `M_BACKUP_PROGRESS` | El backup en curso más reciente: si corren dos a la vez, se muestra el último. Sin probar contra un servidor. |

**Sin progreso, y por qué:**

| Motor | Qué falta | Motivo |
|---|---|---|
| SAP HANA | Progreso de la restauración | `RECOVER DATA` corre desde SYSTEMDB con la base detenida; `M_BACKUP_PROGRESS` solo informa backups. |
| OceanBase, StarRocks, Apache Doris, Redis, Valkey, Dragonfly | Progreso del backup | La sentencia (`ALTER SYSTEM BACKUP`, `BACKUP SNAPSHOT`, `BGSAVE`) vuelve enseguida y el backup sigue en el servidor: su estado se ve en el historial. |
| CrateDB, H2, Manticore, GreptimeDB, DuckDB, SQLite, Memgraph, etcd, Solr, Snowflake, Databricks, BigQuery, Cloud Spanner, DynamoDB, Amazon Keyspaces, presets ODBC | Progreso del backup y de la restauración | El motor no publica el avance de la operación mientras corre: la tarea muestra solo el tiempo transcurrido. |
| SingleStore, ClickHouse, Elasticsearch, OpenSearch | Progreso del backup y de la restauración | Pendiente explícito: `MV_BACKUP_STATUS` (SingleStore), `system.backups` (ClickHouse) y `_snapshot/_status` (Elasticsearch, OpenSearch) podrían dar el avance; falta verificar contra un servidor qué informan durante la operación. |

## Clonar tabla

«Clonar…» (menú contextual del explorador) copia una tabla al lado de la
original, con otro nombre, su estructura y sus datos. El clon es exacto o no
se hace: lo que no se puede copiar igual se rechaza antes de escribir nada,
con el motivo, y el explorador no ofrece la opción donde no aplica.

| Motor | Qué | Motivo |
|---|---|---|
| Neo4j, Memgraph, Neptune | No se clona | Los nodos de una etiqueta no se copian sin sus relaciones. |
| Redis, Valkey, Dragonfly, etcd | No se clona | Un motor clave-valor no tiene tablas: cada clave es un valor suelto. |
| ksqlDB | No se clona | Los datos viven en los topics de Kafka: clonar el stream o la tabla no copia sus mensajes de forma fiel. |
| InfluxDB 2 | No se clona | Flux no puede borrar un measurement (el borrado es otra API, `/api/v2/delete`): si la copia fallara a medias, el clon no se podría deshacer. Pendiente: una forma de borrar un measurement desde el driver. |
| InfluxDB 3 | No se clona | Su SQL es de solo lectura y no borra tablas (el borrado es otra API, `/api/v3/configure/table`): mismo motivo que InfluxDB 2. Pendiente, igual que InfluxDB 2. |
| TDengine | Supertablas y subtablas no se clonan | Las filas de una supertabla viven en subtablas con nombre propio (`tbname`) y tags: el clon necesitaría subtablas con otros nombres. Una subtabla clonada sería otra subtabla de la misma supertabla y sus filas aparecerían dos veces en las consultas sobre ella. |
| IoTDB | Series con alias, tags o atributos, y series que son vistas | DBine todavía no los lee en la estructura del dispositivo; el clon se rechaza con la lista de series. Pendiente: leerlos y recrearlos. |
| Cassandra, ScyllaDB, Keyspaces | Tablas con columnas `counter` | Un counter solo cambia sumando (`UPDATE … SET c = c + n`), no con `INSERT`, y un reintento de la suma duplicaría el valor. |
| CouchDB | No se clona | Sus documentos (`_all_docs`) son la base entera, no una tabla dentro de ella: el clon sería otra base. Para copiarla, crear otra base y usar «Migrar…». |

**Particularidades:**
- **TDengine:** el clon de una tabla normal se crea con su propio `SHOW CREATE TABLE`
  (clave compuesta, codificación, compresión y nivel de cada columna, `TTL`,
  comentario) y se compara con él antes de copiar las filas.
- **IoTDB:** el nombre del dispositivo nuevo es un solo nivel (letras sin
  tildes, números y `_`); si ya hay series de otro dispositivo bajo esa ruta
  (por ejemplo `px.inner` al clonar como `px`), se rechaza, porque borrar el
  clon (`DELETE TIMESERIES px.**`) se llevaría también las del otro.
- **InfluxDB 1:** el measurement se crea con sus primeros puntos; los tags siguen
  siendo tags y los campos conservan su tipo (se compara después de la copia).
  Se copia la política de retención predeterminada; si el measurement tiene
  puntos en otra, se rechaza. Sin copiar los datos no hay clon (un measurement
  sin puntos no existe).
- **Cassandra, ScyllaDB, Keyspaces:** el nombre admite solo letras sin tildes,
  números y `_`. El TTL y la hora de escritura (`writetime`) de cada fila no se
  pueden copiar: en el clon las filas vencen según el TTL por defecto de la
  tabla (o no vencen) y su hora de escritura es la de la copia. El resultado del
  clonado lo avisa.
- **Motores de documentos y de búsqueda** (MongoDB, FerretDB, DocumentDB,
  Couchbase, Cosmos DB, Elasticsearch, OpenSearch, Solr): se copian los
  documentos enteros, con todos sus campos (no solo los que vio la muestra de
  la estructura) y su metadata (`_id` y routing en Elasticsearch/OpenSearch).
- **MongoDB, FerretDB, DocumentDB:** el clon se arma con un nombre propio
  (`<nombre>__dbine_tmp_<hash>`) y al final se renombra: si otro creó la
  colección con ese nombre mientras tanto, el clon se descarta y la del otro
  queda intacta. Las series temporales no se pueden renombrar y se arman con su
  nombre final. Los índices con intercalación simple en una colección con
  intercalación por defecto se recrean con `{locale: "simple"}` (incluidos los
  de texto).
- **Elasticsearch, OpenSearch:** se copian el mapping completo
  (`dynamic_templates`, `_routing`, `runtime`, `_meta`…) y todos los settings
  del índice salvo los que el servidor lleva por su cuenta (uuid, fecha de
  creación, versión). Los pipelines de ingesta (`default_pipeline`,
  `final_pipeline`) y los bloqueos de escritura (`index.blocks.*`) se aplican
  después de copiar los documentos, que ya pasaron por el pipeline una vez. El
  clon no se suma a los alias del original ni a su política de ciclo de vida
  (ILM/ISM); el resultado del clonado lo avisa.

## Acciones según los permisos del usuario

Antes de ofrecer una acción que necesita privilegios, DBine le pregunta al
servidor qué puede hacer el usuario conectado. Si le falta el permiso, el botón
o la opción del menú aparece deshabilitado, con un tooltip que nombra lo que
falta. Se consulta una vez por conexión y base, y otra vez al reconectar.

Las acciones que se chequean son: hacer un backup nativo, restaurarlo, el
profiler, terminar sesiones desde el Monitor, crear y borrar bases,
administrar usuarios y permisos, y crear esquemas (qué motores lo chequean está
en [Nuevo esquema y borrar esquema](#nuevo-esquema-y-borrar-esquema)).

La regla es no deshabilitar nunca una acción que el usuario sí podría hacer.
Cuando el motor no permite saberlo con certeza (permisos que llegan por roles
que no se pueden resolver, permisos por recurso, sistemas de IAM), la acción
queda habilitada y responde el servidor. El chequeo mira los permisos del
servidor; el modo de solo lectura de DBine se aplica aparte (ClickHouse y los
motores de documentos y búsqueda también lo informan como permiso faltante).

### Qué se chequea en cada motor

| Motor | Se chequea | Cómo |
|---|---|---|
| SQL Server | Todas | `HAS_PERMS_BY_NAME` e `IS_SRVROLEMEMBER` en una sola consulta. |
| Azure SQL Database | Backup, profiler, terminar sesiones, usuarios | `VIEW DATABASE STATE`, `KILL DATABASE CONNECTION`, `ALTER ANY USER`. |
| Oracle | Todas | `SESSION_PRIVS`, `SESSION_ROLES` y los permisos sobre las vistas V$. |
| SAP HANA | Todas | `EFFECTIVE_PRIVILEGES`, que incluye lo que llega por roles. |
| Snowflake | Solo con los roles de sistema (ACCOUNTADMIN, SYSADMIN, USERADMIN); borrar base según el dueño | `IS_ROLE_IN_SESSION`. |
| PostgreSQL y derivados (TimescaleDB, YugabyteDB, AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu, KingbaseES, Greenplum, Cloudberry, Greengage) | Profiler, terminar sesiones, crear y borrar bases, usuarios | Atributos del rol y pertenencia a `pg_read_all_stats` y `pg_signal_backend`. |
| openGauss | Profiler, terminar sesiones, crear y borrar bases, usuarios | SYSADMIN, MONADMIN y los atributos del rol. |
| CockroachDB | Todas | Rol `admin`, opciones del rol, privilegios de sistema y grants de la base. |
| Materialize | Crear bases, usuarios | `has_system_privilege`. |
| H2 | Backup, restore, profiler, terminar sesiones, usuarios | Derechos de administrador. |
| MySQL, MariaDB, TiDB (y Aurora MySQL, Cloud SQL) | Profiler, terminar sesiones, crear y borrar bases, usuarios; backup en MySQL y TiDB, restore en TiDB | `SHOW GRANTS`, que incluye los roles activos. |
| StarRocks | Solo con los roles de sistema | `CURRENT_ROLE()`. |
| ClickHouse | Todas las que ofrece | `CHECK GRANT`, que tiene en cuenta roles y el modo `readonly`. |
| Firebird | Profiler, crear y borrar bases, usuarios | SYSDBA, rol RDB$ADMIN, dueño de la base y privilegios de sistema (Firebird 4+). |
| SQLite | Backup | Se deshabilita con `PRAGMA query_only`. |
| DuckDB | Restore, crear bases | Se deshabilitan si la base se abrió en solo lectura. |
| BigQuery, Cloud Spanner | Backup, restore, crear y borrar bases, usuarios; profiler solo para habilitar | `testIamPermissions`. |
| Aurora DSQL | Usuarios | Atributos del rol. |
| Athena | Profiler | Si AWS niega `athena:ListQueryExecutions`. |
| Trino, Presto, Starburst | Profiler | Si el servidor rechaza la lista de consultas. |
| Dremio | Crear y borrar espacios, profiler, usuarios | En OSS todos son administradores; en Enterprise, `sys.privileges` y los roles. |
| Apache Drill | Profiler | Administrador de Drill cuando la autenticación está activa. |
| etcd | Backup, usuarios | Rol `root` cuando la autenticación está activa. |
| Db2 LUW, SAP ASE (ODBC) | Backup, terminar sesiones, usuarios; en ASE también restore y crear y borrar bases | Autoridades del usuario (Db2) y roles del login (ASE). |
| Redis, Valkey, Dragonfly | Backup, profiler, usuarios | `ACL DRYRUN`, sin ejecutar los comandos. |
| Cassandra, ScyllaDB | Profiler, crear y borrar keyspaces, usuarios | Superusuario y `LIST ALL PERMISSIONS`, con los roles heredados. |
| Neo4j | Profiler, terminar sesiones, crear y borrar bases, usuarios | `SHOW USER PRIVILEGES` en Enterprise; Community no tiene roles. |
| Memgraph | Backup, restore, profiler, crear y borrar bases, usuarios | Privilegios del usuario en Enterprise. |
| InfluxDB | 1.x: profiler, crear y borrar bases, usuarios; 2.x: crear y borrar buckets; 3.x: profiler, crear y borrar bases | Usuario administrador (1.x), autorizaciones (2.x), token de administrador (3.x). |
| IoTDB | Profiler, crear y borrar bases, usuarios | `LIST PRIVILEGES OF USER`, con los roles. |
| TDengine | Solo para habilitar | El superusuario habilita todo; nunca se deshabilita (ver abajo). |
| MongoDB, FerretDB, Amazon DocumentDB | Profiler, terminar sesiones, crear y borrar bases, usuarios | `connectionStatus` con `showPrivileges`, que incluye los roles. |
| Elasticsearch | Backup, restore, profiler, usuarios | `_security/user/_has_privileges`. |
| OpenSearch | Profiler, usuarios; todo con el rol `all_access` | `authinfo` y lecturas de prueba de `_tasks` y de los usuarios internos. |
| Solr | Backup, restore, usuarios | Roles del usuario y reglas de autorización. |
| CouchDB | Crear y borrar bases; usuarios solo para habilitar | Rol `_admin` y administradores de la base. |
| Couchbase | Profiler, crear y borrar buckets, usuarios | `checkPermissions`, que incluye todos los roles. |
| OrientDB | Crear y borrar bases, usuarios; solo para habilitar | Usuario de servidor o rol con todos los permisos. |

Flight SQL, ksqlDB, Phoenix y libSQL no tienen ninguna de estas acciones.

### Qué no se puede chequear

| Motor | Qué queda habilitado sin chequeo | Motivo |
|---|---|---|
| Babelfish, Microsoft Fabric | Todo | `HAS_PERMS_BY_NAME` no es confiable en esas variantes. |
| Azure SQL Database | Crear y borrar bases | Se decide en `master` (rol dbmanager), que una conexión a otra base no ve. |
| Snowflake | Lo que llega por roles propios | Leer los privilegios de cuenta de roles propios requiere recorrer toda la jerarquía con `SHOW GRANTS` o `ACCOUNT_USAGE`, que tiene horas de atraso. |
| Oracle | Permiso sobre el DIRECTORY de Data Pump | El directorio se elige en el formulario del backup. |
| Redshift, RisingWave | Usuarios, profiler, terminar sesiones y borrar bases para quien no es superusuario | No se leen los privilegios de sistema de RBAC. |
| CrateDB | Lo que llega por roles | No se sigue el privilegio AL otorgado a un rol. |
| Materialize | Borrar bases y profiler para quien no es superusuario | No se lee el dueño de la base ni la pertenencia a `mz_monitor`. |
| CockroachDB | Profiler y terminar sesiones sin el privilegio de sistema | Un usuario que no es admin no puede leer sus propias opciones de rol. |
| OceanBase, SingleStore, Doris, VeloDB | Todo | Su modelo de permisos no se pudo verificar: no hay servidor de prueba. |
| Databend, GreptimeDB, Manticore | Todo | Modelo de permisos distinto (Manticore no tiene usuarios). |
| StarRocks | Lo que llega por roles propios | Solo se leen los roles de sistema. |
| Firebird | Borrar una base distinta de la conectada; crear bases en Firebird 4+ cuando no se ve el privilegio | Un privilegio de sistema otorgado por un rol de la base de seguridad no se ve desde la base conectada. |
| ClickHouse anterior a 24.5, Timeplus | Todo | No tienen `CHECK GRANT`. |
| SQLite, DuckDB | Carpeta de destino del backup | Se elige al momento del backup. |
| libSQL | Todo | Un token de solo lectura de Turso no se puede detectar sin escribir. |
| Athena | Crear y borrar bases | IAM no tiene un chequeo barato para quien llama, y Glue no tiene simulación. |
| Databricks | Todo salvo habilitar el profiler a administradores | Unity Catalog no le dice al usuario sus propios privilegios, y consultarlos por SQL despertaría el warehouse. |
| BigQuery | Borrar datasets, restore y usuarios cuando se otorgan por dataset | El chequeo del proyecto no ve los permisos por dataset. |
| Cloud Spanner | Profiler con control de acceso fino | El rol `spanner_sys_reader` no se ve desde IAM. |
| Trino, Presto, Starburst | Usuarios | Dependen del control de acceso del conector. El profiler se ve habilitado aunque el usuario solo vea sus consultas. |
| Dremio | Usuarios en OSS; borrar un espacio en Enterprise sin grant de dueño | OSS no tiene usuarios; el listado de grants puede no mostrar al dueño. |
| Otros motores por ODBC | Todo | Cada motor tiene su propio modelo de permisos; ODBC genérico no sabe qué motor hay detrás. |
| Redis anterior a 7, KeyDB, usuarios sin `ACL DRYRUN` | Todo | Sin `ACL DRYRUN` no hay forma de preguntar; ese comando es de administración. |
| Amazon Keyspaces, Amazon Neptune | Todo | Decide IAM. |
| Neo4j Community, Memgraph Community | Crear y borrar bases | Esas ediciones tienen una sola base. |
| InfluxDB 2 | Casi todo, salvo con el token de operador | El servidor oculta los tokens y no se puede saber cuál es el propio. |
| Azure Cosmos DB | Todo | Una clave de solo lectura no se distingue de una de lectura y escritura sin escribir, y los roles de Entra ID están en el plano de control. |
| DynamoDB | Backup y restore | No hay simulación de CreateBackup ni RestoreTableFromBackup, y simular una política de IAM requiere permisos de IAM propios. |
| OrientDB | Nunca se deshabilita | Un rechazo en `/server` no distingue un usuario de base de uno de servidor sin `server.info`. |
| Solr | Todo, si el usuario no puede leer las reglas (`security-read`) | Las peticiones que no coinciden con ninguna regla están permitidas: sin ver las reglas, que falte un permiso no prueba nada. |
| OpenSearch | Snapshots sin `all_access` | No tiene una API para consultar privilegios propios. |
| CouchDB | Usuarios para quien no es administrador | Un usuario común igual puede registrar usuarios. |
| MongoDB por API compatible (Cosmos DB for MongoDB) | Todo | No responden `connectionStatus` con privilegios. |
| TDengine | Todo para quien no es superusuario | En la prueba, el servidor dejó crear y borrar bases y usuarios a un usuario sin SUPER ni CREATEDB: esos flags no prueban un rechazo. |

### Probado contra servidores reales

Con un usuario administrador y uno limitado: SQL Server, Oracle,
PostgreSQL, CockroachDB, H2, openGauss, Materialize, MySQL, MariaDB, TiDB,
StarRocks, ClickHouse, Firebird, Trino, Drill, Dremio OSS, etcd, Redis,
Valkey, Dragonfly, Cassandra, ScyllaDB, Neo4j (Enterprise y Community),
Memgraph Community, InfluxDB 1, 2 y 3 Core, IoTDB, TDengine, MongoDB,
FerretDB, Elasticsearch, OpenSearch, Solr, CouchDB, Couchbase, OrientDB, el
emulador de Cosmos DB y DynamoDB Local. SQLite y DuckDB
se probaron con archivos reales, incluidos archivos de solo lectura.

Implementados según la documentación, sin servidor de prueba: SAP HANA,
Snowflake, BigQuery, Cloud Spanner, Aurora DSQL, Databricks, Athena, Dremio
Enterprise, Db2 LUW, SAP ASE, Redshift, RisingWave, CrateDB, YugabyteDB y los
demás derivados de PostgreSQL, Aurora MySQL, Cloud SQL, Memgraph Enterprise,
InfluxDB 3 Enterprise, Amazon DocumentDB y Open Distro.

## Ejecución de scripts

Cómo funciona: [`ejecucion-de-scripts.md`](ejecucion-de-scripts.md). Todos los
motores que ejecutan texto corren el script sentencia por sentencia, con
mensajes, errores con código y línea, y cancelación. Las excepciones son
estas.

Probado contra servidores reales: SQL Server, Babelfish, PostgreSQL,
TimescaleDB, YugabyteDB, CockroachDB, MySQL, MariaDB, TiDB, Oracle, SQLite,
DuckDB, Firebird, ClickHouse, Flight SQL (GizmoSQL), Drill, Phoenix, Trino,
Dremio, InfluxDB 1, MongoDB, Neo4j, Redis, Elasticsearch, ScyllaDB, Couchbase y el emulador de
Cloud Spanner. Spanner y Phoenix son emuladores o contenedores: sus códigos de
error pueden diferir del servicio real.

Implementados según la documentación y probados solo con tests unitarios y de
corte, sin servidor: SAP HANA, ODBC (presets), libSQL, Aurora DSQL, Presto,
Snowflake, BigQuery, Databricks, Athena, InfluxDB 2 y 3, etcd,
Cosmos DB, CouchDB, OrientDB, ksqlDB y Manticore (su contenedor de prueba no
arrancó por un puerto ocupado en la máquina de pruebas; solo tests unitarios).

### Modo de envío

| Motor | Modo | Motivo |
|---|---|---|
| Snowflake, BigQuery, InfluxDB 2 (Flux) | Todo el texto de una vez; "Seguir si hay un error" no tiene efecto | El servidor ejecuta el script como un solo pedido. Los bloques de Snowflake Scripting y de BigQuery se cortan bien. |
| Firebird | Todo el texto de una vez | Acepta `SET TERM` y bloques sin terminador. Los comandos propios de `isql` se saltean con un aviso. |
| ODBC distinto de Teradata (ODBC genérico, cuerpos SPL de Informix y GBase, scripts de Exasol terminados en `/`, Netezza, NuoDB, IRIS…) | Se corta por `;` con el lexer genérico | Esos cuerpos necesitan el modo "Todo el texto de una vez" en la conexión. |
| Db2 | Un bloque `BEGIN [ATOMIC] … END` suelto se corta en su primer `;` | `CREATE TRIGGER` / `PROCEDURE … BEGIN … END` quedan enteros. Para bloques anónimos hay que usar `--#SET TERMINATOR`, que es lo que hace el cliente de Db2 sin terminador alternativo. |
| Athena, InfluxDB 1 | Una sentencia por pedido | Athena crea una ejecución por sentencia; InfluxDB 1.x pierde el motivo del error de una sentencia que comparte pedido. |

### Transacciones manuales

Ofrecen Auto/Manual, Confirmar y Deshacer: SQL Server, Babelfish, PostgreSQL y
derivados, CockroachDB, MySQL, MariaDB, TiDB, OceanBase (y Aurora y Cloud SQL
MySQL), Oracle, SQLite, DuckDB, Flight SQL, Phoenix, Trino, Spanner, Neo4j y
Couchbase.

| Motor | Qué falta | Motivo |
|---|---|---|
| StarRocks, Manticore, GreptimeDB y otras variantes analíticas de MySQL | Transacciones manuales | El motor no tiene transacciones de varias sentencias. |
| ClickHouse | Transacciones manuales | El motor no tiene transacciones de varias sentencias (solo una función experimental, desactivada por defecto). |
| Snowflake | Una transacción abierta al final del script se pierde | Hay una sesión de la API por ejecución. DBine avisa si el script termina con una transacción abierta. |
| Spanner | `DDL` dentro de una transacción | Spanner no lo admite; DBine lo rechaza con un mensaje. |
| Oracle | Estado "fallida" | Oracle no lo tiene: una sentencia fallida deshace solo ella y la transacción sigue abierta. En el editor el modo arranca en Auto porque la conexión tiene autocommit activo. |
| MySQL, TiDB (autocommit apagado) | Un `SELECT` deja la transacción "abierta" | Es lo que informa el servidor (`SERVER_STATUS_IN_TRANS`). |
| Trino | Tras un error la transacción queda "fallida" | El servidor la aborta y las sentencias siguientes dan `TRANSACTION_ALREADY_ABORTED` hasta deshacer. |
| SQL Server | Una transacción que no se puede confirmar se deshace al final del lote | Lo hace el servidor (error 3998). |

Los demás motores no ofrecen Auto/Manual; el selector no aparece.

### `USE` y base de la pestaña

La pestaña sigue el cambio de base en todos los motores que tienen el
concepto. Particularidades:

| Motor | Comportamiento | Motivo |
|---|---|---|
| Oracle | `ALTER SESSION SET CURRENT_SCHEMA` (también dentro de `EXECUTE IMMEDIATE`) hace de `USE` | Oracle cambia de esquema, no de base. |
| Databricks | `USE CATALOG` mueve la pestaña; `USE SCHEMA` solo cambia el contexto (`Contexto: cat.schema`) | La pestaña muestra el catálogo. |
| Athena | `USE` se comprueba contra las bases del catálogo; una desconocida da `SCHEMA_NOT_FOUND` | El motor no valida el `USE`. |
| InfluxDB 1 | `USE db[.rp]` puede terminar en su línea sin `;`; se comprueba con `SHOW DATABASES` / `SHOW RETENTION POLICIES` | Es la sintaxis de InfluxQL. |
| Redis | `SELECT n` cambia a `db{n}`; dentro de `MULTI` el cambio ocurre en `EXEC`, y no si el `EXEC` falla, hay `DISCARD` o `EXECABORT` | El servidor no cambia de base hasta que se ejecuta la transacción. |
| ODBC | Solo en los presets que tienen lista de bases | Hive, Teradata y similares no tienen el concepto. |

### Cancelar

Conservan la sesión: todos, salvo Oracle y Babelfish (ver
[Cancelar](ejecucion-de-scripts.md#cancelar)). En Elasticsearch, si una
petición ignora la cancelación de `_tasks` (por ejemplo, una espera de salud
del cluster), la cancelación espera a que esa petición termine; las siguientes
no corren.

### Qué falta

| Motor | Qué falta | Motivo |
|---|---|---|
| Todos los motores | Modo SQLCMD, variables (`&var`, `:var`, `$(var)`), tiempo máximo del editor, comandos de cliente más allá de `PROMPT` y `SHOW ERRORS` | Pendiente explícito: no están hechos. |
| PostgreSQL y derivados | `COPY … FROM stdin` | Pendiente explícito: el editor no envía datos por el canal de COPY (ver abajo). |
| PostgreSQL y derivados | Ejecutar la sentencia bajo el cursor corta con el lexer del dialecto, no con el corte de psql | Pendiente explícito: `split_for_ui` con `statements` usa el lexer del núcleo en lugar de `split_script` del driver. Los metacomandos con apóstrofos y las filas de `COPY` no se cortan al estilo psql en esa ejecución. |
| PostgreSQL y derivados | Un metacomando en medio de una sentencia sin cerrar corta la sentencia en esa línea | Pendiente explícito: psql conserva el buffer y sigue la sentencia después del metacomando. |
| PostgreSQL y derivados | `SHOW` dentro de una transacción abierta no informa el tipo de la columna; `RETURNING` no informa `rows_affected` (solo la etiqueta) | Pendiente explícito. |
| Oracle | La línea `select …;` seguida de `/` corre una vez | Diferencia deliberada: SQL*Plus la corre dos veces porque `/` reejecuta el buffer. |
| Oracle | Cancelar sin el privilegio `ALTER SYSTEM` | El cliente no tiene llamada de interrupción; sin el privilegio la cancelación solo se registra. |
| Oracle, motores con plugin | El corte de SQL*Plus en builds publicados | Llega cuando se republique el host del plugin; un host anterior contesta `Unsupported` y la app corta con el dialecto. |
| Oracle | Un backup "Consistente" tomado 1 a 5 segundos después de un commit puede perder esas filas | `FLASHBACK_TIME` usa un mapa de tiempo a SCN grueso; el arreglo es usar `FLASHBACK_SCN`. Pendiente explícito. |
| MySQL y familia | Los errores que no son de sintaxis (por ejemplo 1054) apuntan a la primera línea de la sentencia | El servidor no da la posición. |
| Presto | `TABLE_NOT_FOUND` se ubica al comienzo de la sentencia | Presto no informa línea ni columna. |
| Dremio | Los errores no llevan código | La API REST no lo devuelve. |
| Spanner (emulador) | Algunos errores llegan como `failed to marshal error message` (`INTERNAL`) y un error de clave duplicada aparece en la sentencia siguiente | Comportamiento del emulador; no se probó contra una instancia real. |
| Redis | El texto de los errores conserva prefijos del cliente (`ResponseError:`, `"WRONGTYPE":`) | Pendiente explícito. |
| Redis, Elasticsearch, MongoDB | Un error de sintaxis que impide cortar rechaza todo el script antes de correr nada | Pendiente explícito: no se corren las líneas anteriores al error. |
| Phoenix | Un `UPSERT` sin confirmar no se ve en un `COUNT` dentro de la transacción | Comportamiento de Phoenix con tablas no transaccionales. |
| Timeplus, Proton | Sin probar | No se levantó el contenedor `dbine-test-proton`. |
| Firebird | La suite de pruebas en vivo debe correr con `--test-threads=1` | Las pruebas comparten un mismo `test.fdb` y chocan en DDL concurrente; no es del driver. |

### Scripts de psql en PostgreSQL y compatibles

El editor corta el script como psql, sentencia por sentencia. Lo que psql
resuelve del lado del cliente no llega al servidor:

| Qué | Qué hace DBine | Motivo |
|---|---|---|
| `\echo`, `\qecho`, `\warn` | Muestra el texto en los mensajes. | — |
| `\restrict` / `\unrestrict` (pg_dump) | Se saltean sin aviso. | Solo protegen a psql. |
| Otros metacomandos (`\set`, `\pset`, `\i`, `\gexec`…) | Se ignoran con un aviso; el script sigue. | DBine ejecuta solo SQL; no hay variables ni archivos del cliente. |
| `\connect` / `\c` | Error que detiene el script. | La sesión no cambia de base: el resto correría en la base equivocada. Hay que abrir esa base en otra pestaña. |
| `COPY … FROM stdin` | Error en esa sentencia; sus filas, hasta la línea `\.`, no se envían y el script sigue con lo que viene después. | El editor no envía datos por el canal de COPY. Para cargarlos: Importar datos o `COPY … FROM 'archivo'` en el servidor. |

Un metacomando termina en el fin de su línea, aunque tenga comillas o `;`, y
las filas de COPY terminan en su `\.` aunque tengan apóstrofos o `;`, como en
psql. Una línea que empieza con `\` dentro de un texto entre comillas, un
cuerpo `$$ … $$` o un comentario es parte de ese texto.

## Uso de índices

Al expandir una tabla, el explorador marca las columnas de clave primaria
(llave) y las de clave foránea (eslabón, con la tabla y la columna a la que
apuntan) y agrega la carpeta **Índices**: cada índice con su tipo (PK, UNIQUE,
CLUSTERED, NC, COLUMNSTORE…) y qué parte de las lecturas de la tabla pasan por
él, o **sin uso** en rojo cuando se escribe pero nadie lo lee. Clic en un índice
o clic derecho en la tabla › **Índices…** abre la pestaña con el detalle
(columnas clave e INCLUDE, filtro, tamaño, seeks, scans, lookups, updates,
porcentaje de lecturas, escrituras por lectura y últimos accesos). **Comparar
esquemas** muestra el uso de cada índice en cada lado, leído en su propia
conexión. El contrato está en `crates/dbine-driver/src/index_usage.rs`
(`Driver::supports_index_usage` y `Session::index_usage`); los números
derivados se calculan ahí, en `IndexUsageReport::derive`.

- **Lecturas** = seeks + scans + lookups.
- **% lecturas** = las lecturas del índice sobre las de todos los índices de la
  tabla (vacío si la tabla no tuvo lecturas).
- **Salud de seeks** (`seek_scan_split`): solo donde el motor separa búsquedas
  puntuales de recorridos. Seeks sobre seeks + scans: verde desde 0,8, amarillo
  desde 0,5, rojo debajo (un columnstore nunca es rojo). Donde el motor tiene un
  solo contador («usado N veces»), va en seeks, el color es neutro y no se
  muestra el aviso de las tablas chicas.
- **Escrituras** (`writes_counted`): donde el motor no las cuenta por índice, la
  pestaña muestra un guion (desconocidas, no 0), no hay escrituras por lectura
  y ningún índice sale «sin uso»: uno sin lecturas queda en 0 %.
- **Sin uso** = ninguna lectura y alguna escritura, solo con contadores y
  escrituras contadas.
- **Desde cuándo** (`since`): la pestaña dice «Estadísticas desde …» con la
  fecha que da el motor, o «desde el último reinicio del servidor» si no la da.
- Sin contadores (`stats_available` false: el motor no los tiene o el usuario
  no los puede leer) se listan los índices igual, con un aviso que dice por qué.

En la tabla, «—» en salud y escrituras quiere decir que el motor no tiene
contadores de uso. «En vivo» es contra un servidor o emulador real
(contenedores `dbine-test-*` o archivos locales); el detalle de cada prueba
está en «Probado contra servidores reales», al final de esta sección.

| Motor | Índices | Contadores | Desde cuándo | Salud de seeks | Escrituras | Permisos | En vivo | Notas |
|---|---|---|---|---|---|---|---|---|
| SQL Server | `sys.indexes`, `sys.index_columns` | `sys.dm_db_index_usage_stats` (LEFT JOIN: los índices nunca usados quedan en cero): `user_seeks`, `user_scans`, `user_lookups`, `user_updates` y los últimos accesos | `sqlserver_start_time` | Sí | Sí | VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE desde 2022); sin eso, sin contadores y el aviso lo dice | Sí (2022) | Tamaño de `sys.dm_db_partition_stats`. Los heaps (`index_id` 0) no se listan: son la tabla, no un índice, y sus lecturas no cuentan en el porcentaje. |
| Azure SQL Database | Igual | Igual | `sqlserver_start_time` si la base deja leer `sys.dm_os_sys_info`; si no, «desde el último reinicio» | Sí | Sí | VIEW DATABASE STATE | No | Los contadores también se reinician en un failover. |
| Babelfish | `sys.indexes`, `sys.index_columns` | No tiene `sys.dm_db_index_usage_stats`: sin contadores, con un aviso | — | — | — | — | Sí | — |
| PostgreSQL (y AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB, Fujitsu, KingbaseES) | `pg_index` (la PK incluida, DESC de `indoption`, INCLUDE, filtro `indpred`); tipo = método de acceso (BTREE, GIN, BRIN…); claves foráneas de `pg_constraint` | `pg_stat_all_indexes.idx_scan` a seeks; `last_idx_scan` (16+) a la última lectura; escrituras = `n_tup_ins + n_tup_upd - n_tup_hot_upd` de la tabla, iguales para todos sus índices (toda inserción y toda actualización no HOT escribe en cada índice; PostgreSQL no las cuenta por índice). Un índice parcial no recibe las escrituras de la tabla (no se sabe cuántas filas cumplen su filtro): queda sin escrituras, nunca «sin uso», y el aviso lo dice | `stats_reset` de `pg_stat_database`; si nunca se reiniciaron, el aviso dice que corren desde que se creó la base | No | Sí (de la tabla) | Ninguno: las estadísticas son legibles por cualquier usuario; si se niegan, sin contadores y el aviso sugiere `pg_read_all_stats` | Sí (16) | Las tablas particionadas suman los contadores y el tamaño de sus particiones (`pg_partition_tree`, 12+). Los `seq_scan` de la tabla no son lecturas de un índice: el aviso dice cuántos hubo. Tamaño de `pg_relation_size`. |
| TimescaleDB | Igual | Igual; en una hypertable cada índice suma los de sus chunks (el índice del chunk con la misma definición) | Igual | No | Sí (de la tabla) | Igual | Sí | — |
| YugabyteDB | Igual | `idx_scan` del nodo al que está conectada la sesión, no del clúster | — (los contadores viven en memoria) | No | No | Igual | Sí | Sin tamaño. El aviso dice que los números son del nodo. |
| CockroachDB | `pg_index` (los métodos `prefix` e `inverted` de 26.x se muestran como BTREE y GIN, igual que en PostgreSQL, en el explorador, la comparación y el uso de índices) | `crdb_internal.index_usage_statistics`: `total_reads` a seeks y `last_read`, de todo el clúster, cruzado con `crdb_internal.table_indexes` | — | No | No | Activa `allow_unsafe_internals` solo para esa lectura; si el servidor la niega, sin contadores con un aviso | Sí | Sin tamaño. |
| Greenplum, Apache Cloudberry, Greengage | `pg_index` | `gp_stat_all_indexes_summary` (Greenplum 7) o `pg_stat_all_indexes`, que en Cloudberry ya suma los segmentos (cada consulta cuenta un recorrido por segmento que la atiende); escrituras de `gp_stat_all_tables_summary` o `pg_stat_all_tables`, como PostgreSQL | `stats_reset` del coordinador | No | Sí (de la tabla) | Como PostgreSQL | Cloudberry sí; Greenplum 7 y Greengage no | Greenplum 6 solo tiene las estadísticas del coordinador, que no ven las lecturas de los segmentos: sin contadores, con un aviso. |
| openGauss | `pg_index` | Como PostgreSQL (sin `last_idx_scan` ni particiones de `pg_partition_tree`) | `stats_reset` | No | Sí (de la tabla) | Como PostgreSQL | Sí | — |
| Materialize, Yellowbrick | `pg_index` | No cuentan el uso: sin contadores, con un aviso | — | — | — | — | Materialize sí; Yellowbrick no | Materialize no tiene claves primarias ni foráneas; Yellowbrick no tiene índices secundarios. |
| RisingWave, CrateDB, H2 | De la estructura de la tabla (`information_schema`, `pg_indexes`, SHOW CREATE TABLE en CrateDB): la PK y los índices | Sin contadores, con un aviso | — | — | — | — | Sí | CrateDB indexa cada columna por su cuenta: se ven la PK y los índices de texto completo. H2 muestra sus claves foráneas. |
| Amazon Redshift | No tiene índices (ordena y reparte con SORTKEY y DISTKEY): la carpeta queda vacía, con un aviso | — | — | — | — | — | No | Las claves foráneas (informativas) marcan sus columnas. |
| Amazon Aurora DSQL | `pg_index` (la PK incluida, DESC, INCLUDE, filtro) | No tiene estadísticas de uso: sin contadores, con un aviso | — | — | — | — | Contra PostgreSQL (`dbine-test-dsqlpg`), no contra DSQL | No tiene claves foráneas. |
| Oracle (y Autonomous Database) | `ALL_INDEXES`, `ALL_IND_COLUMNS`, `ALL_IND_EXPRESSIONS` (columnas de función y DESC), PK de `ALL_CONSTRAINTS`; sin los índices de LOB; los invisibles llevan `INVISIBLE` en el tipo | `DBA_INDEX_USAGE` (12.2+): `TOTAL_ACCESS_COUNT` a seeks, `LAST_USED` a la última lectura (la hora del volcado que registró el acceso, no la del acceso); escrituras = «db block changes» de los segmentos del índice en `V$SEGSTAT` (bloques, no filas, desde el arranque de la instancia). Las lecturas se conservan entre reinicios y las escrituras empiezan de cero con cada arranque; el aviso lo dice | — (Oracle no dice desde cuándo cuenta); con MONITORING USAGE, el `START_MONITORING` más antiguo | No | Sí (sin acceso a `V$SEGSTAT`, no) | SELECT_CATALOG_ROLE o SELECT ANY DICTIONARY; sin eso (o antes de 12.2), si todos los índices de una tabla propia tienen `MONITORING USAGE`, solo si cada uno se usó (`USER_OBJECT_USAGE`); si no, sin contadores y el aviso nombra el privilegio | Sí (23ai Free) | Oracle cuenta por muestreo y vuelca a `DBA_INDEX_USAGE` cada 15 minutos: el aviso muestra el último volcado y advierte que, hasta el próximo, un índice recién creado o recién usado tiene escrituras y 0 lecturas (se ve sin uso), y que uno que se usa poco puede no entrar en el muestreo. Tamaño de `DBA_SEGMENTS`, o de `USER_SEGMENTS` cuando los índices son del usuario (un índice sin segmento todavía figura con 0 KB); sin `DBA_SEGMENTS`, los de otro esquema quedan sin tamaño (desconocido, no 0 KB). No hay INCLUDE ni índices filtrados. |
| Google Cloud Spanner | `INFORMATION_SCHEMA.INDEXES` / `INDEX_COLUMNS`: la clave primaria (la tabla se guarda en su orden), los secundarios (UNIQUE, NULL_FILTERED, `STORING` como incluidas, filtro `WHERE`), de búsqueda y vectoriales; sin los que Spanner maneja para las claves foráneas | `SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR` (una fila por tabla y por índice, 30 días): lecturas (seeks) = suma de `READ_QUERY_COUNT`; escrituras = `WRITE_COUNT + DELETE_COUNT`. La clave primaria toma los de la tabla; un índice sin filas queda en cero | El `INTERVAL_END` más antiguo menos una hora (UTC) | No | Sí | `spanner.databases.select` (con control de acceso detallado, el rol `spanner_sys_reader`); sin eso, en el emulador o mientras la tabla horaria no tiene datos, sin contadores y el aviso nombra el permiso | Emulador (sin `SPANNER_SYS`: solo índices, claves y el aviso) | Tamaño: `USED_BYTES` de la última hora en `SPANNER_SYS.TABLE_SIZES_STATS_1HOUR`. Claves foráneas de `REFERENTIAL_CONSTRAINTS`. **Eliminar índice** genera `DROP INDEX` (o `DROP SEARCH/VECTOR INDEX`). |
| BigQuery | Índices de búsqueda (uno por tabla) y vectoriales (uno por columna), de `SEARCH_INDEXES` / `VECTOR_INDEXES` (columnas, `STORING`, estado si no está activo) | Lecturas (seeks) = consultas de los últimos 180 días que usaron el índice según `INFORMATION_SCHEMA.JOBS` de la región (`FULLY_USED` o `PARTIALLY_USED`, sin las que dicen no haber usado el índice de esta tabla). El uso se registra por consulta, no por tabla: el número es un máximo y el aviso lo dice | El inicio de la ventana de 180 días | No | No | `bigquery.jobs.listAll`; sin él, `JOBS_BY_USER` (solo las consultas propias) con un aviso; sin ninguno, sin contadores | No (solo pruebas unitarias; el emulador no tiene estas vistas) | Leer `JOBS` es una consulta facturada y corre cada vez que se abre la carpeta **Índices** de una tabla con índices; el aviso lo dice. Con más de un índice vectorial el trabajo no dice cuál usó: sin contadores. Tamaño `total_storage_bytes`; última escritura, el último refresco. La clave primaria (no aplicada) no se lista; las foráneas salen de `tables.get`. |
| Snowflake | Tablas híbridas: `SHOW INDEXES IN TABLE` (el de la clave primaria, los de claves únicas y foráneas y los secundarios con `INCLUDE`). Las tablas estándar no tienen índices: la carpeta queda vacía, con un aviso | No hay contadores por índice (`ACCESS_HISTORY` es por columna) | — | — | — | — | No (solo pruebas unitarias) | Claves foráneas de `SHOW IMPORTED KEYS IN TABLE`. Los índices de tablas híbridas no entran todavía en la comparación de esquemas, así que **Eliminar índice** no los encuentra: se borran con `DROP INDEX tabla.índice`. |
| Databricks (y Azure Databricks) | No tiene índices secundarios (estadísticas por archivo, clustering, Z-order): la carpeta queda vacía, con un aviso | — | — | — | — | — | No (solo pruebas unitarias) | Solo las claves foráneas de Unity Catalog (`information_schema`, informativas) para los íconos; sin Unity Catalog no hay. |
| Dremio | No tiene índices: se listan las **reflexiones** de la tabla (`sys.reflections`): tipo RAW o AGGREGATION (con el estado si no puede acelerar), columnas o dimensiones como clave y medidas como incluidas | Lecturas (seeks) = `accelerated_count`, las consultas que la reflexión aceleró | — (Dremio no dice desde cuándo cuenta) | No | No (los refrescos no se cuentan) | En Dremio Enterprise, VIEW REFLECTION para leer `sys.reflections` | Sí (OSS) | Tamaño `current_footprint_bytes`; última escritura, el último refresco. Las reflexiones entran en la comparación de esquemas como índices de la tabla (en una conversión a otro motor se omiten con un aviso): **Eliminar índice** genera `ALTER TABLE … DROP REFLECTION` y la sincronización las crea o las rehace. Sin claves. |
| MongoDB (y Amazon DocumentDB) | `listIndexes`: `_id_` (o el índice de una colección clustered) como clave primaria, claves con DESC, tipo (BTREE, TEXT, 2DSPHERE, HASHED, WILDCARD; TTL, SPARSE, HIDDEN), `partialFilterExpression` como filtro | `$indexStats`: `accesses.ops` a seeks; en una colección fragmentada se suman los shards. MongoDB no cuenta escrituras por índice (solo operaciones de la colección, que incluirían las de antes de crear el índice y las de documentos que un índice parcial no cubre): no se muestran y el aviso lo dice | El `accesses.since` más antiguo (el arranque del servidor o la creación del índice) | No | No | La acción indexStats (rol clusterMonitor, o dbAdmin en la base) y collStats; sin ella, sin contadores y el aviso nombra el rol | MongoDB sí; DocumentDB no | Tamaño de `storageStats.indexSizes`. Sin claves foráneas. **Eliminar índice** genera `dropIndex`. |
| FerretDB | Igual | Responde `$indexStats` con todo en cero: sin contadores, con un aviso | — | — | — | — | Sí | Tamaño de `collStats`. **Eliminar índice** funciona (FerretDB contesta `ok: true` en vez de `1`, y el driver lo acepta). |
| Neo4j | `SHOW INDEXES` de la etiqueta o el tipo de relación; los que respaldan una restricción van con el nombre de la restricción (KEY = clave primaria, UNIQUENESS = único); sin los LOOKUP, que son de todas las etiquetas | Neo4j 5: `readCount` a seeks y `lastRead` a la última lectura (los vuelca cada pocos segundos y cuenta como lectura la verificación de unicidad de cada alta). Neo4j 4 no tiene `readCount`: sin contadores | El `trackedSince` más antiguo | No | No | SHOW INDEX (Enterprise con RBAC); sin él, solo los índices de las restricciones (`SHOW CONSTRAINTS`), sin contadores, y el aviso nombra el privilegio | Sí (Community y Enterprise) | Sin tamaño ni claves foráneas. **Eliminar índice** genera `DROP INDEX` o `DROP CONSTRAINT`. |
| Memgraph | `SHOW INDEX INFO` y las restricciones únicas | No cuenta el uso: sin contadores, con un aviso | — | — | — | — | Sí | **Eliminar índice** genera `DROP INDEX ON :Etiqueta(propiedad)`. |
| Couchbase | `system:indexes` (GSI): `#primary` como clave primaria (sobre `meta().id`), claves, la condición `WHERE` como filtro, particionado en el tipo | Estadísticas del servicio de índices desde el cluster manager (`/pools/default/stats/range`, muestreadas cada pocos segundos): `index_num_requests` a seeks e `index_disk_size` al tamaño; la última lectura, de `last_known_scan_time` cuando el indexador la publicó. Su único contador de escrituras (`index_num_docs_indexed`) incluye los documentos de la construcción inicial: no se muestra y el aviso lo dice | El arranque del nodo de índices que arrancó último (su `uptime`) | No | No | External Stats Reader (o un rol de administración); sin él, sin contadores y con un aviso | Sí | Sin claves foráneas ni índices únicos; los índices de búsqueda (FTS) no se listan. **Eliminar índice** genera `DROP INDEX`. |
| Apache Cassandra, ScyllaDB | La clave primaria (partición, después clustering, con DESC) y `system_schema.indexes` (secundarios, SAI, SASI, con su destino: la columna, `keys(…)`, `values(…)`, `full(…)`) | No cuentan el uso por índice: sin contadores, con un aviso | — | — | — | — | Sí | Sin claves foráneas. **Eliminar índice** genera `DROP INDEX`. |
| Amazon Keyspaces | Solo la clave primaria | Igual | — | — | — | — | No | No tiene índices secundarios. |
| Azure Cosmos DB | La clave (`id` con la clave de partición), las rutas incluidas de la política de indexación (rango, con las excluidas como filtro), las claves únicas, los índices compuestos y los espaciales, de texto completo y vectoriales | No hay contadores por índice (las métricas de índices son por consulta): sin contadores, con un aviso | — | — | — | — | No (solo pruebas unitarias) | Sin tamaño ni claves foráneas. **Eliminar índice** no se puede: la política de indexación no se cambia desde un script (la sincronización lo avisa); se cambia en el portal de Azure o con la CLI. |
| CouchDB | En `_all_docs`: el índice de `_id` como clave primaria y los índices Mango (`GET _index`, json o text, DESC, `partial_filter_selector` como filtro) | No cuenta el uso por índice: sin contadores, con un aviso | — | — | — | — | Sí | Tamaño de `_design/…/_info` cuando el documento de diseño tiene ese índice solo. **Eliminar índice** genera `DELETE _index/<nombre>`, una extensión del driver que busca el documento de diseño al ejecutarse. |
| OrientDB | Los índices de la clase, de los metadatos de la base (UNIQUE, NOTUNIQUE, FULLTEXT, hash, Lucene…) | No cuenta el uso por índice: sin contadores, con un aviso | — | — | — | — | Sí | Los registros se ubican por `@rid`, que no es un índice: no hay entrada de clave primaria. Las propiedades LINK con clase enlazada son las claves foráneas. **Eliminar índice** genera `DROP INDEX`. |
| Amazon DynamoDB | La clave primaria (partición y orden, con el tamaño de la tabla), los GSI y los LSI con su proyección (`INCLUDE` como columnas incluidas, `KEYS_ONLY` en el tipo) y su tamaño | No cuenta lecturas por índice (la capacidad consumida por GSI está en CloudWatch): sin contadores, con un aviso | — | — | — | — | DynamoDB Local | Sin claves foráneas. **Eliminar índice** borra un GSI; un LSI no se borra sin recrear la tabla (la sincronización lo avisa). |
| SQLite, libSQL / Turso | La misma lectura del catálogo que **Comparar esquemas** (`pragma_index_list`, `pragma_index_xinfo`): la clave primaria (`ROWID` si es un `INTEGER PRIMARY KEY`, `CLUSTERED` en una tabla `WITHOUT ROWID`, si no su índice automático), los índices con DESC, expresiones y filtro parcial, y los de las restricciones UNIQUE con el nombre que usa la sincronización | SQLite no cuenta el uso de los índices: sin contadores, con un aviso | — | — | — | — | Sí (archivo local y servidor de libSQL) | Tamaño de `dbstat` (páginas × tamaño de página) donde la compilación lo tiene. Claves foráneas de `pragma_foreign_key_list`. **Eliminar índice** genera `DROP INDEX` (una UNIQUE de la tabla la rehace). |
| DuckDB | `duckdb_constraints()` y `duckdb_indexes()`: la clave primaria, las UNIQUE y los `CREATE INDEX`, todos ART | No cuenta el uso de los índices ni informa su tamaño: sin contadores, con un aviso (el plan muestra `INDEX_SCAN` cuando se usa uno) | — | — | — | — | Sí (archivo local) | Claves foráneas de `duckdb_constraints()`. **Eliminar índice** genera `DROP INDEX`. |
| Firebird | `RDB$INDICES`, `RDB$INDEX_SEGMENTS`, `RDB$RELATION_CONSTRAINTS`: tipo `ASC`/`DESC`, `COMPUTED` para los de expresión e `INACTIVE` para los desactivados; filtro de los parciales (Firebird 5) | No cuenta el uso por índice (`MON$RECORD_STATS.MON$RECORD_IDX_READS` es por tabla): sin contadores, con un aviso | — | — | — | — | Sí (Firebird 5) | Sin tamaño (solo `gstat` lo informa). Claves foráneas con nombre. **Eliminar índice** genera `DROP INDEX`. |
| SAP HANA | La lectura de **Comparar esquemas** reducida a la tabla (`SYS.INDEXES`, `SYS.INDEX_COLUMNS`, `SYS.FULLTEXT_INDEXES`, `SYS.CONSTRAINTS`); tipo = `INDEX_TYPE` (CPBTREE, BTREE, INVERTED VALUE/HASH/INDIVIDUAL) o FULLTEXT | No hay contadores de uso por índice: sin contadores, con un aviso | — | — | — | — | No (no hay contenedor) | Tamaño de `M_RS_INDEXES` (índices de tablas row store; en column store viven en los diccionarios de las columnas). Claves foráneas de `SYS.REFERENTIAL_CONSTRAINTS`. **Eliminar índice** genera `DROP INDEX` (o `DROP FULLTEXT INDEX`). |
| ClickHouse | La clave primaria (el índice disperso de MergeTree, tipo SPARSE, no único), los índices de salto (minmax, set, bloom_filter…, con su GRANULARITY) y las proyecciones | No cuenta el uso por índice (cuántos gránulos descarta cada uno sale de `EXPLAIN indexes = 1`, por consulta): sin contadores, con un aviso | — | — | — | — | Sí | Tamaño: `primary_key_bytes_in_memory` de `system.parts`, `data_compressed_bytes` de `system.data_skipping_indices` y `bytes_on_disk` de `system.projection_parts` (partes activas). Sin claves foráneas. **Eliminar índice** genera `ALTER TABLE … DROP INDEX` (o `DROP PROJECTION`). |
| Timeplus Proton | Igual (las mismas tablas de sistema; en un stream de tipo append la clave de ordenamiento no es clave primaria y no se lista) | Igual; el aviso nombra a Timeplus Proton | — | — | — | — | Sí (`dbine-test-proton`) | Igual. |
| Apache Phoenix | `SYSTEM.CATALOG`: la clave de fila (ROW KEY) y los índices GLOBAL y LOCAL, con DESC y las columnas cubiertas (`INCLUDE`) | No cuenta el uso por índice: sin contadores, con un aviso | — | — | — | — | Sí | Sin tamaño (vive en HBase) ni claves foráneas. **Eliminar índice** genera `DROP INDEX`, y la sincronización lo vuelve a crear con su DESC. |
| Flight SQL | Con DuckDB detrás (GizmoSQL): `duckdb_constraints()` y `duckdb_indexes()` por SQL. Con otro motor: la clave primaria y las foráneas de `GetPrimaryKeys` y `GetImportedKeys`, si el servidor los responde | Sin contadores, con un aviso | — | — | — | — | GizmoSQL sí; otros servidores no | Flight SQL no tiene comandos de índices: Dremio y DataFusion (InfluxDB 3) quedan con la clave, si la informan. No tiene sincronización de esquemas, así que **Eliminar índice** no aparece: se borra con `DROP INDEX` en una consulta. |
| Db2 (LUW, por ODBC) | `SQLStatistics`, `SQLPrimaryKeys` y `SQLForeignKeys` de la tabla, con las columnas INCLUDE de `SYSCAT.INDEXCOLUSE` | `MON_GET_INDEX` (LEFT JOIN desde `SYSCAT.INDEXES`: los no usados quedan en cero): `INDEX_SCANS` a seeks, escrituras = `KEY_UPDATES` + `INCLUDE_COL_UPDATES` (las inserciones no cuentan), última lectura = `SYSCAT.INDEXES.LASTUSED` (fecha) | `DB_CONN_TIME` de `MON_GET_DATABASE` (activación de la base) | No | Sí | EXECUTE sobre `MON_GET_INDEX` (o SQLADM, DBADM, DATAACCESS); sin eso, sin contadores y el aviso nombra el privilegio | No (solo pruebas unitarias) | Sin tamaño. |
| Db2 for i (ODBC) | Igual | `QSYS2.SYSINDEXSTAT`: `QUERY_USE_COUNT` a seeks y `LAST_QUERY_USE` | — | No | No | Lectura de `QSYS2.SYSINDEXSTAT`; si se niega, sin contadores con un aviso | No (solo pruebas unitarias) | — |
| Sybase ASE (ODBC) | Igual | `master..monOpenObjectActivity`: `UsedCount` a seeks, filas insertadas + borradas + actualizadas a escrituras, `LastUsedDate` | — (cuenta mientras el descriptor del objeto está abierto) | No | Sí | mon_role y las opciones «enable monitoring» y «per object statistics active»; sin eso, sin contadores y el aviso lo dice | No (solo pruebas unitarias) | — |
| Db2 for z/OS, Informix, GBase 8s, Teradata, SQL Anywhere, Altibase, CUBRID, Dameng, Mimer, Ingres, IRIS y Caché, OpenEdge, MonetDB, Virtuoso, MaxDB, Zen, NuoDB, Ocient, Ignite, Machbase, Access, dBase y el ODBC genérico | Del catálogo ODBC de la tabla | No exponen contadores por índice por SQL: sin contadores, con un aviso | — | — | — | — | El preset genérico sí (por el driver ODBC de SQL Server); los demás no | Candidatos: Teradata con DBQL de objetos (`DBC.DBQLObjTbl`, si está activado), Informix con `sysmaster:sysptprof` (por partición), Db2 for z/OS con `SYSINDEXSPACESTATS.LASTUSED` (solo la fecha). **Eliminar índice** con el script de la sincronización de cada motor. |
| Vertica, Exasol, Netezza (ODBC) | Sin índices definidos por el usuario (proyecciones, índices automáticos, zone maps): la clave primaria como restricción (CONSTRAINT) | — | — | — | — | — | No | Las claves foráneas (informativas) marcan sus columnas. |
| MySQL (y Aurora MySQL, Cloud SQL para MySQL) | `SHOW INDEX` (la PK incluida, prefijos `col(n)`, expresiones, DESC); claves foráneas de `KEY_COLUMN_USAGE` | `performance_schema.table_io_waits_summary_by_index_usage`: `COUNT_FETCH` (filas leídas por el índice) a seeks; las filas leídas sin índice (recorridos de la tabla) a scans de la clave primaria en InnoDB, donde la tabla es su índice agrupado; escrituras = las de la tabla (`COUNT_INSERT + COUNT_UPDATE + COUNT_DELETE`: los inserts no se atribuyen a ningún índice) | Arranque del servidor (`Uptime`); el aviso aclara que vale salvo que las estadísticas se hayan reiniciado o activado después | No | Sí (de la tabla) | SELECT sobre performance_schema; con `performance_schema = OFF`, el instrumento `wait/io/table/sql/handler` apagado o sin el permiso, sin contadores y el aviso dice por qué | Sí (8.4) | «Sin uso» equivale a `sys.schema_unused_indexes`. Tamaño de `mysql.innodb_index_stats` (páginas × `innodb_page_size`, particiones sumadas, según el último ANALYZE) si el usuario lo puede leer. Un TRUNCATE de la tabla de performance_schema reinicia los contadores. |
| MariaDB | Igual | Con `userstat = 1`: `information_schema.INDEX_STATISTICS` (`ROWS_READ` a seeks) y `TABLE_STATISTICS` (`ROWS_CHANGED` a escrituras; su `ROWS_READ` menos lo leído por índices, a scans de la clave primaria en InnoDB). Si no, performance_schema, como MySQL | Igual | No | Sí (de la tabla) | Las vistas de `userstat` en information_schema; performance_schema como MySQL | Sí (11.8) | performance_schema viene apagado por defecto: sin `userstat` ni performance_schema el aviso dice cómo activarlos (`SET GLOBAL userstat = 1` no requiere reiniciar). `FLUSH INDEX_STATISTICS` reinicia los contadores. |
| TiDB | `SHOW INDEX` | 8.0+: `CLUSTER_TIDB_INDEX_USAGE` (sumado entre instancias; `TIDB_INDEX_USAGE` si falla): las consultas que leyeron menos del 10 % de las filas de la tabla a seeks, el resto a scans; `LAST_ACCESS_TIME` a la última lectura; escrituras = `mysql.stats_meta.modify_count` (filas modificadas desde el último ANALYZE) | Arranque de la instancia (`Uptime`) | Sí | Sí (sin SELECT sobre `mysql.stats_meta`, no, y el aviso lo dice) | SELECT sobre `mysql.stats_meta` para las escrituras | Sí (7.5 y 8.5) | La clave primaria agrupada es el identificador de fila: TiDB no la cuenta y queda en cero (nunca «sin uso»). Solo cuenta en tablas con estadísticas. Antes de 8.0, sin contadores con un aviso. No guarda `DESC`. Sin tamaño. |
| OceanBase (modo MySQL) | `SHOW INDEX` | `oceanbase.DBA_INDEX_USAGE` (4.x; cruzado con `DBA_OBJECTS` por el nombre interno `__idx_<id de la tabla>_<índice>`): `TOTAL_ACCESS_COUNT` a seeks, `LAST_USED` a la última lectura; escrituras de `DBA_TAB_MODIFICATIONS` (desde las últimas estadísticas) | — (los contadores persisten entre reinicios) | No | Sí (si `DBA_TAB_MODIFICATIONS` se niega, no) | SELECT sobre la base `oceanbase`; con `_iut_enable` apagado o sin el permiso, aviso | Sí (4.4.2) | Cuenta por muestreo salvo con `_iut_stat_collection_type = 'ALL'` y vuelca a la vista en segundo plano: en 4.4.2 no apareció ningún acceso en 40 minutos, así que el cruce de nombres está verificado contra la definición de la vista, no con datos. `DBA_TAB_MODIFICATIONS` llega con demora y en 4.4 no cuenta los UPDATE. La clave primaria no se cuenta: queda en cero, nunca «sin uso». |
| SingleStore, StarRocks, Apache Doris, VeloDB, Databend, GreptimeDB | `SHOW INDEX` (Databend: `system.indexes`); GreptimeDB lista su PRIMARY y su TIME INDEX | No cuentan el uso de los índices: sin contadores, con un aviso | — | — | — | — | GreptimeDB sí; los demás no | En StarRocks y Doris la clave de ordenamiento no es un índice: se listan los bitmap, N-gram e invertidos. |

### Motores sin uso de índices

La carpeta **Índices** y la pestaña no aparecen (`supports_index_usage` es
false) donde no hay índices que mostrar:

- **Fabric Warehouse**: no tiene índices.
- **Denodo**: no tiene índices.
- **Amazon Neptune**: no tiene índices definidos por el usuario.
- **Elasticsearch, OpenSearch**: un índice de Elasticsearch es la tabla; cada
  campo se indexa por su cuenta (índice invertido, doc values, puntos) y no hay
  índices secundarios que listar ni borrar. Elasticsearch 7.15+ cuenta accesos
  por campo (`_field_usage_stats`), candidato para más adelante; OpenSearch no
  lo tiene.
- **Redis (Valkey, Dragonfly), etcd, ksqlDB**: no tienen índices secundarios
  por tabla.
- **Manticore Search**: la tabla es el índice.
- **Hive, Impala, Spark, Kyuubi, Cloudera, SQream, HeavyDB, NetSuite (ODBC)**:
  sin índices ni claves foráneas en su catálogo.
- **DuckDB, preset de archivos**: consulta archivos, que no tienen índices.
- **Apache Calcite Avatica (preset genérico)**: su protocolo no tiene
  metadatos de índices.
- **Trino, Presto, Starburst y Amazon Athena**: sin índices ni claves en su
  catálogo; los de cada conector no se exponen.
- **Apache Drill**: sin índices ni claves en su catálogo.
- **InfluxDB**: indexa todas las etiquetas por sí solo, sin índices por tabla
  ni contadores.
- **Solr**: como Elasticsearch, la colección es el índice; no hay índices
  secundarios.
- **Apache IoTDB**: series de tiempo por ruta, sin índices secundarios.
- **TDengine**: pendiente. TDengine 3 tiene índices sobre las etiquetas de
  una supertabla (sin contadores de uso), que todavía no se listan.

### Probado contra servidores reales

`index_usage_live` (SQL Server 2022, `dbine-test-sqlserver`): una tabla con
clave primaria y dos índices; cinco seeks sobre uno dan 5 seeks, el otro queda
en cero lecturas con escrituras (sin uso), y `since` viene con la hora de
inicio. `index_usage_babelfish_live` (`dbine-test-babelfish`): lista los
índices y avisa que no hay contadores.

`crates/drivers/mysql/tests/index_usage.rs` (`dbine-test-mysql` 8.4,
`dbine-test-mariadb` 11.8, `dbine-test-tidb` 7.5, `dbine-test-tidb8` 8.5,
`dbine-test-oceanbase` 4.4.2, `dbine-test-greptimedb`): una tabla con clave
primaria, una clave foránea y dos índices; cinco búsquedas por uno dan 5
seeks, el otro queda sin lecturas con escrituras (sin uso), un recorrido de la
tabla suma scans a la clave primaria (MySQL, MariaDB), la foránea apunta a su
tabla y el índice sin uso se borra con el script de **Comparar esquemas**
(`ALTER TABLE … DROP INDEX`). MySQL y MariaDB con contadores: el aviso dice
que valen desde el arranque, salvo reinicio o activación posterior. MySQL con
un usuario sin SELECT sobre performance_schema: índices y foráneas sin
contadores, con el aviso del permiso. MariaDB sin `userstat` ni
performance_schema: el aviso de cómo activarlos; con `userstat`, los
contadores. TiDB 7.5: sin contadores, con el aviso de la versión; 8.5: seeks
con color de salud, última lectura y escrituras contadas; un usuario sin
SELECT sobre `mysql.stats_meta` ve las lecturas sin escrituras (nada «sin
uso») y el aviso que nombra el permiso. OceanBase: índices, foránea,
contadores legibles (vacíos, ver la tabla) y el borrado. GreptimeDB: PRIMARY y
TIME INDEX sin contadores.

Oracle (`crates/drivers/oracle/tests/index_usage.rs`, Oracle 23ai Free,
`dbine-test-oracle`): `index_usage_live` crea una tabla con clave primaria,
clave foránea y dos índices (uno con DESC y una función), hace cinco búsquedas
por uno y espera el volcado de `DBA_INDEX_USAGE`: el índice usado da 5 accesos
con su último uso, el otro queda en cero con escrituras (sin uso), el aviso
habla del volcado y del muestreo, y **Eliminar índice…** (el script de
sincronización sin ese índice, `DROP INDEX`) lo borra.
`index_usage_without_privileges_live`: un usuario sin SELECT_CATALOG_ROLE ve
los índices, su tamaño y las claves foráneas sin contadores y con el aviso del
privilegio; con MONITORING USAGE en todos los índices ve cuál se usó. Un
lector de otro esquema, con solo SELECT sobre esa tabla, ve los índices y las
claves foráneas sin tamaño (desconocido, no 0 KB).

`crates/drivers/postgres/tests/index_usage.rs` (`dbine-test-postgres`,
`-timescale`, `-yugabyte`, `-cockroach`): una tabla con clave primaria, una
clave foránea y tres índices (uno parcial); seis búsquedas por uno dan 6
seeks, los otros quedan sin lecturas. En PostgreSQL y TimescaleDB el índice
común sin lecturas sale «sin uso» con las escrituras de la tabla y el parcial
queda sin escrituras (nunca «sin uso») con el aviso de los índices parciales;
en YugabyteDB y CockroachDB, que no cuentan escrituras, quedan en 0 %. En
CockroachDB 26.x los índices se ven como BTREE. La clave foránea apunta a su
tabla y el índice parcial se elimina con el script de **Comparar esquemas**
(`DROP INDEX`). En una hypertable de TimescaleDB el índice suma los recorridos
de sus chunks. Contra openGauss y Cloudberry se listan índices y claves con
contadores; contra Materialize, RisingWave, CrateDB y H2, sin contadores. En
`crates/drivers/dsql/tests/index_usage.rs` (DSQL contra `dbine-test-dsqlpg`) se
listan los índices sin contadores y se elimina uno con el script de
sincronización.

`index_usage_live` de Spanner (emulador, `dbine-test-spanner`): una tabla con
clave primaria, una foránea y dos índices (uno NULL_FILTERED, otro con
`STORING`); lista la clave primaria y los dos índices sin el de la foránea,
con columnas, `DESC` y almacenadas, la foránea, y el aviso que nombra el
permiso (el emulador no tiene `SPANNER_SYS`); borra un índice con el script de
la sincronización (`DROP INDEX`).

`index_usage_live` de Dremio (OSS, `dbine-test-dremio`): dos reflexiones sobre
una tabla de `$scratch`; las consultas que acelera una dan sus lecturas
(100 %), la otra queda en 0 % sin escrituras; la sincronización la borra
(`DROP REFLECTION`) y la vuelve a crear.

BigQuery, Snowflake y Databricks solo tienen pruebas unitarias (servicios en
la nube). Queda sin probar contra un servicio real: las columnas de
`SHOW INDEXES` de las tablas híbridas de Snowflake y la convención de nombre
`SYS_INDEX_*_PRIMARY`, las rutas de error de `JOBS` y `JOBS_BY_USER` de
BigQuery (y el filtro por `index_unused_reasons.base_table`, que si falla cae
al conteo simple), los joins de `information_schema` de Unity Catalog en
Databricks y la lectura de `SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR` en un
Spanner real (el emulador no la tiene).

Bases documentales y de grafos (`crates/drivers/<motor>/tests/index_usage.rs`):

- `mongodb` (`dbine-test-mongodb`): una colección con dos índices; cinco
  búsquedas por uno dan 5 lecturas, el otro queda en 0 %, sin escrituras
  (MongoDB no las cuenta por índice) y con el aviso que lo explica, con tamaño
  y `since`. En FerretDB (`dbine-test-ferretdb`) se listan sin contadores.
- `neo4j` (`dbine-test-neo4j`): una restricción y dos índices; cinco búsquedas
  por uno dan 5 lecturas y su última lectura, el otro queda en 0 %. Contra
  Neo4j Enterprise (`dbine-test-neo4j-ee`), un usuario sin SHOW INDEX ve los
  índices de las restricciones con el aviso del privilegio. En Memgraph
  (`dbine-test-memgraph`) se listan sin contadores.
- `couchbase` (`dbine-test-couchbase`): `#primary` y dos índices; cinco
  consultas por uno dan 5 lecturas, el otro queda en 0 % con su tamaño, sin
  escrituras y con el aviso de la construcción inicial (no sale «sin uso»).
- `cassandra` (`dbine-test-scylladb` y `dbine-test-cassandra`), `couchdb`,
  `orientdb` (con su LINK como clave foránea) y `dynamodb` (DynamoDB Local):
  listan la clave y los índices sin contadores.

En todos, **Eliminar índice…** (el script de la sincronización sin ese índice)
lo borra. Cosmos DB solo tiene pruebas unitarias.

Embebidos y analíticos (`crates/drivers/<motor>/tests/index_usage.rs`): una
tabla con clave primaria, una clave foránea y dos índices, cinco búsquedas
por uno.

- `sqlite` y `duckdb` (archivos locales, sin `#[ignore]`), `libsql`
  (`dbine-test-libsql`), `firebird` (`dbine-test-firebird`, Firebird 5) y
  `flightsql` (GizmoSQL, `dbine-test-flightsql`): listan la clave, los dos
  índices y la clave foránea, sin contadores y con el aviso; SQLite y libSQL
  con el tamaño de `dbstat`.
- `clickhouse` (`dbine-test-clickhouse`): la clave dispersa y dos índices de
  salto con su tamaño, sin claves foráneas, y el aviso que nombra a
  ClickHouse. `timeplus_index_usage_live` (`dbine-test-proton`): un stream con
  un índice minmax lo lista sin contadores, con el aviso que nombra a Timeplus
  Proton.
- `phoenix` (`dbine-test-phoenix`): la clave de fila, dos índices globales
  (uno con su columna cubierta) y uno local con DESC, que la sincronización
  borra y vuelve a crear igual.

En todos menos Flight SQL (que no tiene sincronización y lo borra con
`DROP INDEX`), **Eliminar índice…** lo borra con el script de la
sincronización. SAP HANA y las variantes de ODBC con contadores (Db2, Db2 for
i, ASE) solo tienen pruebas unitarias (no hay contenedores);
`crates/drivers/odbc/tests/index_usage.rs` prueba el preset genérico por un
driver ODBC real.

## Dependencias

Clic derecho en una tabla, una vista, una rutina o una columna ›
**Ver dependencias…** abre una pestaña con lo que depende de ese objeto:
claves foráneas, índices y claves primarias, restricciones CHECK, y vistas,
rutinas y triggers cuyo código lo usa, con las líneas donde aparece. Cada
resultado dice qué tan seguro es: **Confirmada** (lo registra el catálogo del
motor), **Probable** (el código nombra el objeto; para una columna, junto con
su tabla) o **Revisar** (el nombre solo aparece dentro de un texto, como en el
SQL dinámico). Los comentarios no cuentan, ni un nombre calificado con otro
esquema, ni un alias. Las aplicaciones y los reportes externos no se ven.

El contrato está en `crates/dbine-driver/src/dependencies.rs`
(`Driver::supports_dependencies` y `Session::dependents`). La versión por
defecto sirve para todos los motores: toma las claves foráneas, los índices y
los checks de `database_schema` y lee una por una las definiciones de las
vistas, rutinas, triggers, paquetes, sinónimos, alias, streams, tasks y sinks
(`CODE_KINDS`). Los drivers que tienen un registro de dependencias lo usan en
su lugar:

- **SQL Server y Azure SQL Database**: una sola consulta
  sobre `sys.sql_modules` trae solo los cuerpos que nombran el objeto o que
  `sys.sql_expression_dependencies` registra como usuarios. Las columnas solo
  quedan confirmadas en los objetos con SCHEMABINDING (el catálogo no registra
  columnas en los demás). Los módulos cifrados figuran como no legibles.
  **Babelfish** y **Fabric Warehouse** usan la versión por defecto si su
  catálogo no tiene esa vista.
- **Los demás motores**: la versión por defecto. En una base con miles de
  rutinas tarda, porque lee cada definición por separado. Las versiones con el
  catálogo de PostgreSQL (`pg_depend`), Oracle (`ALL_DEPENDENCIES`) y MySQL
  (`VIEW_TABLE_USAGE`) están pendientes.

Los drivers que corren en su propio proceso (los que se descargan bajo
demanda) responden `Dependents` por el protocolo; un proceso publicado antes
responde que no lo conoce y la app corre la versión por defecto a través de
sus otras llamadas.

### Motores sin dependencias

La opción no aparece (`supports_dependencies` es false) donde no hay claves
foráneas ni objetos con código que puedan depender de otros:

- **Redis, Valkey, Dragonfly, etcd**: solo keys.
- **Amazon DynamoDB, Azure Cosmos DB, Apache Solr, Manticore Search**: tablas,
  colecciones e índices sin vistas, rutinas ni claves foráneas (los
  procedimientos de Cosmos DB son JavaScript que el driver no lista).
- **Amazon Keyspaces**: sin vistas materializadas ni funciones (Cassandra y
  ScyllaDB sí las tienen y entran por la versión por defecto).
- **Amazon Neptune**: etiquetas y relaciones sin consultas guardadas.
- **InfluxDB 1 (InfluxQL), InfluxDB 2 (Flux), InfluxDB 3 (SQL), Apache IoTDB,
  TimechoDB**: medidas y series sin objetos que dependan de otros (las
  consultas continuas y las tareas de InfluxDB no se listan).

### Probado contra servidores reales

`crates/drivers/sqlserver/tests/dependencies.rs` (SQL Server 2022,
`dbine-test-sqlserver`): una tabla con clave primaria, un índice y un CHECK
sobre una columna, otra tabla con una clave foránea hacia ella, una vista con
SCHEMABINDING, una sin él, un procedimiento que usa la columna, uno con SQL
dinámico y uno que usa una columna del mismo nombre de otra tabla. Para la
tabla: la foránea, las vistas y el procedimiento quedan confirmados, el SQL
dinámico para revisar, y el procedimiento de la otra tabla no aparece. Para la
columna: la vista con SCHEMABINDING queda confirmada, el procedimiento como
probable (con la línea `SELECT Pepe`), el índice y el CHECK confirmados, y ni
la vista que no la usa ni el procedimiento de la otra tabla aparecen. La
versión por defecto, corrida sobre la misma base, encuentra el mismo código.

## Deshabilitar y habilitar índices

Clic derecho en un índice (en el explorador o en la pestaña **Índices**) ›
**Deshabilitar índice…**; si ya está deshabilitado, **Habilitar índice…**. El
diálogo muestra la sentencia del motor y lo que conviene saber antes, y corre
como tarea (como **Eliminar índice…**). Un índice deshabilitado queda definido
pero el optimizador no lo usa: el explorador y la pestaña lo marcan
**deshabilitado** y nunca como "sin uso". El contrato está en
`crates/dbine-driver/src/lib.rs` (`Driver::supports_index_toggle` y
`Driver::index_toggle_script`) y el estado en `IndexUsage::disabled`. No forma
parte de la estructura (`IndexDef`): **Comparar esquemas** no ve diferencia
entre un índice habilitado y uno deshabilitado.

| Motor | Deshabilitar | Habilitar | Notas |
|---|---|---|---|
| SQL Server, Azure SQL Database | `ALTER INDEX … DISABLE` | `ALTER INDEX … REBUILD` | Deja de mantenerse y libera su espacio; habilitarlo lo reconstruye entero. El índice clustered deja la tabla sin acceso, y el de la clave primaria o un UNIQUE deshabilita las claves foráneas que lo apuntan (no vuelven solas). |
| MySQL 8, Aurora MySQL, Cloud SQL para MySQL, TiDB, OceanBase (MySQL) | `ALTER TABLE … ALTER INDEX … INVISIBLE` | `… VISIBLE` | Se sigue manteniendo. La clave primaria (también la implícita) no se puede. OceanBase sin probar contra un servidor. MySQL 5.7 rechaza la sentencia. |
| MariaDB | `… IGNORED` | `… NOT IGNORED` | Desde 10.6. Una base MariaDB conectada como "MySQL" recibe la sintaxis de MySQL y la rechaza. |
| Oracle, Oracle Autonomous Database | `ALTER INDEX … INVISIBLE` | `ALTER INDEX … VISIBLE` | No UNUSABLE: sigue mantenido y garantizando la unicidad (también el de la clave primaria). Un índice UNUSABLE se marca deshabilitado y habilitarlo lo reconstruye, por particiones si hace falta. Los de tablas IOT y de cluster no se pueden. |
| Firebird | `ALTER INDEX … INACTIVE` | `ALTER INDEX … ACTIVE` | Activarlo lo reconstruye. Los índices de restricciones (PRIMARY KEY, FOREIGN KEY, UNIQUE) no se pueden. |
| CockroachDB | `ALTER INDEX t@ix NOT VISIBLE` | `… VISIBLE` | Desde 22.2. La clave primaria no se puede. |
| MongoDB | `hideIndex` | `unhideIndex` | Desde 4.4 (`collMod`). El índice `_id_` no se puede. |
| IBM Informix, GBase 8s (ODBC) | `SET INDEXES … DISABLED` | `… ENABLED` | Sin probar contra un servidor. No se mantiene mientras está deshabilitado; habilitarlo lo reconstruye. |
| SAP MaxDB (ODBC) | `ALTER INDEX … DISABLE` | `… ENABLE` | Sin probar contra un servidor. |

### Motores sin deshabilitar índices

La opción no aparece (`supports_index_toggle` es false) donde el motor no
tiene una forma nativa de apagar un índice sin borrarlo:

- **PostgreSQL y su familia** (PostgreSQL, TimescaleDB, YugabyteDB,
  KingbaseES, AlloyDB para PostgreSQL, Amazon Aurora PostgreSQL, Cloud SQL
  para PostgreSQL, EDB Postgres Advanced Server, Fujitsu Enterprise Postgres,
  openGauss, Greenplum, Apache Cloudberry, Greengage DB, Amazon Redshift,
  Amazon Aurora DSQL, H2 (servidor PostgreSQL), Materialize, RisingWave,
  Yellowbrick): no hay forma soportada; marcar el índice como inválido
  tocando `pg_index` a mano no es razonable de ofrecer. **Babelfish for
  PostgreSQL** tampoco acepta `ALTER INDEX … DISABLE`.
- **SQLite, libSQL / Turso, DuckDB, Archivos dBase (DBF), Microsoft Access**:
  sin estado de índice; solo se crean y se borran.
- **IBM Db2 (LUW), IBM Db2 for i (AS/400), IBM Db2 for z/OS, SAP ASE
  (Sybase), SAP SQL Anywhere, SAP HANA, Teradata, Actian Ingres, Actian Zen
  (Pervasive PSQL), Mimer SQL, CUBRID, Altibase, InterSystems IRIS,
  InterSystems Caché, Progress OpenEdge, NuoDB, MonetDB, OpenLink Virtuoso,
  Machbase, Ocient, Exasol, IBM Netezza, Vertica, ODBC (genérico)**: sin
  sentencia para deshabilitar un índice (o sin índices de usuario). CUBRID 10
  podría tener `INVISIBLE`: pendiente de verificar.
- **Dameng (DM)**: pendiente; probablemente acepta `ALTER INDEX … INVISIBLE`,
  sin verificar la sintaxis ni dónde se lee el estado.
- **SingleStore, StarRocks, Apache Doris, VeloDB, Databend, GreptimeDB,
  ClickHouse, Timeplus Proton, Apache Phoenix, CrateDB, Apache Ignite 2,
  Apache Ignite 3, Arrow Flight SQL, Dremio, Snowflake, Google BigQuery,
  Google Cloud Spanner, Databricks SQL, Azure Databricks**: los índices (o
  claves de ordenamiento, índices de salto) solo se crean y se borran.
- **FerretDB**: rechaza `hidden` (probado contra `dbine-test-ferretdb`).
  **Amazon DocumentDB**: no tiene índices ocultos (según la documentación de
  AWS, sin probar).
- **Apache Cassandra, ScyllaDB, Amazon Keyspaces, Couchbase, CouchDB, Azure
  Cosmos DB, Amazon DynamoDB, Neo4j, Memgraph, OrientDB**: sin forma de
  apagar un índice secundario sin borrarlo.

### Probado contra servidores reales

Cada prueba crea una tabla con un índice, lo deshabilita con el script del
driver, comprueba que `index_usage` lo marca deshabilitado y que la tabla se
sigue consultando, lo habilita y comprueba que vuelve:

- `crates/drivers/sqlserver/tests/index_toggle.rs` (SQL Server 2022,
  `dbine-test-sqlserver`).
- `crates/drivers/mysql/tests/index_toggle.rs` (MySQL 8.4, MariaDB 11.8,
  TiDB 7.5 y 8.5): además, la clave primaria se rechaza.
- `crates/drivers/oracle/tests/index_toggle.rs` (Oracle 23ai Free): además, el
  índice de la clave primaria invisible sigue rechazando duplicados
  (ORA-00001), un índice UNUSABLE se reconstruye al habilitarlo (también por
  particiones) y el de una tabla IOT se rechaza.
- `crates/drivers/firebird/tests/index_toggle.rs` (Firebird 5): la clave
  primaria se rechaza y una restricción UNIQUE con nombre la rechaza el
  servidor.
- `crates/drivers/postgres/tests/index_toggle.rs` (CockroachDB 26.3): la clave
  primaria se rechaza.
- `crates/drivers/mongodb/tests/index_toggle.rs` (MongoDB 7 y FerretDB 2):
  `_id_` se rechaza; FerretDB no ofrece la opción y el servidor rechaza
  `hideIndex`.

## Ejecutar en varias bases

En el editor de consultas, **Ejecutar en varias bases…** ejecuta el código
(o la selección) en varias bases de la misma conexión y junta los
resultados. El diálogo lista las bases de la conexión con un filtro que
admite `*` (por ejemplo `*tenant-n*`), **Todas** / **Ninguna** sobre lo
que muestra el filtro, y recuerda la última elección por conexión; la
primera vez viene elegida la base de la pestaña. Cada base corre en una
sesión propia (nunca la del explorador), de a 4 a la vez, partida como la
ejecución normal del motor (sentencia por sentencia, lote por lote o
entera) y con el mismo máximo de filas por base. Corre como tarea: sigue en
segundo plano y se cancela desde el diálogo o el panel **Tareas** (no
arranca ninguna base más y se interrumpen las que están corriendo, como el
Cancelar del editor).

- **Seguridad**: una conexión de solo lectura sigue rechazando todo lo que
  no sea lectura. Si el código no es solo de lectura (la misma
  clasificación que la guarda de solo lectura), o el motor no es SQL y no
  se puede verificar, pide confirmación diciendo en cuántas bases va a
  correr. Nunca se ejecuta solo.
- **Resultados**: si el primer resultado de cada base que terminó bien tiene
  las mismas columnas (sin distinguir mayúsculas), se ve una sola grilla
  con una primera columna `base`; se exporta y se copia como cualquier
  grilla (las filas cargadas). Si no, una pestaña de resultado por base.
  **Mensajes** tiene el resumen por base: filas y tiempo, o el error.
- El comando es `run_multi_db` / `cancel_multi_db`
  (`src-tauri/src/commands/multi_db.rs`) y usa solo `Session::execute`:
  funciona en todos los motores con varias bases.

### Motores sin ejecutar en varias bases

La acción no aparece donde el motor tiene una sola base
(`DriverInfo::databases_label` vacío: el explorador muestra los objetos
directo bajo la conexión). Motivo en todos: una sola base.

- **Archivos**: SQLite, libSQL / Turso, Archivos CSV / Parquet / JSON,
  Archivos dBase (DBF), Microsoft Access.
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
- **NoSQL y búsqueda**: Amazon DynamoDB, Amazon Neptune, Elasticsearch,
  OpenSearch, Open Distro for Elasticsearch, Apache Solr, etcd, ksqlDB.

### Probado contra servidores reales

SQL Server (`dbine-test-sqlserver`) y PostgreSQL (`dbine-test-postgres`):
tres bases con la misma tabla se juntan en una grilla con `base`; con una
cuarta de columnas distintas queda un resultado por base; una base sin la
tabla informa su error y las demás se juntan igual; en una conexión de solo
lectura un `DELETE` se rechaza (probado en SQL Server)
(`cargo test -p dbine --lib multi_db -- --include-ignored`).

## Autenticación integrada (Windows / Kerberos)

Entrar con la cuenta del dominio en lugar de un usuario de la base
([`autenticacion-integrada.md`](autenticacion-integrada.md)).

| Motor | Qué hay |
|---|---|
| SQL Server | **Windows: usuario actual**: SSPI en Windows (NTLM), Kerberos con el ticket de la sesión en macOS y Linux. **Windows: usuario y contraseña de dominio** (NTLMv2): en Windows, macOS y Linux. |
| MongoDB | **Kerberos (GSSAPI)** con el ticket de la sesión (SSPI en Windows). Requiere MongoDB Enterprise. |
| Motores por ODBC (Db2, Teradata, Hive, Impala, Spark, Vertica…) y ODBC genérico | Con los atributos del driver ODBC en **Atributos adicionales** (`Authentication=KERBEROS`, `AuthMech=1`, `Trusted_Connection=yes`…), que reemplazan a los del mismo nombre. |

### Motores sin autenticación integrada

| Motor | Motivo |
|---|---|
| SQL Server desde macOS y Linux, con usuario y contraseña de dominio | Pendiente. El cliente (tiberius) hace NTLM fuera de Windows con `sspi-rs`, cuya versión fija una versión preliminar de `crypto-bigint` incompatible con la del cliente SSH (russh). Se resuelve con un parche a la copia de tiberius de DBine (`vendor/tiberius`) que use su cliente NTLMv2 propio, en Rust, en todas las plataformas. Mientras tanto: **Windows: usuario actual** con `kinit`. |
| Azure SQL Database, Microsoft Fabric | No tienen logins de Windows (Active Directory local): usan Microsoft Entra ID. Entra ID integrado no está todavía. |
| Babelfish for PostgreSQL | No tiene logins de Windows. |
| Oracle, Oracle Autonomous | El cliente es el thin de Oracle en Rust (`oracledb`), que solo hace el login con contraseña (O5LOGON). La autenticación externa (`/`, wallet con credenciales, Kerberos, usuario del sistema operativo) es del cliente con Instant Client, que DBine no usa. La del usuario del sistema operativo por red (`REMOTE_OS_AUTHENT`) además ya no existe desde Oracle 21c. |
| PostgreSQL y compatibles (TimescaleDB, AlloyDB, Cloud SQL, Aurora, EDB, YugabyteDB, CockroachDB, Greenplum…), Amazon Aurora DSQL | El cliente (tokio-postgres) rechaza los métodos GSSAPI y SSPI del servidor; no hay forma de agregarlos sin reescribir su inicio de sesión. |
| MySQL, MariaDB, TiDB y compatibles | El cliente (mysql_async) no tiene los plugins `authentication_kerberos_client`, `authentication_windows_client` (MySQL Enterprise) ni `auth_gssapi_client` (MariaDB). |
| SAP HANA | El cliente (hdbconnect) solo hace el login con usuario y contraseña. |
| Firebird | El cliente en Rust solo hace SRP; la seguridad integrada de Windows (`Win_Sspi`) es de la biblioteca nativa fbclient. |
| Cassandra, ScyllaDB | El autenticador Kerberos es de DataStax Enterprise; el cliente (scylla) no lo trae. |
| Neo4j | El esquema `kerberos` de Bolt necesita un plugin del servidor y un ticket que DBine no obtiene todavía. |
| ClickHouse, Trino, Elasticsearch, OpenSearch, Solr, Apache Phoenix, Apache Drill, Dremio, CouchDB, InfluxDB, ksqlDB | El servidor admite Kerberos por HTTP (SPNEGO) en algunos casos, pero el cliente HTTP de DBine no negocia SPNEGO. Pendiente. |
| Redis, etcd, Couchbase, OrientDB, Apache IoTDB, TDengine | El motor no tiene autenticación de Windows ni Kerberos. |
| Arrow Flight SQL | El protocolo solo define usuario y contraseña o un token. |
| BigQuery, Spanner, Snowflake, Databricks, Athena, DynamoDB, Cosmos DB y otros servicios en la nube | Usan la identidad de la nube (cuentas de servicio, IAM, tokens), no la del dominio. |
| SQLite, DuckDB, libSQL y otros motores de archivo local | No hay servidor al que autenticarse. |

### Probado

- Unidad: cada modo arma la autenticación correcta de tiberius (SQL, usuario
  actual, NTLM), qué campos muestra el formulario en cada modo, que Azure
  SQL, Fabric y Babelfish no ofrezcan Windows, los mensajes de Kerberos, el
  login GSSAPI de MongoDB y los atributos ODBC que reemplazan a los del
  preset.
- Contra servidores reales: el login con usuario de SQL Server sigue igual
  (`dbine-test-sqlserver`) y el de MongoDB con usuario y contraseña también
  (`dbine-test-mongodb`).
- **Sin probar de punta a punta**: no hay un dominio de Active Directory de
  prueba, así que el usuario actual (SSPI y Kerberos), NTLM y Kerberos de
  MongoDB no se probaron contra un servidor real.

## Procesos

La lista de procesos del Monitor ([`procesos.md`](procesos.md)). Cada motor
la completa con lo que informa; hay tres capacidades por separado: listar,
cancelar la consulta de otra sesión (la sesión sigue abierta) y terminar la
sesión (su transacción se deshace).

**Listan procesos:** PostgreSQL y los motores que conservan
`pg_stat_activity` (TimescaleDB, AlloyDB, Cloud SQL, Aurora PostgreSQL, EDB,
Fujitsu, KingbaseES, openGauss, Greenplum, Cloudberry, Greengage,
YugabyteDB), CockroachDB, Redshift, Yellowbrick, Materialize, RisingWave,
CrateDB, H2, Denodo, Aurora DSQL; MySQL, MariaDB, Aurora MySQL, TiDB,
OceanBase, SingleStore, StarRocks, Doris, VeloDB, Databend, Manticore,
GreptimeDB; SQL Server, Azure SQL, Fabric y Babelfish; Oracle, SAP HANA,
Firebird; MongoDB, FerretDB, DocumentDB, Elasticsearch, OpenSearch,
Couchbase, CouchDB, Cassandra, ScyllaDB, OrientDB; Redis, Valkey,
Dragonfly, Neo4j, Memgraph, Amazon Neptune, InfluxDB 1 y 3, IoTDB,
TDengine, ksqlDB; ClickHouse, Trino, Presto, Starburst, Drill, Dremio,
Databricks, Snowflake, BigQuery, Athena y Spanner; y, por ODBC, Db2 LUW,
Db2 for i, Sybase ASE, SQL Anywhere, Teradata, Vertica, Exasol, Netezza,
Dameng y Altibase.

**Cancelan la consulta de otra sesión:** todos los anteriores salvo los de
la tabla de abajo (SQL Server y su familia, Cassandra y ScyllaDB, InfluxDB 3,
Dragonfly, Denodo, FerretDB, Sybase ASE, SQL Anywhere, Teradata, Netezza y
Altibase). Cada motor usa su forma nativa: `pg_cancel_backend`, `KILL
QUERY`, `ALTER SYSTEM CANCEL SQL`, `killOp`, `CLIENT UNBLOCK`,
`TERMINATE TRANSACTION`, `WLM_CANCEL_ACTIVITY`, `INTERRUPT_STATEMENT`, etc.
En Neo4j, Memgraph, ksqlDB y CouchDB «cancelar» tiene un alcance distinto
(deshace la transacción, pausa la consulta persistente, solo detiene
replicaciones transitorias): está en [`procesos.md`](procesos.md#según-el-motor).

**Terminan la sesión:** los que aparecen en [Bloqueos](#bloqueos) con
«Terminar sesiones», SQL Server y su familia (también Fabric y Babelfish),
Netezza, Redis, Valkey y Dragonfly (`CLIENT KILL`), Firebird, OrientDB y
Snowflake (la sesión de la consulta elegida). TDengine, ClickHouse, Trino,
Presto, Starburst, Drill, Dremio, Databricks, BigQuery, Athena, Spanner,
Couchbase, Elasticsearch, OpenSearch, CouchDB, InfluxDB, IoTDB, ksqlDB,
Memgraph, Neptune, Db2 for i, Teradata, Altibase, Manticore y GreptimeDB
no terminan sesiones (o no tienen sesiones que terminar).

**Pruebas:** hay pruebas contra servidor real (`tests/processes.rs` de cada
driver, que se corren con la variable `DBINE_TEST_<MOTOR>_URL` y se omiten
sin ella) para los drivers propios de cada motor. Las variantes por ODBC
(Db2 LUW, Db2 for i, Sybase ASE, SQL Anywhere, Teradata, Vertica, Exasol,
Netezza, Dameng y Altibase) no tienen prueba de este tipo: se escribieron
según la documentación del fabricante y se verifican con respuestas
simuladas.

| Motor | Qué falta | Motivo |
|---|---|---|
| SQL Server, Azure SQL, Fabric | Cancelar la consulta | `KILL` es lo único que existe: cierra la sesión y deshace su transacción; no hay forma de detener la sentencia y conservar la sesión. |
| Babelfish | Cancelar la consulta | Babelfish no permite detener la consulta de otra sesión desde T-SQL: solo terminarla (`KILL`), lo que cierra la sesión. |
| Fabric | Texto de la sentencia | Las vistas del almacén no se pueden unir con `dm_exec_sql_text`. |
| Cassandra, ScyllaDB | Cancelar y terminar | CQL no puede detener el pedido de otro cliente ni cerrar su conexión. La lista es la del nodo coordinador (tablas virtuales locales); Cassandra 4.0 o posterior (`system_views`), y las consultas en curso desde la 4.1. ScyllaDB solo lista conexiones. |
| Amazon Keyspaces | Todo | Amazon Keyspaces no expone sus conexiones ni las consultas en curso. |
| InfluxDB 3 | Cancelar | No tiene `KILL QUERY` ni una API para detener la consulta de otro cliente. |
| InfluxDB 2 (Flux) | Todo | Ninguna API lista ni detiene las consultas de otros clientes. |
| Dragonfly | Cancelar | No tiene `CLIENT UNBLOCK`: no se puede cortar el comando bloqueante de otro cliente sin cerrar su conexión. Tampoco informa comando ni usuario. |
| Denodo | Cancelar y terminar | VQL no tiene forma de cancelar una consulta: se cancelan desde Diagnostic & Monitoring Tool o por JMX. |
| FerretDB | Cancelar y terminar | Lista sus sesiones (backends de PostgreSQL) con poco detalle y no tiene `killOp`. |
| Amazon Neptune | Terminar | Solo tiene consultas en curso (`/openCypher/status`) y las cancela con `cancelQuery`; no hay sesiones. |
| Sybase ASE | Cancelar | ASE solo termina una sesión entera (`KILL`). El texto del lote sale de las tablas MDA (`monProcessSQLText`) y solo si el monitoreo está activo. |
| SQL Anywhere | Cancelar | No hay cancelación del pedido de otra conexión, solo `DROP CONNECTION`. |
| Teradata | Cancelar, terminar y texto | Abortar un pedido necesita el id de host de PM/API, que SQL no da. El texto exige una llamada a `MonitorSQLText` por sesión, por eso no se muestra. |
| Netezza | Cancelar | Solo se termina una sesión (`DROP SESSION`). |
| Altibase | Cancelar | No hay cancelación por SQL. **Pendiente**: terminar sesiones (no está implementado; falta confirmar la sentencia que Altibase ofrece). |
| Db2 for i | Terminar | **Pendiente**: cancela con `QSYS2.CANCEL_SQL`, pero terminar el trabajo no está implementado (sus vistas de bloqueos difieren de Db2 LUW y no se pudieron validar). |
| Manticore, GreptimeDB | Terminar | Solo informan consultas en curso: `KILL` termina la consulta, no hay sesión que cerrar. |
| Materialize, RisingWave, CrateDB | Terminar | Cancelan con su forma nativa; terminar sesiones no está en las vistas de bloqueos de estos motores (ver [Bloqueos](#bloqueos)). |
| TDengine | Terminar | Sus conexiones son el pool compartido de taosAdapter: cerrar una cortaría a otros clientes. |
| Spanner (emulador) | Todo | El emulador no implementa `SPANNER_SYS` ni `cancel_query`: no hay consultas en curso que listar. Spanner real sí. |
| Spanner, ClickHouse, Trino, Presto, Starburst, Drill, Dremio, Databricks, BigQuery, Athena, Couchbase, Elasticsearch, OpenSearch | Terminar | Su protocolo es HTTP/REST sin sesiones que el operador pueda cerrar (las de Spanner son un pool del cliente): listan consultas o tareas en curso, que sí se cancelan. |
| Solr | Todo | No lleva una lista de consultas ni de sesiones: `/tasks/list` solo ve, núcleo por núcleo, las consultas enviadas con `canCancel=true`. |
| Azure Cosmos DB, DynamoDB | Todo | Son servicios HTTP sin estado: no exponen sesiones ni consultas en curso. |
| etcd | Todo | No tiene sesiones de servidor ni una vista de los pedidos en curso: cada pedido es independiente (las «sesiones» de los clientes son leases) y no se puede cancelar el de otro cliente. |
| libSQL / Turso | Todo | Cada pedido Hrana es independiente y el servidor no tiene una vista ni una API para listarlos o cancelarlos. |
| SQLite, DuckDB | Todo | Son bases embebidas: no hay un servidor con sesiones de otros clientes que listar ni consultas ajenas que cancelar. |
| Flight SQL | Todo | El protocolo no define cómo listar ni cancelar las consultas de otros clientes: depende del servidor. Para Dremio o Apache Doris, el driver propio sí las lista. |
| Phoenix | Todo | Ni Phoenix ni HBase llevan una lista que se pueda leer o detener, y el Query Server (Avatica) solo conoce sus propias conexiones. |
| Db2 for z/OS (ODBC) | Todo | No expone sus hilos por SQL: se ven con `-DISPLAY THREAD`, IFI u OMEGAMON. |
| Spark Thrift Server, Kyuubi (ODBC) | Todo | No exponen sus sesiones por SQL: se ven en la interfaz web de Spark. |
| Hive (ODBC) | Todo | HiveServer2 no lista sus sesiones por SQL: se ven en su interfaz web (puerto 10002). |
| Actian Zen (ODBC) | Todo | No expone sus sesiones por SQL: se ven en Zen Monitor. |
| Mimer SQL (ODBC) | Todo | No expone sus sesiones por SQL: se ven con `sqlmonitor`. |
| NetSuite (ODBC) | Todo | SuiteAnalytics Connect es un servicio de solo lectura: no informa sesiones. |
| Access, dBase (ODBC) | Todo | Son bases de archivos sin servidor: no tienen sesiones que listar. |
| ODBC genérico | Todo | El preset genérico no conoce las vistas de sesiones del motor: hay que usar el preset del motor. |
| Informix, Cubrid, MonetDB, IRIS, OpenEdge, MaxDB, NuoDB, HeavyDB, Machbase, Ignite, Ignite3, Ocient, SQream, Ingres, Virtuoso, Impala (ODBC) | Todo | **Pendiente**, no imposible: DBine todavía no lista los procesos de estos motores. Falta escribir, para cada uno, la consulta a sus vistas de sesiones (en el código el preset responde «este motor todavía no lista sus procesos en DBine»). |

## Tareas programadas

Las tareas programadas ([`tareas-programadas.md`](tareas-programadas.md)) funcionan en **todos los motores**, con los seis tipos de paso: ejecutar un script, exportar a archivo, comparar esquemas, backup, documentar la base y enviar un mail. No agregan nada al driver: cada paso usa lo que el motor ya ofrece para esa función.

- **Ejecutar un script y Exportar:** hasta donde llega la ejecución de scripts y la exportación de cada motor. La exportación corre siempre en solo lectura.
- **Comparar esquemas:** hasta donde llega la comparación de cada motor, que no cambia por ser una tarea (ver [Comparar esquemas](#comparar-esquemas)). El script de sincronización se guarda en un archivo y nunca se ejecuta.
- **Backup:** el **Backup del motor** solo existe en los motores que tienen backups propios (ver [Backups](#backups)). Los demás ofrecen solo la **Copia de DBine**, así que todos los motores se pueden respaldar.
- **Documentar la base:** hasta donde llega la función (ver [Documentar la base](#documentar-la-base)); corre en una sesión de solo lectura.
- **Enviar un mail:** no usa el driver, así que no depende del motor: envía por SMTP con el servidor de **Configuración › Correo**.

Probado: los pasos de las tareas tienen pruebas automáticas con bases SQLite (aprobación de cambios, exportación, comparación, copia de DBine, detener o seguir ante un error). Sobre los demás motores no hay pruebas propias de las tareas: cada paso usa el mismo código que la función equivalente de la app, con el soporte y las pruebas contra servidores que se listan en sus secciones.

El registro en el programador del sistema (LaunchAgent, Programador de tareas, systemd o cron) y la notificación se probaron con los textos que generan en las pruebas automáticas; según el código, la ejecución real contra cada sistema operativo no tiene pruebas automáticas.

| Motor | Qué falta | Motivo |
|---|---|---|
| Los de la lista "Qué falta y por qué" de [Backups](#backups) | Backup del motor | Sin backups propios por SQL o por el protocolo, el paso ofrece solo la **Copia de DBine**. Los motivos de cada motor están en esa sección. |
| Los de la sección [Comparar esquemas](#comparar-esquemas) con límites | Lo que la comparación no cubre | La tarea compara lo mismo que la función; los límites y sus motivos están en esa sección. |
| Conexiones que no guardan su contraseña | Correr sin atención | La tarea lee la contraseña del llavero del sistema; sin contraseña guardada no hay quién la escriba. |
| Cualquiera | **Solo si…** en pasos que no sean **Enviar un mail** | El motor de tareas evalúa la condición en cualquier paso, pero la interfaz solo la ofrece en **Enviar un mail**. Pendiente explícito: ofrecerla en los demás tipos de paso. |
| Linux sin systemd | Acceso al llavero desde cron | Una tarea registrada con `crontab` puede no ver el llavero del usuario; un timer de systemd de usuario sí. |

## Calidad de código

La calidad de código ([`calidad-de-codigo.md`](calidad-de-codigo.md)) marca problemas en el editor de **todos los motores** salvo InfluxDB 2, que usa Flux. Es análisis de texto: no consulta el servidor ni agrega nada al driver. Cada motor recibe el analizador léxico y las reglas de su familia, que salen de `DriverInfo` (`language`, `dialect`, `id`) en `src-tauri/src/lint/mod.rs`:

| Familia de reglas | Motores |
|---|---|
| SQL (comunes) | Todos los de lenguaje SQL |
| T-SQL (además de las comunes) | SQL Server, Azure SQL, Fabric, Babelfish y, por ODBC, Sybase ASE y SQL Anywhere (dialecto `mssql` o `sybase`) |
| PostgreSQL | Los de dialecto `postgres`: PostgreSQL y su familia, y Aurora DSQL |
| MySQL | Los de dialecto `mysql`: MySQL, MariaDB, TiDB, OceanBase y la familia |
| Oracle | Oracle y Oracle Autonomous |
| InfluxQL | InfluxDB 1 e InfluxDB 3 |
| CQL (comunes de SQL que corresponden, más las de CQL) | Cassandra, ScyllaDB y Amazon Keyspaces |
| MongoDB | MongoDB, FerretDB y Amazon DocumentDB |
| CouchDB | CouchDB |
| Consolas de búsqueda | Elasticsearch, OpenSearch y Solr |
| Redis | Redis, Valkey y Dragonfly |
| etcd | etcd |
| Cypher | Neo4j, Memgraph y Amazon Neptune |

**Probado:** las reglas de todas las familias y la asignación de motores a familias tienen pruebas automáticas sobre texto (`src-tauri/src/lint/tests.rs` y `src-tauri/src/commands/lint.rs`). Como el análisis no usa el servidor, no hay pruebas contra servidores reales ni hacen falta.

| Motor | Qué falta | Motivo |
|---|---|---|
| InfluxDB 2 (Flux) | Todas las reglas | Pendiente explícito: falta un analizador léxico de Flux y reglas para él. El perfil de Flux no tiene reglas. |
| Los motores SQL sin familia propia (SQLite, libSQL, DuckDB, ClickHouse, Snowflake, BigQuery, Databricks, Spanner, Trino, Athena, SAP HANA, Firebird, Db2, Teradata, Vertica y el resto de los presets ODBC) | Reglas propias del motor | Pendiente explícito: solo reciben las reglas comunes de SQL. Faltan las reglas de cada familia. |
| Cosmos DB, DynamoDB (PartiQL), Couchbase (N1QL), OrientDB, ksqlDB, IoTDB, TDengine | Reglas propias del motor | Pendiente explícito: sus lenguajes parecen SQL y reciben las reglas comunes de SQL, pero no hay reglas propias de cada dialecto. |
| Elasticsearch, OpenSearch, Solr | Reglas distintas de `leading-wildcard` y `write-all` | Pendiente explícito: faltan reglas para sus consolas (por ejemplo, lecturas sin filtro). |

## Documentar la base

**Documentar la base…** ([`documentar-la-base.md`](documentar-la-base.md)) y su paso de tarea programada funcionan en **todos los motores**: el documento se arma con lo que el motor informa por el contrato (`list_objects`, `database_schema`, `columns`, `definition`), así que cada motor sale con lo que tiene. No agrega métodos al driver. Corre en una sesión de solo lectura.

**Probado:** una base SQLite de archivo, con claves, índice, `CHECK`, vista y trigger, documentada en HTML y en Markdown y como paso de una tarea (prueba automática); y PostgreSQL contra el contenedor `dbine-test-postgres` (prueba marcada como ignorada, que se ejecuta a mano). En los demás motores no hay pruebas propias del documento: cada parte usa las mismas llamadas que el explorador, con el soporte y las pruebas de sus secciones.

| Motor | Qué falta | Motivo |
|---|---|---|
| Los que no tienen claves foráneas (`capabilities().foreign_keys` falso: por ejemplo Cassandra y ScyllaDB, MongoDB, CouchDB, Couchbase, Cosmos DB, Redis, Neo4j, ClickHouse, Athena, Dremio, InfluxDB, IoTDB y Aurora DSQL) | Claves foráneas y las líneas del diagrama | El motor no tiene claves foráneas; las tablas del diagrama quedan sin líneas. |
| Los de [Motores sin dependencias](#motores-sin-dependencias) | **Usada por** | El motor no tiene claves foráneas ni objetos con código que dependan de otros; la opción sale deshabilitada. |
| Los que no devuelven el código de un tipo de objeto (`has_definition` falso en ese tipo) | Código fuente de ese tipo | El motor no tiene un texto para devolver; el documento lo lista sin código. |
| Cualquiera | Diagrama con más de 150 tablas por esquema | Límite del diagrama (`DIAGRAM_MAX`): con más tablas se arma un diagrama por esquema y el que supera el máximo queda sin diagrama, con un aviso. Pendiente explícito: un diagrama que se pueda recortar o paginar. |
| Cualquiera | Diagrama en Markdown | El diagrama es un SVG dentro del HTML; Markdown no lo lleva. |

## Constructor de consultas

**Diseñar consulta…** ([`constructor-de-consultas.md`](constructor-de-consultas.md)) está en todos los motores de lenguaje **SQL y CQL**. Los demás lenguajes (documentos, clave-valor, grafos, Flux) no lo tienen, porque no hay un `SELECT` que armar. Los nombres de las tablas, las comillas y el límite de filas salen de la consulta **Ver datos** de cada driver (`Session::browse_query`); lo que ese texto no dice (uniones, agrupación, `HAVING`, operadores) está por dialecto en `features()` de `src-tauri/src/commands/query_builder.rs`. No agrega métodos al driver.

**Probado:** las consultas generadas para SQL Server, PostgreSQL, Oracle, MySQL, MS Access, Cosmos DB, CQL y ksqlDB tienen pruebas automáticas sobre el texto del SQL, y una unión se ejecuta de punta a punta contra SQLite. No hay pruebas contra servidores reales de los demás motores.

Qué ofrece cada motor (lo que no está en la tabla lo ofrece completo: `INNER`, `LEFT`, `RIGHT` y `FULL`, `GROUP BY`, `HAVING`, los seis agregados, `DISTINCT`, `ORDER BY`, límite, grupos `OR` y los trece operadores):

| Motor | Qué falta | Motivo |
|---|---|---|
| MySQL y su familia, MS Access | Unión `FULL OUTER JOIN` | El dialecto MySQL (MySQL, MariaDB y la familia) y Access no tienen `FULL OUTER JOIN`. Access además anida cada unión entre paréntesis. |
| SQLite y libSQL anteriores a 3.39 | `RIGHT` y `FULL` | `RIGHT` y `FULL JOIN` llegaron a SQLite en la 3.39; el constructor lee la versión del servidor. |
| Couchbase (N1QL), HeavyDB | `RIGHT` y `FULL` | Pendiente explícito: el constructor solo genera `INNER` y `LEFT`, pero el código no registra el motivo del motor; falta confirmarlo con la documentación del fabricante. |
| Sybase ASE, CUBRID, Ignite, NuoDB, OpenEdge, Zen, Machbase, NetSuite (ODBC) | `FULL OUTER JOIN` | Pendiente explícito: el constructor solo genera `INNER`, `LEFT` y `RIGHT`, pero el código no registra el motivo del motor; falta confirmarlo con la documentación del fabricante. |
| Cosmos DB, DynamoDB (PartiQL), InfluxDB 1, IoTDB, TDengine, ksqlDB, OrientDB | Uniones | Consultan un contenedor, una medida, un dispositivo, un stream o una clase por vez; el constructor deja una sola tabla en el lienzo. |
| Cassandra, ScyllaDB, Amazon Keyspaces | Uniones, `GROUP BY`, `HAVING`, `DISTINCT`, grupos `OR`, `COUNT DISTINCT` y los operadores distintos de `=`, `<`, `<=`, `>`, `>=` e `IN` | El constructor genera CQL de una sola tabla. Un filtro fuera de la clave primaria agrega `ALLOW FILTERING`, con un aviso de que recorre la tabla. |
| Cosmos DB | `HAVING`, `COUNT DISTINCT`, `IS NULL` e `IS NOT NULL` | Pendiente explícito: el código no registra el motivo del motor; falta confirmarlo con la documentación del fabricante. |
| DynamoDB (PartiQL) | `GROUP BY`, `HAVING`, agregados, `DISTINCT`, `ORDER BY`, `LIKE` y `NOT LIKE`; el límite de filas | Pendiente explícito: el código no registra el motivo del motor. La consulta de **Ver datos** de DynamoDB no lleva límite de filas. |
| InfluxDB 1 | `HAVING`, `DISTINCT` y los operadores distintos de las seis comparaciones; el orden solo por `time` | Pendiente explícito: el código no registra el motivo del motor. |
| IoTDB | `GROUP BY`, `HAVING`, `DISTINCT` y `COUNT DISTINCT` | Pendiente explícito: el código no registra el motivo del motor. |
| ksqlDB | `DISTINCT`, `ORDER BY` y `COUNT DISTINCT` | Pendiente explícito: el código no registra el motivo del motor. |
| OrientDB | `HAVING` y `COUNT DISTINCT` | Pendiente explícito: el código no registra el motivo del motor. |
| TDengine, MS Access | `COUNT DISTINCT` | Pendiente explícito: el código no registra el motivo del motor. |

## Copiar un subconjunto

**Copiar un subconjunto…** ([`subconjunto-de-datos.md`](subconjunto-de-datos.md)) está en el menú de las tablas de todos los motores (con objetos que tienen columnas y se pueden explorar). El origen y el destino pueden ser motores distintos: la estructura de las tablas que faltan se convierte con `dbine_schema`. No agrega métodos al driver: usa `database_schema`, `Driver::filtered_browse` (el filtro de claves `IN`), `table_ddl`, `insert_script` y `update_script`. El origen se lee en una sesión de solo lectura.

**Probado:** SQLite a SQLite (pruebas automáticas): una tabla con hijas, padres y enmascaramiento, un ciclo de claves foráneas con una clave compuesta, y los rechazos (mismo origen y destino, destino de solo lectura, producción sin confirmar). PostgreSQL a PostgreSQL contra el contenedor `dbine-test-postgres` (prueba marcada como ignorada, que se ejecuta a mano). Los demás motores no tienen pruebas propias.

| Motor | Qué falta | Motivo |
|---|---|---|
| Los sin claves foráneas (documentos, clave-valor, series de tiempo: ver la lista de [Documentar la base](#documentar-la-base)) | Padres e hijas | No hay claves foráneas que seguir: se copia la tabla o colección elegida, con su filtro y el enmascaramiento. |
| Los que no filtran en el servidor (ver [Filtros por columna](#filtros-por-columna-en-los-datos-de-una-tabla)) | Filtro de la tabla de inicio, y buscar padres e hijas por clave | La copia pide los filtros al driver (`filtered_browse`); donde el driver no los aplica, la copia puede fallar con ese error. Redis y etcd son los casos que esa sección declara sin filtro en el servidor. |
| Motores sin lenguaje de condiciones (los que no son SQL ni CQL) | **Condición** | Solo se ofrece el filtro **Por columna**, el de la grilla de datos. |
| Cualquiera, con destino de otro motor | Tablas que no se pueden crear | Si la conversión de la estructura al motor del destino falla, la tabla queda marcada **no se puede copiar** con el error y bloquea la copia: hay que crearla antes en el destino. |
| Cualquiera | Copiar más de 2.000.000 de filas, o más de 1.000.000 de la tabla de inicio; deshacer una copia parcial | Límites de la función: las filas se juntan en memoria antes de escribirlas, y no hay una transacción alrededor de la copia. Pendiente explícito: copiar por lotes sin juntar todo en memoria. |

## Optimizar consulta

**Optimizar consulta** ([`optimizar-consulta.md`](optimizar-consulta.md)) está en el editor de **todos los motores**. Tiene cuatro partes, que dependen del motor de maneras distintas: las **reglas** de reescritura (por lenguaje y dialecto), los **índices sugeridos** (del plan estimado), las **alternativas de la IA** (cualquier motor: se envía la consulta, la estructura y el plan, nunca filas) y **Comparar** (ejecuta con `Session::execute` y el receptor de filas de la exportación, en una sesión de solo lectura). No agrega métodos al driver: usa `explain`, `supports_explain`, `database_schema` y `table_ddl`.

**Probado:** SQLite, con pruebas automáticas, las reglas, la comparación (también la que detecta una versión no equivalente) y los índices sugeridos. Contra los contenedores `dbine-test-*` hay pruebas marcadas como ignoradas, que se ejecutan a mano: PostgreSQL (índices sugeridos y comparación), SQL Server (el índice faltante que informa el motor) y MongoDB (`COLLSCAN` y `$where`). Los demás motores no tienen pruebas propias.

| Motor | Qué falta | Motivo |
|---|---|---|
| Cosmos DB, DynamoDB (PartiQL), ksqlDB, IoTDB, TDengine, InfluxDB 1 (InfluxQL), OrientDB, Couchbase (N1QL) | Reglas de reescritura | Su lenguaje no tiene las construcciones que las reglas reescriben (subconsultas, `UNION`, uniones en su forma general) o trata los `NULL` a su manera. Quedan las alternativas de la IA y las propias. |
| Los de lenguaje no SQL, salvo MongoDB (CQL, Cypher, Redis, etcd, Flux, Elasticsearch, OpenSearch, Solr, CouchDB) | Reglas de reescritura | Pendiente explícito: no hay reglas para esos lenguajes. Quedan las alternativas de la IA y las propias. |
| MongoDB, FerretDB, Amazon DocumentDB | Todas las reglas salvo `mongo_where` | Pendiente explícito: solo se reescribe `$where`, y solo si son comparaciones de campos con constantes unidas por `&&`. |
| Todos los SQL salvo PostgreSQL (y su familia), MySQL (y su familia), SQL Server y Oracle | Regla `function_to_range` | Pendiente explícito: falta escribir el literal de fecha de cada dialecto (`date_sql`). |
| Los que no dan plan: Redis, Valkey, Dragonfly, etcd, IoTDB, FerretDB, InfluxDB 2 (Flux), y por ODBC Exasol, CUBRID, Informix, GBase 8s, Altibase, Db2 for i, Ingres, Mimer, Caché, Zen, Access, dBase, NetSuite y OpenEdge | Índices sugeridos, **Avisos del plan** y el costo en **Comparar** | El motor no tiene planes de ejecución o no los entrega por el protocolo disponible. El motivo de cada uno está en [Planes de ejecución](#planes-de-ejecución). **Comparar** sigue midiendo tiempos y resultado. |
| Todos menos SQL Server | Índice sugerido por el propio motor | Solo SQL Server informa índices faltantes en su plan. En los demás, la sugerencia sale de un recorrido completo de una tabla que la consulta filtra o une por columnas con las que ningún índice empieza (en MongoDB, un `COLLSCAN`). |
| Los que escriben datos (`INSERT`, `UPDATE`, `DELETE`, y todo lo no SQL que escribe) | Ejecución en **Comparar** | Una consulta que escribe nunca se ejecuta: se compara solo su plan estimado. En los lenguajes que no son SQL la sesión de solo lectura rechaza la escritura. |

## Búsqueda

**Buscar en la base…** ([`busqueda.md`](busqueda.md)) funciona en **todos los motores**. Los nombres salen de `list_objects` y las columnas de `database_schema` (si el motor no la da, la búsqueda sigue con nombres y código). El código se busca de dos maneras con el mismo resultado: por el catálogo del motor en pocas consultas (`Session::search_code`) o, donde el driver no la implementa, leyendo la definición de cada objeto con avance y resultados parciales. Corre en una sesión de solo lectura.

Vía rápida por el catálogo, según cada `search.rs`:

| Motor | Qué se lee del catálogo | Lo que queda para la lectura objeto por objeto |
|---|---|---|
| SQL Server, Azure SQL | `sys.sql_modules` (vistas, rutinas, triggers), filtrado en el servidor con `LIKE` | Secuencias, sinónimos, tipos y catálogos de texto, de a uno |
| PostgreSQL y su familia | `pg_get_viewdef`, `pg_get_functiondef` y `pg_get_triggerdef`, una consulta por tipo, filtradas con `LIKE`/`ILIKE` | Secuencias, tipos y sinónimos (se arman con varios catálogos). CockroachDB, los motores de streaming, CrateDB, H2, Redshift y Denodo: todo, porque sus fuentes solo salen de a una (`SHOW CREATE`) |
| Oracle | `DBMS_METADATA.GET_DDL` de vistas, rutinas, paquetes y triggers en una consulta, y de las tablas en otra, filtradas con `DBMS_LOB.INSTR`; tipos, secuencias y sinónimos de `ALL_SOURCE`, `ALL_SEQUENCES` y `ALL_SYNONYMS` | Todo, si una lectura en bloque falla (por ejemplo, sin privilegio sobre `DBMS_METADATA`) |
| SAP HANA | `DEFINITION` de vistas, procedimientos, funciones y triggers, sin filtrar en el servidor (son NCLOB) | Tablas y secuencias (`GET_OBJECT_DEFINITION`, una llamada por objeto), sinónimos y tipos de tabla |
| Firebird | `RDB$RELATIONS`, `RDB$PROCEDURES`, `RDB$FUNCTIONS`, `RDB$PACKAGES`, `RDB$TRIGGERS`, `RDB$GENERATORS` y `RDB$FIELDS`, sin filtrar en el servidor | — |
| ClickHouse | `system.tables` (tablas, vistas, diccionarios, streams) y `system.functions`, filtradas en el servidor | — |
| BigQuery | `INFORMATION_SCHEMA.TABLES` y `ROUTINES` (el `ddl`) | Los objetos sin `ddl` en `INFORMATION_SCHEMA`; en el emulador, todo |
| Snowflake | `FUNCTIONS`, `PROCEDURES` y `SEQUENCES` de `INFORMATION_SCHEMA` | Tablas, vistas, streams y tareas (`GET_DDL`, una llamada por objeto) y las rutinas sobrecargadas |
| Databricks | `routine_definition` de las funciones | Tablas, vistas y vistas materializadas (`SHOW CREATE TABLE`, una sentencia por objeto) |
| Presets ODBC | Las consultas de definición de cada preset, ejecutadas una vez para todos los objetos | Los tipos cuya fuente no es una consulta (`SHOW …` en Hive, Impala, Spark y Teradata; `GET_DDL(?)`; Netezza) y el preset genérico, que adivina `INFORMATION_SCHEMA` |
| Todos los demás (MySQL y familia, SQLite, libSQL, DuckDB, Trino, Athena, Spanner, Cassandra, MongoDB, etcétera) | — | Todo, objeto por objeto. Pendiente explícito: una vía por catálogo; los drivers que no la tienen leen cada definición por separado y tardan más en bases con miles de rutinas. |

Sin definiciones que leer, solo se buscan nombres y columnas: los motores cuyos tipos de objeto no tienen código (clave-valor, series de tiempo, la mayoría de los de documentos).

**Probado:** el código de las vías por catálogo afirma, en cada `search.rs`, que da los mismos resultados que la lectura objeto por objeto; no tengo registro de pruebas contra servidores reales de la búsqueda.

## Chequeo de salud

El **Chequeo de salud** ([`chequeo-de-salud.md`](chequeo-de-salud.md)) funciona en **todos los motores** con los chequeos comunes, que usan lo que DBine ya lee (Monitor, procesos y backups). Cada driver puede sumar los suyos con `Session::health_checks`; los motores sin chequeos propios muestran solo los comunes. Corre en una sesión de solo lectura y los scripts de corrección solo se abren en una consulta.

| Chequeo común | Motores que no lo tienen | Motivo |
|---|---|---|
| Conexiones y aciertos de caché | Los sin Monitor (`capabilities().monitor` falso) | Sin Monitor no hay métricas de conexiones ni de caché. Ver [Monitor del servidor](#monitor-del-servidor). |
| Consultas largas, bloqueos y transacciones abiertas | Los sin lista de procesos (`capabilities().processes` falso) | Sin procesos no hay qué medir. Ver [Procesos](#procesos). |
| Último backup | Los sin backups propios (`Driver::backup()` vacío) | Sin backups del motor no hay historial que consultar. Ver [Backups](#backups). |

Chequeos propios (un `health.rs` por driver):

| Motor | Qué revisa | Qué no |
|---|---|---|
| SQL Server, Azure SQL | Configuración, estadísticas, índices sin uso, restricciones no confiables, claves foráneas sin índice, heaps, índices deshabilitados | Lo que exige `VIEW SERVER STATE` o no existe en Azure se saltea |
| PostgreSQL y familia | Autovacuum, tuplas muertas, *wraparound*, índices sin uso, inválidos y duplicados, claves foráneas sin índice, tablas sin clave primaria, secuencias | Vacuum y tuplas muertas, en CockroachDB y YugabyteDB (su almacenamiento no tiene `VACUUM`) y en las variantes MPP (los contadores del coordinador no ven los segmentos); *wraparound* en openGauss (sus XID son de 64 bits). Específicos: CockroachDB (estadísticas automáticas), Redshift (estadísticas vencidas y filas sin ordenar) |
| MySQL, MariaDB, TiDB, OceanBase | Tablas sin clave primaria, MyISAM, índices sin uso y redundantes, fragmentación, claves foráneas sin índice, collations mezcladas | Índices sin uso: sin contadores (`performance_schema` o `userstat` apagados) se saltea, y en OceanBase se saltea porque guarda los conteos sin fecha de inicio. Fragmentación: no en Aurora (su almacenamiento no informa ese espacio). Claves foráneas sin índice: no en OceanBase, InnoDB y TiDB crean uno solos |
| Oracle | Inválidos, índices inutilizables, tablespaces, estadísticas, claves foráneas sin índice, tablas sin clave primaria, secuencias, papelera | Lo que necesita vistas `DBA_` sin acceso se saltea |
| SAP HANA | Inválidos, fusión *delta*, tablas sin clave primaria, tablas virtuales sin estadísticas | Lo que necesita vistas de monitoreo sin acceso se saltea |
| Firebird | Distancia entre transacciones, escrituras forzadas, estadísticas de índices, índices inactivos, tablas sin clave primaria | — |
| ClickHouse | Particiones con demasiadas partes, partes desprendidas, réplicas, mutaciones, tablas sin TTL | Versiones viejas, Timeplus o sin acceso a `system`: se saltea cada chequeo que falla |
| Snowflake | Time Travel, *clustering*, tablas borradas retenidas, *warehouses* que no se suspenden | Solo costo y mantenimiento, con `SHOW`: no se despierta ningún *warehouse* ni se leen datos |
| BigQuery | Tablas grandes sin particionar, filtro de partición, vencimiento, modelo de cobro, *time travel* | Solo costo y mantenimiento, con la API REST (metadatos gratuitos, sin jobs) |
| Databricks | Autoapagado, optimización predictiva, retención de archivos borrados, tablas que no son Delta | Tablas grandes sin *clustering*: el tamaño exige `DESCRIBE DETAIL` en un *warehouse*, y este chequeo no usa ninguno |
| ODBC: Db2 LUW, Sybase ASE, Informix y GBase 8s | Ver [`chequeo-de-salud.md`](chequeo-de-salud.md) | Db2 for i y z/OS, Teradata, Vertica y los demás presets: sus catálogos no se leen todavía (pendiente explícito); el preset genérico no conoce el motor |
| Todos los demás (Cassandra, MongoDB, Redis, Neo4j, DuckDB, SQLite, etcétera) | Solo los comunes | Pendiente explícito: no hay chequeos propios; falta definir qué conviene revisar en cada uno |

Cada chequeo propio es una consulta aparte: si falla (versión vieja, permisos), se saltea y se lista en **No se pudieron revisar**.

## Datos de prueba

**Generar datos de prueba…** ([`datos-de-prueba.md`](datos-de-prueba.md)) funciona en los motores que **insertan desde DBine con `insert_script`**. Los generadores viven en DBine; el driver solo aporta el script de inserción, las columnas y, donde existen, las claves foráneas. Aparece en las tablas de conexiones que no son de solo lectura.

Tienen `insert_script` propio: Athena, BigQuery, Cassandra, ClickHouse, Cosmos DB, Couchbase, CouchDB, Databricks, Dremio, DynamoDB, Elasticsearch, etcd, Firebird, SAP HANA, ksqlDB, MongoDB, MySQL y su familia, Neo4j, ODBC, Oracle, OrientDB, Phoenix, PostgreSQL y su familia, Redis, Solr, Snowflake, Spanner, SQL Server, TDengine y Trino. El resto usa el `INSERT` estándar de los motores SQL (SQLite, libSQL, DuckDB, Flight SQL, Aurora DSQL…).

**Probado:** de punta a punta, solo SQL Server (`DBINE_TEST_SQLSERVER_URL`, prueba marcada como ignorada que se ejecuta a mano). Los demás motores no tienen pruebas propias de esta función.

| Motor | Qué falta | Motivo |
|---|---|---|
| Apache Drill | Todo | Drill no tiene `INSERT`: las tablas se crean con `CREATE TABLE AS SELECT`. |
| InfluxDB 1, 2 y 3 | Todo | Flux no tiene lenguaje de inserción, InfluxQL no tiene `INSERT` por la API HTTP y el SQL de InfluxDB 3 es de solo lectura: los puntos se escriben con *line protocol*. Pendiente explícito: generar y enviar *line protocol*. |
| ksqlDB | Insertar en topics | ksqlDB no inserta en topics: hay que hacerlo en un stream. |
| IoTDB | Tablas sin columna `Time` | IoTDB necesita la marca de tiempo para insertar filas. |
| CouchDB | Vistas | No se insertan documentos en una vista. |
| Los sin claves foráneas | **De la tabla referenciada** y las claves foráneas automáticas | El motor no tiene claves foráneas que leer; las columnas se llenan con el generador por nombre o tipo. |
| Cualquiera | Unicidad frente a las filas existentes y claves únicas distintas de la primaria | El generador solo verifica la clave primaria de una columna, y solo contra lo que generó él. Pendiente explícito: leer las restricciones `UNIQUE` y las filas existentes. |
| Cualquiera | Deshacer una generación cortada | Se inserta de a 500 filas sin una transacción global: los lotes anteriores al error quedan. |

## Propiedades de la base

**Propiedades…** ([`propiedades-de-la-base.md`](propiedades-de-la-base.md)) aparece en los motores con la capacidad `database_properties`. Cada driver (`properties.rs`) informa lo suyo y solo ofrece lo que el servidor reporta, así que una opción de una versión más nueva sale solo si existe. Cambiar una propiedad genera el script del motor, que se revisa y se confirma con las advertencias antes de aplicarse; en conexiones de solo lectura solo se ven. Los campos con sugerencias del servidor (collations, ubicaciones…) son los mismos de **Nueva base de datos** (ver [Crear bases: opciones](#crear-bases-opciones)).

Solo datos, sin nada para cambiar:

| Motor | Motivo |
|---|---|
| DuckDB | No guarda ajustes por base: `SET` y `PRAGMA` son de la instancia o de la sesión, y cómo se adjunta una base lo fija el `ATTACH`. |
| Redis, Valkey, Dragonfly | No guardan ajustes por base: `CONFIG SET` cambia todo el servidor. |
| SAP HANA | La "base" es un esquema y HANA no tiene `ALTER SCHEMA`: el propietario se fija al crearlo y los demás ajustes viven en tablas, particiones y columnas. |
| Informix y GBase 8s (ODBC) | El modo de registro se cambia con `ondblog`/`ontape` y un backup de nivel 0, no con SQL. |
| Memgraph | El modo de almacenamiento y el aislamiento tienen sentencias propias fuera de este diálogo. Pendiente explícito: ofrecerlas. |
| Neo4j Community | No tiene `ALTER DATABASE`. |
| libSQL | Solo `user_version`: el servidor maneja el diario (siempre WAL), el tamaño de página y el vacuum, y rechaza esos `PRAGMA` y `VACUUM`. |
| FerretDB, Amazon DocumentDB | FerretDB no tiene el comando `profile`; en DocumentDB el *profiler* se define en el grupo de parámetros del clúster y escribe en CloudWatch Logs. |
| Dremio (espacios y hogares) | Solo tienen nombre; los orígenes sí tienen políticas de actualización. |
| Spanner con dialecto PostgreSQL | Se muestran pero no se cambian: el driver habla GoogleSQL. |
| Cosmos DB sin rendimiento propio | Con rendimiento por contenedor o *serverless* no hay oferta de la base que cambiar. |

Con propiedades que no se ofrecen aunque se muestren: Firebird (solo lectura, escrituras forzadas e intervalo de *sweep* van por el API de servicios, que el cliente usado no tiene; el cifrado necesita un plugin y una clave que una conexión no ve), Athena (su DDL no cambia la descripción ni la ubicación, ni quita propiedades), Databricks (raíz de almacenamiento, aislamiento y tipo del catálogo no cambian por SQL), TDengine (`MAXROWS` y `KEEP_TIME_OFFSET`: 3.3 rechaza el primero y toma el segundo sin aplicarlo) y Couchbase (tipo, motor de almacenamiento y resolución de conflictos no se editan después de crear el *bucket*; la compactación automática y el cifrado en reposo quedan fuera).

Sin la función:

| Motor | Qué falta | Motivo |
|---|---|---|
| Amazon Neptune | Todo | Los ajustes viven en el grupo de parámetros del clúster (API de AWS), no detrás de openCypher. |
| Databend | Todo | Su `ALTER DATABASE` solo renombra. |
| Manticore | Todo | No tiene bases. |
| Denodo, CrateDB, H2 | Todo | No tienen bases que DBine administre. |
| Apache Drill, DynamoDB, Aurora DSQL, Elasticsearch, OpenSearch, etcd, Flight SQL, ksqlDB, Phoenix, Solr, Trino | Todo | Pendiente explícito: no tienen `properties.rs`, y el código no registra si el motor tiene propiedades por base. |

**Probado:** no encontré pruebas de esta función contra servidores reales en los `properties.rs` revisados, salvo las unitarias que traen algunos drivers; lo implementado a partir de la documentación del fabricante no se distingue en el código.

## Renombrar con impacto

**Renombrar…** ([`renombrar.md`](renombrar.md)) cambia el nombre de un objeto
y vuelve a crear el código que lo nombra, en un solo script. El contrato está
en `crates/dbine-driver/src/rename.rs` (`Driver::rename_spec` y
`Driver::rename_script`); la búsqueda y la reescritura de los dependientes son
comunes a todos los motores. Cada driver completa su fila cuando implementa la
función y la prueba contra un servidor real; hasta entonces el explorador no
ofrece **Renombrar…** en ese motor.

| Motor | Estado | Qué renombra | Dependientes que el motor actualiza solo | Cómo se reponen los reescritos | Límites |
|---|---|---|---|---|---|
| PostgreSQL (y Aurora, AlloyDB, Cloud SQL, Timescale, Yugabyte, Greenplum, Cloudberry, Greengage, EDB, KingbaseES, Fujitsu, openGauss) | sí | tabla, vista, vista materializada, secuencia, tipo, dominio, función, procedimiento, trigger, columna, índice, restricción, esquema (`ALTER … RENAME TO`, `RENAME COLUMN`, `RENAME CONSTRAINT`, `ALTER FUNCTION f(args) RENAME TO` una vez por sobrecarga, `ALTER TRIGGER t ON tabla RENAME TO`) | vistas, vistas materializadas, triggers, claves foráneas, índices, funciones `BEGIN ATOMIC` | funciones y procedimientos plpgsql/sql: `CREATE OR REPLACE`; en una transacción (YugabyteDB: sin transacción) | el `EXECUTE` dinámico queda manual; los índices, restricciones y secuencias propios de la tabla conservan su nombre; los `search_path` que nombran el esquema no se actualizan |
| CockroachDB | sí | lo mismo que PostgreSQL (índice: `ALTER INDEX tabla@índice`) | claves foráneas e índices | vistas y funciones que usan el objeto: se borran antes y se crean después (pierden los permisos) | el DDL se confirma sentencia por sentencia (`autocommit_before_ddl`): sin transacción; una función de trigger que nombra la tabla impide renombrarla |
| Amazon Redshift, Yellowbrick | sí (sin prueba en vivo) | tabla, vista (Redshift: con `ALTER TABLE`), columna, esquema | claves foráneas | vistas: `CREATE OR REPLACE` | sin índices, restricciones ni rutinas; sin transacción |
| Materialize, RisingWave | sí | tabla, vista, vista materializada, source, sink, índice, esquema | vistas, vistas materializadas y sinks | — | sin columnas; sin transacción |
| CrateDB | sí | tabla, vista (`ALTER TABLE … RENAME TO`), columna (5.5+) | — | vistas: `CREATE OR REPLACE` | sin esquemas, índices ni restricciones; sin transacción |
| H2 (servidor PostgreSQL) | sí | tabla, vista, columna, índice, restricción, esquema | — | vistas: `CREATE OR REPLACE` | sin transacción |
| Denodo | no | — | — | — | no modifica objetos por SQL: las vistas se definen en Denodo |
| Amazon Aurora DSQL | sí (sin prueba en vivo) | tablas, vistas y secuencias (`ALTER TABLE\|VIEW\|SEQUENCE … RENAME TO`), funciones (`ALTER FUNCTION f(args) RENAME TO`, cada sobrecarga), columnas de tablas y vistas (`RENAME COLUMN`) y restricciones (`RENAME CONSTRAINT`) | vistas | `CREATE OR REPLACE` (funciones SQL) | no renombra índices, esquemas ni dominios (no tiene `ALTER INDEX`, `ALTER SCHEMA` ni `ALTER DOMAIN`); cada sentencia DDL va en su propia transacción: no es atómico |
| SQL Server, Azure SQL | sí | tabla, vista, procedimiento, función, trigger, columna, índice, restricción (`sp_rename`; un módulo se renombra con `sp_rename` y luego `CREATE OR ALTER` con el encabezado nuevo, porque `sp_rename` no cambia el texto guardado) | claves foráneas, índices y restricciones del objeto renombrado | `CREATE OR ALTER` (mantiene permisos); las vistas con SCHEMABINDING se borran antes y se crean después; en una transacción | sin esquemas ni sinónimos; una columna que usan restricciones CHECK o índices filtrados se renombra borrándolos y creándolos de nuevo en el mismo lote; si la usa una columna calculada, no se renombra |
| Microsoft Fabric Data Warehouse | sí (sin prueba en vivo) | tabla, columna (`sp_rename`) | — | `CREATE OR ALTER`; sin transacción | sin vistas, rutinas, índices ni restricciones |
| Babelfish for PostgreSQL | sí | tabla, vista, procedimiento, función, columna (`sp_rename`; procedimientos y funciones se borran y se crean con el nombre nuevo) | restricciones CHECK y columnas calculadas al renombrar una columna | se borran antes y se crean después (pierden los permisos); en una transacción | sin triggers, restricciones, índices, esquemas ni sinónimos |
| MySQL, Aurora MySQL, Cloud SQL para MySQL | sí | tabla y vista (`RENAME TABLE`), columna (`CHANGE COLUMN` con la definición completa: sirve en todas las versiones), índice (`RENAME INDEX`, MySQL 5.7+) | claves foráneas e índices; los checks sobre la columna se borran y se vuelven a agregar con el nombre nuevo en el mismo `ALTER TABLE` | vistas, rutinas y triggers: se borran y se vuelven a crear (pierden los permisos) | sin bases ni restricciones; DDL sin transacción; un `DEFINER` ajeno exige `SET_USER_ID` (`SET_ANY_DEFINER` desde 8.2) o `SUPER`; la intercalación propia de una columna de texto hay que agregarla a mano |
| MariaDB | sí | tabla y vista (`RENAME TABLE`), columna (`CHANGE COLUMN` con la definición completa), índice (`RENAME INDEX`, 10.5+) | claves foráneas, índices y checks | vistas, rutinas y triggers: `CREATE OR REPLACE` | sin bases ni restricciones; DDL sin transacción; un `DEFINER` ajeno exige `SET USER` o `SUPER`; la intercalación propia de una columna de texto hay que agregarla a mano |
| TiDB | sí | tabla y vista (`RENAME TABLE`), columna (`CHANGE COLUMN`), índice (`RENAME INDEX`) | claves foráneas e índices; los checks sobre la columna se borran y se vuelven a agregar | vistas: `CREATE OR REPLACE` | sin bases ni restricciones; DDL sin transacción |
| StarRocks, Apache Doris, VeloDB, GreptimeDB | sí | tabla (`ALTER TABLE … RENAME`) | — | vistas: se borran y se vuelven a crear | sin vistas, columnas ni índices |
| SingleStore, Databend, OceanBase (MySQL) | sí (sin prueba en vivo) | tabla (SingleStore: `ALTER TABLE … RENAME TO`; Databend y OceanBase: `RENAME TABLE`) | — | vistas y rutinas: se borran y se vuelven a crear | sin vistas, columnas ni índices |
| Manticore Search | no | — | — | — | el motor no renombra tablas desde SQL |
| Oracle, Oracle Autonomous Database | sí | tabla, vista, columna, índice, restricción, secuencia, sinónimo privado y trigger con el `RENAME` del motor (vistas, secuencias y sinónimos: solo conectado como el dueño del esquema); procedimientos, funciones y paquetes por recreación (`CREATE` con el nombre nuevo y `DROP` del viejo) | claves foráneas, índices y checks; las vistas y el código quedan `INVALID` | `CREATE OR REPLACE [FORCE]`: quedan `VALID` sin recompilar; lo que depende de ellos se recompila solo al usarse | el DDL confirma solo (sin transacción); no renombra usuarios (esquemas), sinónimos públicos, vistas materializadas ni tipos; recrear una rutina pierde sus permisos; los hints en comentarios no se tocan |
| SQLite | sí | tablas y tablas virtuales (`ALTER TABLE … RENAME TO`), columnas (`ALTER TABLE … RENAME COLUMN`, 3.25+), vistas, triggers e índices (se crean con el nombre nuevo y se borra el anterior) | al renombrar una tabla o una columna: vistas, triggers, índices, CHECK y claves foráneas (con `legacy_alter_table` en OFF; el script lo apaga antes) | borrar y crear, en una transacción; al renombrar una vista, las vistas y triggers que la usan se reescriben | falla si ya hay una vista rota en la base; no renombra restricciones, bases adjuntas ni los índices automáticos de PRIMARY KEY/UNIQUE |
| libSQL / Turso | sí | igual que SQLite (`ALTER TABLE … RENAME TO / RENAME COLUMN`; vistas, triggers e índices por recreación) | al renombrar una tabla o una columna: vistas, triggers, índices, CHECK y claves foráneas | borrar y crear, en una transacción (servidores Hrana 3); al renombrar una vista, las vistas y triggers que la usan se reescriben | como SQLite; el servidor rechaza `PRAGMA legacy_alter_table` y el script no lo envía |
| DuckDB | sí | tablas (`ALTER TABLE … RENAME TO`), vistas (`ALTER VIEW … RENAME TO`), columnas (`ALTER TABLE … RENAME COLUMN`) | índices y CHECK; las vistas y macros no | `CREATE OR REPLACE` después del cambio, en una transacción | rechaza renombrar una tabla con índices o referenciada por una clave foránea, y una columna indexada o en una clave foránea; no renombra índices, secuencias, macros, tipos ni esquemas |
| Archivos CSV / Parquet / JSON | no | — | — | — | las vistas se regeneran desde los archivos de la carpeta en cada conexión: se renombra el archivo |
| MongoDB | sí | colecciones (`renameCollection`), vistas (se borran y se crean con `db.createView`), campos (`updateMany` con `$rename`) | — | vistas que dependen: se reescriben `viewOn`, `$lookup.from`, `$graphLookup.from`, `$unionWith.coll`, `$out` y `$merge`, y se borran y se crean (no guardan datos ni pierden permisos: los roles los otorgan por nombre); al renombrar un campo, los índices que lo usan se borran y se crean con el campo nuevo y el validador se cambia con `collMod` | no es transaccional; renombrar un campo reescribe cada documento que lo tiene; no se renombran `_id`, las colecciones de series temporales ni su campo de tiempo o de metadatos; los campos que usan las vistas se listan y no se reescriben; los roles con privilegios sobre el nombre viejo no se actualizan |
| FerretDB | sí | colecciones (`renameCollection`), campos (`updateMany` con `$rename`) | — | al renombrar un campo, los índices que lo usan se borran y se crean con el campo nuevo | no tiene vistas; no guarda validadores; no es transaccional; renombrar un campo reescribe cada documento que lo tiene |
| Amazon DocumentDB | sí (sin prueba en vivo) | colecciones (`renameCollection`), campos (`updateMany` con `$rename`) | — | al renombrar un campo, los índices que lo usan se borran y se crean con el campo nuevo; el validador se cambia con `collMod` | no tiene vistas; no es transaccional; renombrar un campo reescribe cada documento que lo tiene |
| Azure Cosmos DB | no | — | — | — | no renombra bases ni contenedores; un campo no se puede renombrar del lado del servidor (el `UPDATE` va documento por documento, por `id`, y no borra campos) |
| Couchbase | sí | campos (`UPDATE ks SET b = a UNSET a WHERE a IS NOT MISSING`) | — | los índices GSI que usan el campo se borran después del `UPDATE` y se crean con el campo nuevo; las funciones SQL++ se listan y no se reescriben | no renombra buckets, scopes ni colecciones; hace falta un índice que sirva al `WHERE` (el del campo o el primario); no es atómico |
| CouchDB | no | — | — | — | no renombra bases; un campo solo se podría renombrar reescribiendo desde el cliente cada documento con su `_rev` |
| ClickHouse | sí | tablas, vistas, vistas materializadas, diccionarios (`RENAME TABLE\|DICTIONARY`), columnas (`ALTER TABLE … RENAME COLUMN`) y bases de datos con motor Atomic (`RENAME DATABASE`, desde el nodo de la base) | índices de salto y CHECK de la tabla | `CREATE OR REPLACE`; una vista materializada sin `TO` queda destildada porque se reemplaza vacía | no renombra columnas de claves ni las que lee una vista materializada (lo frena una guarda en el servidor); no detecta tablas `Distributed` ni diccionarios que la leen; no es transaccional |
| Timeplus Proton | sí | streams, vistas, vistas materializadas (`RENAME STREAM`) y columnas (`ALTER STREAM … RENAME COLUMN`) | — | se borran antes y se crean después; una vista materializada creada de nuevo pierde lo que guardaba | no renombra bases de datos ni columnas de la clave del stream; no detecta streams externos ni diccionarios; no es transaccional |
| Cassandra, ScyllaDB | sí | columnas de la clave primaria (`ALTER TABLE ks.t RENAME a TO b`) | — | — | no renombra columnas comunes, tablas, keyspaces, tipos ni funciones; el servidor rechaza una columna con índice secundario (DBine lo avisa antes) o usada por una vista materializada |
| Amazon Keyspaces | no | — | — | — | su `ALTER TABLE` no tiene `RENAME` |
| Elasticsearch, OpenSearch, Open Distro | sí | índice por copia (`PUT /old/_block/write` → `POST /old/_clone/new` → espera a la copia → `DELETE /old`, o `_aliases` con `remove_index`); alias (`_aliases` remove+add atómico) | alias del índice: pasan al nuevo en el mismo paso atómico | — | el clon copia los datos (tarda, ocupa disco) y el índice no acepta escrituras mientras dura; los campos no se renombran (exige reindexar); los data streams no se renombran |
| Apache Solr | sí | core en modo standalone (CoreAdmin `RENAME`) | — | — | la carpeta del core conserva el nombre anterior; en SolrCloud no se renombra (`RENAME` solo agrega un alias) y el servidor lo rechaza |
| Snowflake | sí (sin prueba en vivo) | tablas, vistas, vistas materializadas, secuencias (`ALTER … RENAME TO`), funciones y procedimientos (`ALTER FUNCTION\|PROCEDURE f(tipos) RENAME TO`, cada sobrecarga), columnas (`ALTER TABLE … RENAME COLUMN`), esquemas (`ALTER SCHEMA … RENAME TO`) | claves foráneas | `CREATE OR REPLACE` (pierde los permisos: no se agrega `COPY GRANTS`) | sin índices ni restricciones; las bases de datos todavía no se renombran (pendiente: el explorador no ofrece renombrar la base en motores con esquemas); el DDL se confirma sentencia por sentencia |
| BigQuery | sí (sin prueba en vivo) | tablas (`ALTER TABLE … RENAME TO`), columnas (`ALTER TABLE … RENAME COLUMN`) | — | `CREATE OR REPLACE` | sin vistas, rutinas ni datasets; no renombra columnas de partición, de clustering, de claves ni campos de STRUCT; se pierden los índices de búsqueda y vectoriales; con streaming activo no se puede; las referencias escritas `proyecto.dataset.tabla` entre un solo par de comillas invertidas no se detectan |
| Databricks | sí (sin prueba en vivo) | tablas (`ALTER TABLE … RENAME TO`), vistas (`ALTER VIEW … RENAME TO`), columnas (`ALTER TABLE … RENAME COLUMN`, con column mapping) | — | `CREATE OR REPLACE` | columnas solo en tablas Delta con `delta.columnMapping.mode` = `name` o `id`; sin esquemas, funciones ni vistas materializadas; con AWS Glue como metastore no hay `RENAME` |
| Trino, Starburst | sí | tablas, vistas y vistas materializadas (`ALTER TABLE\|VIEW\|MATERIALIZED VIEW … RENAME TO`), columnas (`ALTER TABLE … RENAME COLUMN`) y esquemas (`ALTER SCHEMA … RENAME TO`) | — | `CREATE OR REPLACE`, después de un `USE` del esquema del objeto | lo que acepta lo decide el conector (memory e Iceberg renombran todo; Hive no renombra algunas cosas); los nombres se guardan en minúsculas, así que se rechazan mayúsculas; no es transaccional; al renombrar un esquema, las vistas de adentro que nombran tablas sin calificar dejan de funcionar (se avisa) |
| Presto | sí | tablas (`ALTER TABLE … RENAME TO`), vistas (`ALTER VIEW … RENAME TO`), columnas (`RENAME COLUMN`) y esquemas (`ALTER SCHEMA … RENAME TO`) | — | `CREATE OR REPLACE`, después de un `USE` del esquema | sin vistas materializadas; el conector memory no renombra columnas ni esquemas, y versiones viejas no tienen `ALTER VIEW … RENAME`; lo demás, igual que Trino |
| Amazon Athena | sí (sin prueba en vivo) | tablas Iceberg (`ALTER TABLE … RENAME TO`), columnas de tablas Iceberg (`ALTER TABLE … CHANGE COLUMN`) y vistas (se crean con el nombre nuevo y se borra la anterior) | — | `CREATE OR REPLACE VIEW` | las tablas externas (Hive) no se renombran: el servidor rechaza la tabla y DBine rechaza sus columnas, porque en Parquet u ORC dejarían de leer datos; nombres solo `[a-z0-9_]`; la vista renombrada pierde sus permisos de Lake Formation; no es transaccional |
| Dremio | sí | columnas de tablas Iceberg (`ALTER TABLE … CHANGE COLUMN`) y vistas (se crean con el nombre nuevo y se borra la anterior) | — | `CREATE OR REPLACE VIEW` | no tiene `RENAME`: tablas, espacios y carpetas no se renombran; columnas de tipo compuesto, no; la vista renombrada pierde reflexiones, wiki, etiquetas y permisos; los dependientes se buscan solo en el mismo espacio u origen y sus carpetas |
| Apache Drill | sí | vistas (se crean con el nombre nuevo y se borra la anterior) | — | `CREATE OR REPLACE VIEW` | tablas (archivos), columnas y espacios de trabajo no se renombran |
| Apache Arrow Flight SQL | no | — | — | — | protocolo genérico: el DDL depende del backend y no hay uno portable |
| Google Cloud Spanner | sí | tablas (`ALTER TABLE … RENAME TO`) y vistas (se crean con el nombre nuevo y se borra la anterior) | índices, claves foráneas, tablas intercaladas y change streams | se borran antes del cambio y se vuelven a crear después (Spanner no renombra una tabla que usa una vista); las vistas que leen una vista borrada, en cadena, se borran antes y se vuelven a crear sin cambios | no renombra columnas, índices, secuencias, restricciones ni esquemas; no es atómico; las claves foráneas no se probaron en vivo (el emulador no renombra tablas con claves foráneas) |
| SAP HANA | sí (sin prueba en vivo) | tabla, columna, índice (`RENAME TABLE s.t TO n`, `RENAME COLUMN s.t.c TO n`, `RENAME INDEX s.ix TO n`) | índices y claves foráneas (verificar) | `CREATE OR REPLACE` (vistas, procedimientos, funciones) | sin vistas, rutinas, restricciones ni esquemas; DDL sin transacción; en versiones sin `CREATE OR REPLACE` ese paso falla |
| Firebird | sí | columna (`ALTER TABLE t ALTER COLUMN a TO b`) | índices comunes | se borran antes y se crean después con `CREATE OR ALTER`; sin transacción (el borrado recién vale al confirmar) | rechaza si la columna está en una vista, rutina, trigger, CHECK o clave primaria/única/foránea; los triggers con `NEW.columna` quedan manuales |
| ODBC: Db2 (LUW) | sí (sin prueba en vivo) | tabla, columna, índice (`RENAME TABLE\|INDEX`, `ALTER TABLE … RENAME COLUMN`) | índices | `CREATE OR REPLACE`; en una transacción | rechaza tablas con triggers o en claves foráneas |
| ODBC: Db2 for z/OS | sí (sin prueba en vivo) | tabla, columna, índice (`RENAME TABLE\|INDEX`, `ALTER TABLE … RENAME COLUMN`) | índices | borrar y crear | rechaza tablas con triggers o leídas por vistas no reescritas |
| ODBC: Db2 for i | sí (sin prueba en vivo) | tabla, vista, índice (`RENAME TABLE\|INDEX`) | — | borrar y crear | sin columnas |
| ODBC: Sybase ASE | sí (sin prueba en vivo) | tabla, vista, columna, índice (`sp_rename`) | claves e índices | borrar y crear | solo objetos del usuario conectado |
| ODBC: Informix | sí (sin prueba en vivo) | tabla, columna, índice (`RENAME TABLE\|COLUMN\|INDEX`) | vistas | borrar y crear (triggers y SPL) | — |
| ODBC: Teradata | sí (sin prueba en vivo) | tabla, vista (`RENAME TABLE\|VIEW db.x TO db.n`) | — | borrar y crear | sin columnas |
| ODBC: Vertica | sí (sin prueba en vivo) | tabla, vista, columna, esquema (`ALTER TABLE\|VIEW\|SCHEMA … RENAME TO`, `RENAME COLUMN`) | — | `CREATE OR REPLACE` | DDL sin transacción |
| ODBC: Exasol | sí (sin prueba en vivo) | tabla, vista, columna, esquema (`RENAME TABLE\|VIEW\|SCHEMA`, `ALTER TABLE … RENAME COLUMN`) | — | `CREATE OR REPLACE` | — |
| ODBC: resto de los presets | no | — | — | — | sintaxis sin confirmar |
| Apache Phoenix | no | — | — | — | Phoenix no tiene `RENAME` de tablas ni de columnas |
| ksqlDB | no | — | — | — | no renombra streams, tablas ni columnas (`ALTER STREAM/TABLE` solo agrega columnas) |
| Redis, Valkey, Dragonfly | sí | claves (`RENAMENX` dentro de un `EVAL`: si la clave nueva ya existe, falla sin pisarla) | — | — (el servidor no guarda nada que nombre una clave) | conserva valor y TTL; en Redis Cluster las dos claves tienen que caer en el mismo hash slot (se avisa si no; usar la misma etiqueta `{…}`) |
| etcd | no | — | — | — | el lenguaje de scripts (etcdctl) no tiene `txn`: no hay forma atómica de poner la clave nueva y borrar la vieja sin pisar una existente |
| Amazon DynamoDB | no | — | — | — | el servicio no renombra tablas |
| InfluxDB (v1, v2, v3) | no | — | — | — | InfluxQL no renombra bases, políticas de retención ni measurements; en v2 la API renombra un bucket, pero el script de la sesión es Flux y no puede expresarlo; InfluxDB 3 Core no renombra bases ni tablas |
| Apache IoTDB, TimechoDB | no | — | — | — | el modelo de árbol no renombra series, dispositivos ni bases (`ALTER TIMESERIES … RENAME` solo cambia claves de tags); el modelo de tabla de 2.x rechaza renombrar tablas y columnas |
| TDengine | sí | columnas de tablas comunes (`ALTER TABLE … RENAME COLUMN a b`, la de marca de tiempo incluida) y tags de supertablas (`ALTER STABLE … RENAME TAG a b`) | el índice del tag | los streams que usan el nombre se borran antes y se vuelven a crear después, reescritos, sobre la misma tabla de salida (así pasa también un tag que usa un stream) | no renombra tablas, supertablas, columnas de supertablas, subtablas, vistas, streams, tópicos ni bases; el servidor rechaza una columna o un tag que usa un tópico; lo que llega mientras el stream está borrado no se procesa |
| OrientDB | sí | clases de vértices, aristas y documentos (`ALTER CLASS … NAME`, `UNSAFE` en aristas) y propiedades (`ALTER PROPERTY … NAME` + `UPDATE … SET nuevo = viejo REMOVE viejo`) | — | se listan, no se reescriben (funciones) | sus índices se borran y se recrean (conservan el nombre), leídos de la estructura de la clase; en aristas se mueven los campos `out_`/`in_` de los vértices; no es atómico; no renombra índices, funciones ni secuencias, ni V/E, `out`/`in`, ni atributos `@` |
| Neo4j, Memgraph, Amazon Neptune | no se ofrece | — | — | — | Una etiqueta o un tipo de relación no se renombra: cambiarlo es `SET n:Nueva REMOVE n:Vieja` sobre cada nodo (o recrear cada relación), que reescribe los datos, puede tardar horas en un grafo grande, no es atómico fuera de una transacción del tamaño del grafo y obliga a recrear los índices y las restricciones de la etiqueta. Las consultas guardadas fuera de la base tampoco se ven. |


## Filas aproximadas y comentarios

[Documentar la base](documentar-la-base.md) muestra, debajo de cada tabla,
**Filas (aproximadas)**, y los comentarios de vistas, rutinas, triggers,
secuencias y tipos. Los trae `Session::row_estimates` y
`Session::object_comments` (`crates/dbine-driver/src/stats.rs`); por defecto
devuelven vacío. Las filas salen **solo de estadísticas que el motor ya
guarda**, nunca de un `COUNT`: no recorre ni bloquea. El número puede estar
desactualizado hasta que el motor refresque sus estadísticas. Una tabla sin
estadísticas queda sin cifra.

Lo implementan todos los drivers menos Spanner, IoTDB, ksqlDB e InfluxDB 1/2
(sus motivos están en la tabla).

**Probado contra servidores reales:** PostgreSQL 16 y CockroachDB; SQL Server
2022 (incluido el preset ODBC genérico sobre ODBC Driver 18); Oracle 23;
Firebird; MySQL y MariaDB; ClickHouse y Timeplus; BigQuery (emulador); Trino
(conector `memory`); Dremio, Drill, DSQL (como PostgreSQL común), Flight SQL
(GizmoSQL) y Phoenix; SQLite y DuckDB (archivos); libSQL, TDengine e InfluxDB 3;
MongoDB, FerretDB, CouchDB, Elasticsearch y Redis. **Todo lo demás solo tiene
pruebas unitarias** y sigue la documentación del fabricante.

| Driver | Filas (fuente) | Comentarios (fuente) |
|---|---|---|
| `postgres` (PostgreSQL y familia) | `pg_class.reltuples` de tablas, particionadas y vistas materializadas (sin `ANALYZE`: -1, o 0 sin páginas antes de PG 14, y se omite) | `obj_description` de vistas, vistas materializadas, secuencias, funciones, procedimientos, triggers y tipos |
| `postgres`: CockroachDB | `estimated_row_count` de `SHOW TABLES` (estadísticas de tabla; `reltuples` viene siempre nulo); 0 solo si `SHOW STATISTICS` tiene la tabla | como PostgreSQL |
| `postgres`: Redshift | `svv_table_info.tbl_rows`, luego `pg_class` | como PostgreSQL |
| `postgres`: Yellowbrick | contadores de `sys.table`, luego `pg_class` | como PostgreSQL |
| `postgres`: CrateDB | documentos de los shards primarios (`sys.shards`) | ninguna: CrateDB no guarda comentarios |
| `postgres`: RisingWave | claves del estado de cada tabla y vista materializada (`rw_catalog.rw_table_stats`) | como PostgreSQL |
| `postgres`: H2 | `information_schema.tables.row_count_estimate` | `REMARKS` |
| `postgres`: Materialize | ninguna: guarda tamaños, no filas | `mz_internal.mz_comments` |
| `postgres`: Denodo | ninguna: sus vistas leen las fuentes en vivo | descripción de las vistas |
| `dsql` | `pg_class.reltuples` (DSQL corre `ANALYZE` solo; -1 se omite) | `obj_description` de vistas, secuencias y funciones; se salta lo que DSQL rechaza leer |
| `mysql` (MySQL, MariaDB, TiDB, OceanBase, StarRocks, Doris…) | `information_schema.TABLES.TABLE_ROWS`, la estimación del motor de almacenamiento (en StarRocks y Doris, los informes de tablets); Databend: `system.tables.num_rows`; Manticore: `SHOW TABLE … STATUS` (`indexed_documents`), una tabla por vez | `ROUTINE_COMMENT` de procedimientos y funciones; `TABLE_COMMENT` de secuencias de MariaDB y de vistas (y vistas materializadas de StarRocks) cuando el motor guarda uno. Los triggers no tienen comentarios |
| `sqlserver` (y Azure SQL, Fabric) | `sys.dm_db_partition_stats` (necesita `VIEW DATABASE STATE`); sin ese permiso, `sys.partitions.rows` | propiedad extendida `MS_Description` de vistas, procedimientos, funciones, triggers, secuencias, sinónimos y tipos. Fabric no tiene propiedades extendidas |
| `oracle` | `ALL_TABLES.NUM_ROWS` (estadísticas de `DBMS_STATS`; NULL se omite); vistas materializadas bajo el nombre de la vista | `ALL_TAB_COMMENTS` (vistas) y `ALL_MVIEW_COMMENTS`; Oracle no guarda comentarios de unidades PL/SQL, secuencias ni sinónimos |
| `hana` | `M_TABLES.RECORD_COUNT`; sin acceso, `M_CS_TABLES.RECORD_COUNT` (solo tablas columnares) | `SYS.VIEWS.COMMENTS` (vistas); HANA no guarda comentarios de procedimientos, funciones, triggers ni secuencias |
| `firebird` | Firebird no guarda un conteo: se estima como `1 / selectividad` de la estadística del índice único (`RDB$INDICES.RDB$STATISTICS`), prefiriendo la clave primaria. Sin índice único, o con estadística nunca calculada (0), se omite. Se actualiza con `SET STATISTICS` | `RDB$DESCRIPTION` de vistas, procedimientos, funciones, paquetes, triggers, secuencias y dominios |
| `odbc`: Db2 (LUW) | `SYSCAT.TABLES.CARD` (`RUNSTATS`; -1 = nunca) | `REMARKS` |
| `odbc`: Db2 for z/OS | `SYSIBM.SYSTABLES.CARDF` (`RUNSTATS`) | `REMARKS` |
| `odbc`: Db2 for i | `QSYS2.SYSTABLESTAT.NUMBER_ROWS` | `LONG_COMMENT` |
| `odbc`: Sybase ASE | `row_count()` (`systabstats`) | solo tablas y columnas |
| `odbc`: SQL Anywhere | `SYS.SYSTAB.count` (se actualiza en cada checkpoint) | `remarks` |
| `odbc`: Informix, GBase 8s | `systables.nrows` de las tablas que vio `UPDATE STATISTICS` | solo tablas y columnas |
| `odbc`: Teradata | `DBC.StatsV.RowCount` (`COLLECT STATISTICS`) | `CommentString` |
| `odbc`: Vertica | `v_monitor.projection_storage.row_count`, la proyección más grande | `v_catalog.comments` |
| `odbc`: Exasol | `EXA_ALL_TABLES.TABLE_ROW_COUNT` | columnas `*_COMMENT` |
| `odbc`: Netezza | `_V_TABLE.RELTUPLES` | `DESCRIPTION` |
| `odbc`: Dameng | `ALL_TABLES.NUM_ROWS` (`DBMS_STATS`) | `ALL_TAB_COMMENTS` |
| `odbc`: MonetDB | `sys.tablestorage.rowcount` | `sys.comments` |
| `odbc`: Ingres | `iitables.num_rows` | solo tablas y columnas |
| `odbc`: SQream | `sqream_catalog.tables.row_count` | ninguna |
| `odbc`: SQL Server (preset genérico) | `sys.partitions.rows` | `MS_Description` |
| `odbc`: Hive, Impala, Spark, Kyuubi, Cloudera | ninguna: las estadísticas están en el metastore y piden una llamada `DESCRIBE`/`SHOW TABLE STATS` por tabla. Pendiente explícito: decisión del dueño | ninguna |
| `odbc`: MaxDB | ninguna: no se pudo confirmar que `SYSINFO.TABLESIZE` no recorra datos, así que se excluye por la regla de no escanear | ninguna |
| `odbc`: IRIS, Caché, OpenEdge, Mimer y los presets menores | ninguna: no se conoce una fuente de catálogo confiable | ninguna |
| `sqlite`, `libsql` | `sqlite_stat1`, solo si existe (lo crea `ANALYZE`); sin esa tabla no hay estadística y la respuesta es vacía. Nunca se ejecuta `ANALYZE`, porque escribe en el archivo | ninguna: SQLite no tiene comentarios en los objetos |
| `duckdb` | `duckdb_tables().estimated_size` (metadatos de almacenamiento); las tablas de catálogos adjuntos de otros motores no tienen | `COMMENT ON` de vistas, macros, secuencias y tipos |
| `clickhouse` (y Timeplus Proton) | `system.tables.total_rows`; NULL (Log, tablas externas, vistas) se omite | `COMMENT` de vistas, vistas materializadas y diccionarios |
| `trino` (y Presto) | `SHOW STATS FOR` por tabla, `row_count` de la fila resumen: pregunta al conector las estadísticas que guarda (metastore de Hive, resumen de snapshot de Iceberg, log de Delta…). Máximo `MAX_TABLES` tablas; sin estadísticas del conector se omite | `system.metadata.table_comments` (vistas) y `system.metadata.materialized_views`; Trino no tiene comentarios de funciones |
| `athena` | `numRows` (Hive/Spark `ANALYZE`) o `recordCount` (crawler de Glue) en los parámetros de la tabla del catálogo, con `ListTableMetadata`: gratis, no factura ni lee S3. El `ANALYZE` de Athena solo guarda estadísticas de columnas | parámetro `comment` de las vistas; Athena escribe ahí «Presto View» (se descarta) y no tiene `COMMENT` para vistas, así que solo salen las creadas desde Hive o Spark |
| `bigquery` | `numRows` de `tables.get` (REST, sin job de consulta, sin costo) para tablas, snapshots, clones y vistas materializadas; las vistas y las tablas externas no tienen | `description` de vistas, vistas materializadas, funciones y procedimientos. Cada objeto se lee por separado, hasta `MAX_OBJECTS` |
| `databricks` | propiedad `spark.sql.statistics.numRows` por la API REST de Unity Catalog (no despierta ningún warehouse); la deja `ANALYZE TABLE … COMPUTE STATISTICS` o la optimización predictiva. Sin `ANALYZE`, se omite. El catálogo `hive_metastore` no está en la API: vacío | `comment` de vistas, vistas materializadas y funciones |
| `snowflake` | columna `rows` de `SHOW TABLES` y `SHOW MATERIALIZED VIEWS` (metadatos de micro-particiones; sin warehouse, no factura). Las tablas externas no tienen | vistas, vistas materializadas, funciones, procedimientos, secuencias, streams y tareas; el texto fijo de `description` cuando no hay comentario se descarta |
| `dremio` | ninguna: ni `INFORMATION_SCHEMA` ni la API del catálogo traen conteos, y contar correría un job que lee la fuente | descripción (wiki) de las vistas por la API del catálogo, hasta `MAX_VIEWS`; sin jobs |
| `drill` | `NUM_ROWS` de `INFORMATION_SCHEMA.TABLES`, que viene del metastore de Drill una vez corrido `ANALYZE TABLE … REFRESH METADATA`; sin metastore es NULL y se omite; antes de Drill 1.17 no existe la columna | ninguna: Drill no tiene `COMMENT` |
| `flightsql` | solo si el servidor es DuckDB (GizmoSQL): `duckdb_tables().estimated_size`; Dremio, DataFusion y otros: ninguna, porque los comandos de metadatos de Flight SQL (`GetTables`) no traen conteos | solo con DuckDB (GizmoSQL): `comment` de `duckdb_views()`; Flight SQL no lista funciones ni secuencias |
| `phoenix` | `SYSTEM.STATS` (guide posts de `UPDATE STATISTICS` y compactaciones mayores), suma de `GUIDE_POSTS_ROW_COUNT`; corre corto por las filas después del último guide post (300 MB por defecto); una tabla chica o sin estadísticas se omite; las vistas comparten el almacenamiento y no tienen. Avatica genérico: vacío | ninguna: Phoenix no tiene `COMMENT` |
| `cassandra` (y ScyllaDB) | estimaciones de particiones del nodo (`system.table_estimates` en Cassandra 4.0+, `system.size_estimates` antes y en ScyllaDB), extrapoladas al anillo por la fracción de rangos que cubre el nodo. **Cuentan particiones, no filas CQL**: una tabla con columnas de clustering tiene más filas. Amazon Keyspaces no tiene ninguna de las dos tablas: ninguna | `comment` de tablas (viene con el esquema) y de vistas materializadas (`system_schema.views`); tipos y funciones no tienen |
| `influxdb`: v3 | filas de los archivos Parquet ya persistidos (`system.parquet_files`), sin leer datos; **los puntos que siguen en el WAL (los últimos minutos) todavía no suman**; un token sin acceso a las tablas de sistema no obtiene nada | ninguna: InfluxDB no tiene comentarios |
| `influxdb`: v1 y v2 | ninguna: el motor no guarda estadísticas de filas | ninguna: Flux e InfluxQL no tienen comentarios |
| `tdengine` | ninguna: `SHOW TABLE DISTRIBUTED` las daría sin leer datos, pero cuesta más cuanto más crecen las tablas y queda fuera por la regla de no cargar el servidor. Pendiente explícito: decisión del dueño | `ins_tables.table_comment` de las subtablas (tablas y supertablas ya traen el suyo); vistas, streams y tópicos no tienen |
| `mongodb` (y FerretDB, DocumentDB) | `estimatedDocumentCount` por colección (metadatos, sin recorrer); las vistas y las colecciones que el usuario no puede contar se omiten | ninguna: MongoDB no guarda comentarios en colecciones, vistas ni índices |
| `cosmosdb` | `documentsCount` de `x-ms-resource-usage` (con `x-ms-populatequotainfo`); el servicio lo actualiza cada pocos minutos; -1 mientras no lo sabe. Un `COUNT` leería todos los ítems y costaría RU | ninguna |
| `couchbase` | gauge `kv_collection_item_count` por colección, de la API REST de estadísticas (`/pools/default/stats/range`, Couchbase Server 7.0+), sumado entre nodos; versiones anteriores o sin el privilegio: ninguna | ninguna: no guarda comentarios |
| `couchdb` | `doc_count` de `GET /{db}` para `_all_docs`; las vistas solo informan el tamaño del índice | ninguna |
| `dynamodb` | `ItemCount` de `DescribeTable`, de la tabla y de sus índices secundarios; el servicio lo refresca cada unas seis horas, así que atrasa respecto de las escrituras recientes. Un `Scan` con `COUNT` leería y facturaría toda la tabla | ninguna: las etiquetas no son comentarios |
| `elasticsearch` (y OpenSearch, Open Distro) | `docs.count` de `_cat/indices` (documentos de los primarios; cuenta también los anidados); un data stream suma sus índices de respaldo; los índices cerrados y los alias se omiten | ninguna |
| `solr` | `index.numDocs` de `admin/cores?action=STATUS`. En SolrCloud solo se informa la colección si todos sus shards tienen una réplica en el nodo al que se conecta DBine | ninguna |
| `orientdb` | `records` de cada clase en los metadatos de la base (`GET /database/{db}`); es polimórfico: incluye los registros de las subclases | las clases y sus propiedades ya traen su descripción con el esquema; funciones, secuencias e índices no tienen |
| `neo4j` | nodos por etiqueta y relaciones por tipo, desde el count store (solo las dos formas exactas de `count` que el planificador responde sin tocar datos). Memgraph: `count` de `SHOW INDEX INFO` en índices de solo etiqueta o solo tipo; una etiqueta sin índice no tiene cifra. Neptune: ninguna, porque su resumen de estadísticas da totales del grafo y los nombres, no un conteo por etiqueta | ninguna: Neo4j, Memgraph y Neptune no guardan comentarios |
| `redis` (y Valkey, Dragonfly) | cantidad de claves de la base lógica (`keys=` de `INFO keyspace`), **por base y no por colección**: no aparece bajo ninguna tabla del documento | ninguna |
| `etcd` | gauge `etcd_debugging_mvcc_keys_total` de `/metrics`, de **todo el espacio de claves** (etcd no lleva conteo por prefijo): no aparece bajo ninguna tabla del documento; una conexión limitada a un prefijo, o sin acceso a `/metrics`, no obtiene nada | ninguna |
| `spanner` | ninguna: no guarda conteos de filas (`SPANNER_SYS` solo informa tamaños en bytes) | ninguna: GoogleSQL no tiene `COMMENT` |
| `iotdb` | ninguna: el esquema no guarda conteos por dispositivo, y contar sería un `SELECT COUNT(*)` sobre los TsFiles, que la regla de no escanear prohíbe | ninguna: no tiene comentarios |
| `ksqldb` | ninguna: no expone cantidad de mensajes por stream ni tabla | ninguna: no tiene comentarios |

**Pendiente explícito:** TDengine (filas) y Hive, Impala, Spark, Kyuubi y
Cloudera por ODBC (filas) esperan la decisión del dueño.

## Modificar tablas (diseñador en modo edición)

**Modificar…** abre el diseñador con la tabla tal como está, y arma un solo
script con el `ALTER` del motor (el mismo de **Comparar esquemas**, `Driver::sync_script`).
Cómo se usa: [`comparacion-de-esquemas.md`](comparacion-de-esquemas.md#modificar-una-tabla).
El explorador lo ofrece cuando el driver tiene diseñador (`designer`), responde
`supports_schema_sync`, el tipo del objeto es el del diseñador (tabla, colección,
índice, stream…) y la conexión no es de solo lectura.

Lo tienen todos los motores que cumplen esas dos condiciones, es decir, los de
las tablas "Sincronización" de este documento que también tienen diseñador.
Qué puede cambiar en cada uno es lo que dicen esas tablas: lo que el motor no
aplica con DDL queda como aviso en el script y no se ejecuta (por ejemplo, el
tipo de una columna en Cassandra, o los campos de documentos que ya existen en
MongoDB).

**Probado contra servidores reales:** SQL Server, PostgreSQL, CockroachDB,
MySQL, MariaDB, SQLite, Oracle, ClickHouse, Cassandra y MongoDB. **Todo lo
demás solo tiene pruebas unitarias** y sigue la documentación del fabricante.

El renombre de una columna dentro del diseñador pasa por **Renombrar…**
([`renombrar.md`](renombrar.md)). En los motores cuya `rename_spec` no cubre
columnas, el nombre de una columna existente queda fijo en el diseñador y se
muestra el motivo (ver "Renombrar con impacto"). El nombre y el esquema de la
tabla no se cambian acá: es **Renombrar…**.

| Motor | Qué falta | Motivo |
|---|---|---|
| Denodo | Modificar… | Es una capa virtual: no tiene diseñador (`designer` devuelve `None`) ni DDL de tablas; las vistas se definen en Denodo. |
| Apache Calcite Avatica (Phoenix en modo genérico) | Modificar… | Sin diseñador ni sincronización: el DDL depende de la base detrás del servidor y el driver no sabe cuál es. |
| Amazon Neptune | Modificar… | Sin diseñador ni sincronización: no tiene esquema definido por el usuario, indexa todo solo y no tiene restricciones. |
| NetSuite (ODBC) | Modificar… | SuiteAnalytics Connect es de solo lectura: no tiene diseñador ni sincronización. |
| InfluxDB (v1, v2, v3) | Modificar… | Sin diseñador ni DDL: los measurements y sus campos se crean al escribir datos. |
| Apache Drill | Modificar… | Sin diseñador ni sincronización: sus tablas son archivos creados con `CREATE TABLE AS`, sin columnas que modificar. |
| Apache Arrow Flight SQL | Modificar… | Es un protocolo, no un motor: el DDL depende del backend y no hay uno portable. Conviene conectarse con el driver propio de la base. |
| CouchDB | Modificar… | Tiene sincronización (índices y design docs), pero no diseñador: los documentos no tienen esquema, así que no hay tabla que abrir. |
| Redis, Valkey, Dragonfly | Modificar… | Tienen diseñador de keys, pero no sincronización: las keys no tienen esquema que alterar. |
| etcd | Modificar… | Tiene diseñador de claves, pero no sincronización: no tiene esquema, solo claves con valores. |
| Cosmos DB | Cambiar un contenedor existente | La sincronización solo crea y borra contenedores: la partition key, las claves únicas, la política de índices, el TTL y las RU/s se cambian desde el portal o la CLI de Azure, no con SQL. El script avisa y no ejecuta. |
| Cassandra, ScyllaDB, Keyspaces | Cambiar el tipo de una columna o la clave primaria | El motor no tiene `ALTER` para eso: hay que recrear la tabla. El script avisa y no ejecuta. |
| MongoDB, FerretDB, DocumentDB | Cambiar los campos de documentos que ya existen | Los documentos no tienen columnas: se modifican reescribiendo cada uno. Se cambian el validador, los índices y las opciones; los campos quedan como aviso. |
| Elasticsearch, OpenSearch | Cambiar el tipo o borrar un campo, cambiar shards | El mapping no se modifica en el lugar: hay que reindexar. El script avisa. |
| Pendiente explícito | Modificar… en los motores donde no se probó | Solo los diez motores de arriba se probaron contra un servidor. Falta probar el resto: no hay contenedores ni emuladores para todos. |
