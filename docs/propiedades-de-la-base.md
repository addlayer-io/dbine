# Propiedades de la base

Muestra lo que el motor informa de una base (tamaño, codificación, estado,
configuración) y deja **cambiar lo que el motor permite cambiar**, siempre
mostrando antes el script y las advertencias de lo que el cambio provoca.

## Dónde está

Clic derecho sobre una **base** › **Propiedades…**. Aparece solo en los
motores que tienen la capacidad (`database_properties`).

## El diálogo

- **Pestañas:** **General** y, a continuación, un grupo por cada conjunto de
  opciones que tiene el motor (por ejemplo, en SQL Server: recuperación,
  opciones automáticas, archivos…). Si hay un solo grupo no se muestran
  pestañas.
- Arriba de cada pestaña, los **datos de solo lectura** (tamaño, fecha de
  creación, cantidad de objetos, versiones…). Debajo, los **campos
  editables** con su valor actual. Los campos que el motor puede completar
  (colaciones, ubicaciones, usuarios…) ofrecen sugerencias del servidor.
- **Ver script:** muestra el script de lo que cambiaste, con **Abrir en una
  consulta** para revisarlo o ejecutarlo aparte. Solo incluye lo que difiere
  de lo leído.
- **Aplicar:** pide confirmación con el script y las **advertencias** de los
  cambios que molestan a la base o a sus usuarios (por ejemplo, que cierran
  sesiones abiertas o la ponen fuera de línea). Recién entonces lo ejecuta.
- En una **conexión de solo lectura** las propiedades se ven pero no se
  pueden cambiar (**Conexión de solo lectura…**).
- Si el motor no tiene nada para mostrar ni cambiar, lo dice.

Las propiedades se leen con la sesión a nivel de servidor de la conexión,
como la creación y el borrado de bases: algunos cambios no se pueden
ejecutar desde dentro de la propia base (por ejemplo, el tablespace de
PostgreSQL).

## Particularidades por motor

Cada motor muestra y cambia lo suyo, y **solo ofrece lo que el servidor
informa**: una opción de una versión más nueva aparece solo si existe.

