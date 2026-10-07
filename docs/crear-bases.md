# Crear bases

«Nueva base de datos…», en el menú de una conexión, abre un diálogo con el
nombre y, en los motores que las tienen, las opciones avanzadas del
`CREATE DATABASE` de ese motor.

## El diálogo

- **Nombre:** obligatorio.
- **Opciones avanzadas:** plegado por defecto. Solo aparece en los motores que
  tienen opciones; en los demás el diálogo pide únicamente el nombre.
- **Sugerencias del servidor:** al abrir las opciones, DBine le pregunta al
  servidor lo que puede ofrecer (intercalaciones, carpetas por defecto,
  usuarios, tablespaces, clústeres, regiones…) y lo muestra como lista. El
  campo sigue aceptando cualquier otro valor.
- **Campo vacío = el valor por defecto del servidor.** Si hay un valor
  conocido, se muestra como «Predeterminado: …». Con todas las opciones
  vacías, la base se crea como siempre, solo con el nombre.
- **Ver script:** muestra exactamente lo que va a ejecutar «Crear». Se puede
  abrir en una consulta con «Abrir en una consulta».
- **Crear:** corre como tarea en segundo plano, como el resto de las
  operaciones largas.

Cada valor se valida antes de armar el script, y los nombres y literales se
escapan: lo que se escribe en un campo no puede salirse de su cláusula.

## Pasos posteriores a la creación

En algunos motores una opción no entra en el `CREATE DATABASE` y se aplica
después con otra sentencia (por ejemplo, el modelo de recuperación, el nivel
de compatibilidad y el dueño en SQL Server, o el juego de caracteres por
defecto en Firebird). «Ver script» los muestra todos, en orden.

Si un paso posterior falla, **la base ya existe**. El error lo dice así:
«la base … se creó, pero falló este paso: …», con la sentencia que falló y
el mensaje del servidor. No se deshace la creación.

## Qué ofrece cada motor

### Familia SQL Server

- **SQL Server:** intercalación, dueño, modelo de recuperación, nivel de
  compatibilidad, y los archivos de datos y de log (carpeta, tamaño inicial,
  crecimiento y tamaño máximo de cada uno).
  - La carpeta va en `CREATE DATABASE … ON / LOG ON`, que necesita las dos
    partes: **la carpeta del log necesita la del datos**.
  - Los tamaños sin carpeta se aplican después con `ALTER DATABASE … MODIFY
    FILE`, sobre los archivos que el servidor nombró como la base (`nombre`,
    `nombre_log`).
  - Modelo de recuperación, nivel de compatibilidad y dueño son `ALTER`
    posteriores.
- **Azure SQL:** intercalación, edición, objetivo de servicio, tamaño máximo
  o elastic pool. Sin verificar contra un servidor real.
- **Microsoft Fabric y Babelfish:** sin opciones (Fabric se crea desde su
  portal; T-SQL de Babelfish no las tiene).

### Familia PostgreSQL

- **PostgreSQL y los que conservan su `CREATE DATABASE`** (TimescaleDB, EDB,
  Fujitsu, AlloyDB, Cloud SQL, Aurora): dueño, plantilla, codificación,
  proveedor de locale (15+), `LC_COLLATE`, `LC_CTYPE`, locale ICU (15+),
  locale builtin (17+), tablespace, límite de conexiones e `IS_TEMPLATE`
  (9.5+). Las cláusulas que el servidor conectado no tiene por ser de una
  versión vieja se rechazan antes de ejecutar, con la versión que piden.
- **KingbaseES, Greenplum, Cloudberry y Greengage:** lo mismo, sin proveedor
  de locale (su base PostgreSQL no lo tiene).
- **YugabyteDB:** sin tablespace (los suyos ubican tablas, no bases), más
  `COLOCATION`.
- **openGauss:** sin `IS_TEMPLATE`, más `DBCOMPATIBILITY` (el dialecto SQL).
- **CockroachDB:** dueño y las cláusulas multirregión (`PRIMARY REGION`,
  `REGIONS`, `SURVIVE … FAILURE`). `ENCODING` y `CONNECTION LIMIT` solo
  aceptan sus valores por defecto, así que no se ofrecen.
- **Redshift:** dueño, límite de conexiones, `COLLATE CASE_SENSITIVE` /
  `CASE_INSENSITIVE` y nivel de aislamiento. Sin verificar contra un servidor
  real.
- **RisingWave:** dueño, grupo de recursos, intervalo de barreras y
  frecuencia de checkpoint.
- **Yellowbrick:** dueño, codificación (`UTF8` o `LATIN9`), límite de
  conexiones y `HOT_STANDBY`. Sin verificar contra un servidor real.