| Motor | Qué hay |
|---|---|
| SQL Server, Azure SQL | Propietario y collation; recuperación, compatibilidad, `PAGE_VERIFY`, tiempo de recuperación objetivo, durabilidad diferida; opciones `AUTO_*`; estado, acceso y solo lectura; aislamiento de instantánea; opciones ANSI, `TRUSTWORTHY` y `DB_CHAINING`; tamaño, crecimiento y máximo de cada archivo. En Azure SQL Database, el servicio (edición, objetivo, tamaño máximo). Los cambios que necesitan la base para sí corren con `ROLLBACK IMMEDIATE` y la advertencia lo dice antes. |
| PostgreSQL y su familia (TimescaleDB, EDB, Fujitsu, AlloyDB, Cloud SQL, Aurora, KingbaseES, Greenplum, YugabyteDB, openGauss) | Propietario, límite de conexiones, `ALLOW_CONNECTIONS`, `IS_TEMPLATE`, tablespace, `REFRESH COLLATION VERSION`, comentario y los valores por defecto por base (`SET` / `RESET`). Datos: tamaño, conexiones, codificación y locale, versiones de collation, edad del XID. |
| CockroachDB | Propietario, regiones, objetivo de supervivencia, ubicación, comentario y valores por defecto. |
| Redshift, Yellowbrick, RisingWave, Materialize | Redshift: propietario, límite de conexiones, sensibilidad a mayúsculas, aislamiento y comentario. Yellowbrick: propietario, límite, `ALLOW_CONNECTIONS`, `HOT_STANDBY`, solo lectura y tamaño máximo. RisingWave: propietario, grupo de recursos, intervalo de barrera y frecuencia de checkpoint. Materialize: propietario y comentario. |
| MySQL, MariaDB, TiDB, OceanBase | Juego de caracteres y collation por defecto (en un solo `ALTER`); según versión, `READ ONLY`, `DEFAULT ENCRYPTION` (solo en instalaciones propias: necesita un keyring), comentario (MariaDB 10.5+), política de ubicación (TiDB). Datos: tamaño, tablas y vistas. |
| SingleStore, StarRocks, Doris, GreptimeDB | Replicación sincrónica o asincrónica; cuotas de datos y réplicas (StarRocks, Doris; Doris suma la cuota de transacciones y `SET PROPERTIES`); TTL por defecto (GreptimeDB). |
| Oracle | La "base" es un esquema (una cuenta): sus datos salen de `DBA_USERS`, `DBA_SEGMENTS` y `DBA_TS_QUOTAS`; se cambian tablespace por defecto y temporal, cuota por tablespace, perfil y bloqueo de la cuenta. Necesita las vistas DBA y el privilegio `ALTER USER`. |
| Snowflake | Propietario y comentario; Time Travel; opciones (collation de DDL, identificadores entre comillas, política de serialización, catálogo y volumen externo); tareas y registro. Solo los parámetros que lista la cuenta. Vaciar un parámetro lo devuelve al valor de la cuenta. |
| BigQuery | Un *dataset*: descripción; vencimiento por defecto de tablas y particiones, collation, nombres sin distinguir mayúsculas, modo de redondeo; ventana de *time travel* y modelo de cobro; etiquetas. Todo en una sola llamada, que se aplica completa o no se aplica. |
| Databricks | Un catálogo: comentario, optimización predictiva y propietario. La raíz de almacenamiento, el aislamiento y el tipo se muestran, no se cambian. |
| Spanner | Protección contra borrado, líder por defecto, zona horaria, retención de versiones, versión del optimizador, paquete de estadísticas y tipo de secuencia por defecto. Las bases en dialecto PostgreSQL se muestran pero no se cambian. |
| ClickHouse | El comentario y, en los motores de base que lo admiten (MaterializedPostgreSQL, DataLakeCatalog), cada opción de `SETTINGS`. Timeplus Proton no tiene `ALTER DATABASE`. |
| Firebird | Datos de `MON$DATABASE`; se cambian el juego de caracteres por defecto y el comentario, y según versión el *linger*, `SQL SECURITY` y la publicación de replicación. Solo lectura, escrituras forzadas e intervalo de *sweep* se muestran pero no se ofrecen. |
| Cassandra, ScyllaDB | Replicación (clase, factor o factor por datacenter) y `durable_writes`. En ScyllaDB con *tablets*, solo `NetworkTopologyStrategy`, y si usa *tablets* no se puede cambiar. |
| Amazon Keyspaces | Agregar una región (activa las marcas de tiempo del cliente, como exige AWS). |
| MongoDB | Contadores de `dbStats` y el nivel del *profiler* (por base); `slowms` y `sampleRate` son del servidor entero. |
| FerretDB, Amazon DocumentDB | Solo `dbStats` (FerretDB no tiene `profile`; DocumentDB lo define en el grupo de parámetros del clúster). |
| CouchDB | Documentos, tamaños, clúster y secuencias; se cambian `_revs_limit`, `_purged_infos_limit` y el objeto `_security` (se reemplaza completo). |
| Couchbase | Un *bucket*: cuota de RAM, réplicas, vaciado, expulsión, durabilidad, TTL máximo, compresión y más, solo si el servidor lo informa. Tipo, motor de almacenamiento y resolución de conflictos son de solo lectura. |
| Azure Cosmos DB | El rendimiento compartido de la base (manual o autoescalado). Con rendimiento por contenedor o *serverless*, solo se ve. |
| Neo4j | Datos de `SHOW DATABASE` y conteos. En Enterprise se cambian acceso, topología, enriquecimiento del log de transacciones y versión de Cypher; Community, solo datos. |
| Memgraph | Datos de `SHOW STORAGE INFO`; el modo de almacenamiento y el aislamiento tienen sus propias sentencias fuera de este diálogo. |
| OrientDB | Zona horaria, locale, charset, formatos de fecha, selección de clústeres, estrategia de conflicto, validación, SQL estricto y atributos propios. |
| InfluxDB | 1: políticas de retención (duración, grupo de shards, replicación, por defecto). 2: retención, duración del grupo de shards y descripción del bucket. 3: período de retención (desde 3.2). En las versiones HTTP el "script" es el pedido que se envía. |
| IoTDB | TTL de toda la base y la cantidad de grupos de regiones de esquema y de datos. |
| TDengine | Los parámetros de `ALTER DATABASE` que informa el servidor (WAL, réplicas, almacenamiento compartido…); varían con la versión. |
| Athena | Las propiedades de la base (`DBPROPERTIES`), cambiadas o nuevas en una sentencia; la descripción solo se ve. |
| Dremio | De un origen: políticas de actualización de metadatos y de reflexiones. Los espacios y los hogares solo muestran datos. |
| ODBC | Sybase ASE: propietario, tamaño y opciones de `sp_dboption`. Netezza: propietario, esquema por defecto, historial de consultas y retención de viajes en el tiempo. Informix y GBase 8s: datos solamente (el modo de registro se cambia con herramientas del motor, no con SQL). |
| DuckDB, SQLite, libSQL | DuckDB: solo datos (los ajustes son de la instancia o de la sesión). SQLite: `user_version`, `application_id`, el diario (`DELETE` o `WAL`), el tamaño de página y el *auto vacuum* (estos dos se aplican con `VACUUM`). libSQL: solo `user_version`. |
| Redis, Valkey, Dragonfly | Solo datos de `INFO keyspace`: no guardan ajustes por base (`CONFIG SET` es del servidor entero). |
| SAP HANA | La "base" es un esquema y HANA no tiene `ALTER SCHEMA`: solo datos (propietario, creación, cantidad de objetos y tamaño en memoria y disco). |
| Databend, Manticore, Amazon Neptune, Denodo, CrateDB, H2 | Sin propiedades. Databend: su `ALTER DATABASE` solo renombra. Manticore: no tiene bases. Neptune: los ajustes viven en el grupo de parámetros del clúster (API de AWS), no detrás de openCypher. Denodo, CrateDB y H2: no tienen bases que DBine administre. |

Los motores que no están en la lista no ofrecen **Propiedades…**.

## Contrato

Tres métodos, detrás de la capacidad `database_properties`:

- `database_properties(database)` devuelve un `DatabaseProperties`: los
  `fields` (los campos editables), sus `values` actuales, la información de
  solo lectura (`info`), las sugerencias del servidor (`choices`) y las
  advertencias por campo (`warnings`).
- `Driver::alter_database_script(database, changes)` genera el script de lo
  que cambió (`changes` es campo → valor nuevo).
- `Session::alter_database(database, changes)` lo aplica.

Comandos: `database_properties`, `alter_database_script` y `alter_database`
(rechazado en conexiones de solo lectura). Ver
[`api-comandos.md`](api-comandos.md).