- **Materialize:** sin opciones (`CREATE DATABASE` solo toma el nombre).

### Familia MySQL

- **MySQL (Aurora, Cloud SQL), MariaDB, TiDB y OceanBase:** juego de
  caracteres e intercalación. MariaDB suma un comentario y TiDB una política
  de ubicación (placement policy).
- **SingleStore:** cantidad de particiones.
- **StarRocks:** volumen de almacenamiento (`storage_volume`) y otras
  propiedades `clave=valor`. **Doris / VeloDB:** réplicas (`replication_num`)
  y otras propiedades `clave=valor`.
- **GreptimeDB:** retención por defecto de sus tablas (`WITH (ttl)`).
- **Databend:** sin opciones (su `ENGINE` tiene un solo valor útil).

### Oracle, SAP HANA, Firebird y por ODBC

- **Oracle:** acá una «base» es un esquema, es decir, un usuario sin
  autenticación (`CREATE USER … NO AUTHENTICATION`). Las opciones son su
  almacenamiento: tablespace por defecto, tablespace temporal y cuota en el
  tablespace por defecto (ilimitada si se deja vacía). Sin tablespace
  elegido, la cuota va al tablespace permanente por defecto de la base, que
  solo el servidor conoce: el script es un bloque anónimo que lo lee.
- **SAP HANA:** las «bases» de una conexión son esquemas, y `CREATE SCHEMA`
  toma una sola opción: el dueño (`OWNED BY`). Las bases de tenant se crean
  desde el cockpit de la base de sistema, no desde una conexión a un tenant.
  Sin verificar contra un servidor real.
- **Firebird:** la base es un archivo que crea el servidor. Las opciones son
  la carpeta (junto con el nombre forma la ruta del archivo), el tamaño de
  página y el juego de caracteres por defecto, que se aplica con un
  `ALTER DATABASE … SET DEFAULT CHARACTER SET` justo después (Firebird 3+).
  El script muestra el `CREATE DATABASE` y el `ALTER` separados por `;`.
- **Sybase ASE (ODBC):** dispositivos de datos y de log con sus tamaños
  (`ON dispositivo = tamaño`, `LOG ON …`). Si los dos comparten dispositivo
  usa `WITH OVERRIDE`, como exige ASE. Corre desde `master` y la sesión
  vuelve a su base después, aunque el `CREATE` falle. Sin verificar contra un
  servidor real.
- **Netezza (ODBC):** historial de consultas (`COLLECT HISTORY`) y retención
  de versiones en días (`DATA VERSION RETENTION TIME`). Sin verificar contra
  un servidor real.
- **ODBC genérico:** sin opciones; no se sabe qué motor hay detrás.

### Analíticas y en la nube

- **ClickHouse:** motor de la base (`Atomic`, `Replicated` con su ruta en
  Keeper, shard y réplica, `Memory`), `ON CLUSTER` y comentario. `Lazy` no
  se ofrece porque los servidores recientes (26.x) lo quitaron, y los motores
  que reflejan otro servidor (MySQL, PostgreSQL, S3…) piden una conexión,
  no opciones. Con `ON CLUSTER` se lee la respuesta de cada host hasta el
  final, así que un host que falla aparece.
- **Snowflake:** `TRANSIENT`, días de Time Travel, extensión máxima de
  retención, intercalación por defecto y comentario. Sin verificar contra un
  servidor real.
- **Databricks:** una «base» es un catálogo de Unity Catalog. Ubicación
  administrada (`MANAGED LOCATION`, con las ubicaciones externas que el
  usuario ve como sugerencia) y comentario. Sin verificar contra un servidor
  real.
- **Athena:** comentario, ubicación en S3 (`LOCATION`) y `DBPROPERTIES`
  (líneas `clave=valor`). Sin verificar contra un servidor real.
- **BigQuery:** una «base» es un dataset. **El script no es SQL: muestra la
  llamada a la API** (`datasets.insert`) con su cuerpo. Opciones: ubicación,
  vencimiento por defecto de las tablas, descripción, etiquetas e
  intercalación por defecto. La sesión agrega el proyecto y, si no se elige
  ubicación, la de la conexión.
- **Cloud Spanner:** período de retención de versiones y líder por defecto,
  como `ALTER DATABASE … SET OPTIONS` que corren junto con el `CREATE
  DATABASE` en una sola operación atómica: si uno falla no queda base. El
  dialecto PostgreSQL no se ofrece (ver
  [`soporte-por-motor.md`](soporte-por-motor.md#crear-bases-opciones)).

### Documentos, grafos, series de tiempo y claves

- **Cassandra y ScyllaDB (keyspace):** clase de replicación
  (`NetworkTopologyStrategy` por defecto, o `SimpleStrategy`), factor de
  replicación o uno por datacenter, y `durable_writes`. ScyllaDB suma
  `tablets`. Los datacenters se sugieren desde `system.local` y
  `system.peers`. Sin opciones, la creación es la de siempre:
  `NetworkTopologyStrategy` con una réplica por datacenter. ScyllaDB sin
  verificar contra un servidor real.
- **Amazon Keyspaces:** `SingleRegionStrategy` o `NetworkTopologyStrategy`
  con las regiones (Keyspaces mantiene siempre tres réplicas por región).
  Sin verificar contra un servidor real.
- **Couchbase (bucket):** **el script no es SQL: muestra la llamada**
  `POST /pools/default/buckets` con su formulario, que es lo que se envía.
  Opciones: tipo de bucket, memoria (se sugiere la libre del clúster),
  réplicas, política de desalojo, durabilidad mínima, almacenamiento, vida
  máxima de los documentos y permitir vaciar. Sin opciones, el bucket es de
  100 MB sin vaciado.
- **CouchDB:** shards (`q`), réplicas (`n`) y si es particionada, como
  parámetros de `PUT /{db}`. Los valores del clúster se sugieren solo si el
  usuario es administrador.
- **OrientDB:** el tipo de almacenamiento. El tipo de base no se pregunta:
  sigue siendo `graph`, que en OrientDB 3 tiene las mismas clases (`V`, `E`)
  que una base de documentos.
- **InfluxDB 1 (InfluxQL):** política de retención por defecto del `CREATE
  DATABASE … WITH DURATION … REPLICATION … SHARD DURATION … NAME …`.
- **InfluxDB 2 (buckets):** retención, duración de los grupos de shards y
  descripción, en el cuerpo de `POST /api/v2/buckets`. El id de la
  organización lo busca la sesión al crear, así que **el script lo muestra
  como el texto `(el id de la organización de la conexión)`** y no como un id.
- **InfluxDB 3:** período de retención de `POST /api/v3/configure/database`.
  Las duraciones se validan (`30d`, `1h30m`, `INF` donde corresponde).
- **IoTDB:** propiedades de `CREATE DATABASE root.x WITH …`: TTL, intervalo
  de partición por tiempo, cantidad de grupos de regiones de esquema y de
  datos, y, en IoTDB 1.x, los factores de replicación (IoTDB 2 los toma solo
  de la configuración del clúster y los rechaza acá). Las duraciones aceptan
  unidad (`7d`, `12h`) o milisegundos.
- **TDengine:** los parámetros del `CREATE DATABASE` de TDengine 3: precisión
  del tiempo, retención (`KEEP`), días por archivo, réplicas, vgroups,
  memoria (`BUFFER`, `PAGES`, `PAGESIZE`), caché del último dato, WAL,
  compresión, tamaños de bloque, `STT_TRIGGER` y `SINGLE_STABLE`.
- **Neo4j** (Enterprise, la edición que crea bases): topología (primarios y
  secundarios) y el mapa `OPTIONS` (formato de almacenamiento, enriquecimiento
  del log de transacciones y un backup o dump desde el cual sembrar). Se
  ejecuta en `system`.
- **Cosmos DB:** throughput compartido por los contenedores de la base,
  manual o autoscale, como encabezados de `POST /dbs`. Sin él, cada
  contenedor tiene el suyo, como antes. Las cuentas serverless rechazan el
  throughput provisionado y el error del servidor vuelve tal cual. Sin
  verificar contra un servidor real.

## Contrato

- `Driver::create_database_fields()` devuelve las opciones (`Field`), en
  orden, con su `key`. Vacío: el motor solo toma el nombre. Viaja en
  `DriverInfo` como `create_database_fields`.
- `Driver::create_database_script(name, options)` devuelve el código que
  muestra «Ver script»; por defecto, `Error::Unsupported`.
- `Session::create_database_choices()` devuelve las sugerencias del servidor
  (`FieldChoices`: `key`, `default`, `values`).
- `Session::create_database_with(name, options)` crea la base. `options` es
  `key` → valor; vacío o ausente es el valor por defecto del servidor. Con
  todas las opciones vacías llama a `Session::create_database`; si el motor
  no implementa las opciones, devuelve `Error::Unsupported`.

Los comandos Tauri están en [`api-comandos.md`](api-comandos.md#bases-de-datos).
Qué soporta cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#crear-bases-opciones).
