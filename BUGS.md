# Bugs conocidos

Bugs encontrados mientras se implementaba el chequeo de permisos
(`Session::permissions`, septiembre de 2026). Cada entrada explica qué pasa,
dónde está el código, cómo reproducirlo y una idea de arreglo.

## Estado

| # | Estado |
|---|---|
| 1 | Arreglado y verificado en vivo (test `clickhouse/tests/readonly_profile.rs`). El profiler también usa `self.sends_readonly()` para perfiles `readonly=2`. |
| 2 | Arreglado (Couchbase y DuckDB rechazan crear y borrar bases en solo lectura; el menú del explorador oculta las dos opciones). Verificado en vivo. |
| 3 | Arreglado y verificado en vivo (mongo:7): `sampleRate` solo se manda si la tasa actual es menor que 1, y al restaurar solo si se cambió. Test: `db_admin_only_turns_the_profiler_on`. |
| 4 | No se reproduce en CockroachDB v26.3.2: un usuario sin admin puede leer `crdb_internal.cluster_queries` y `cluster_sessions`. Igual se agregó la alternativa `SHOW CLUSTER STATEMENTS/SESSIONS` si esa lectura falla, y un aviso en español cuando falta `VIEWACTIVITY` (sin ese permiso el usuario solo ve sus consultas). También se corrigió el Monitor: para las métricas de nodo pedía `VIEWACTIVITY` y el permiso real es `VIEWCLUSTERMETADATA`. Test en vivo: `postgres/tests/cockroach_viewer.rs`. |
| 5 | Arreglado: `dbine-test-iotdb2` ahora usa 27150 (REST) y 27151. Aparte: el round trip de tipos 2.x en `transfer_iotdb` hace que Docker mate el contenedor por falta de memoria (exit 137). |
| 6 | Arreglado y probado con Scylla 2026.3.1. |
| 7 | Arreglado en Athena y DynamoDB: los tests usan un cliente HTTP plano y pasan sin `SSL_CERT_FILE` (Athena 45, DynamoDB 46). |
| 8 | Arreglado. |
| Inconsistencia | Arreglado en ClickHouse, Elasticsearch, Solr, CouchDB, Couchbase, OrientDB, Cosmos DB, DynamoDB y MongoDB: el chequeo ya no mira el modo solo lectura. DuckDB: resuelto con el punto 2. |

---

## 1. ClickHouse: un usuario con perfil `readonly = 1` no puede conectarse

**Gravedad:** alta. El usuario no puede usar DBine con esa cuenta.

**Qué pasa.** Cada consulta que DBine manda a ClickHouse por HTTP lleva
settings en la URL: `output_format_json_quote_64bit_integers=1`,
`output_format_json_quote_decimals=1` y `http_write_exception_in_output_format=0`.
Si el perfil del usuario en el servidor tiene `readonly = 1`, ClickHouse no deja
cambiar ningún setting desde la consulta y rechaza todo con un error del tipo
"Cannot modify 'output_format_json_quote_64bit_integers' setting in readonly
mode". Falla ya la primera consulta al conectar.

Con `readonly = 2` sí funciona, porque ese nivel permite cambiar settings.
El modo solo lectura de DBine también funciona: manda `readonly=1` en la misma
URL que los otros settings, y ClickHouse los acepta juntos.

**Dónde.** `crates/drivers/clickhouse/src/lib.rs`, función
`ClickHouseSession::send` (cerca de la línea 285), donde se arma el vector `q`
con los settings.

**Cómo reproducirlo** (en `dbine-test-clickhouse`):

```sql
CREATE SETTINGS PROFILE ro_profile SETTINGS readonly = 1;
CREATE USER ro_user IDENTIFIED BY 'x' SETTINGS PROFILE 'ro_profile';
GRANT SELECT ON *.* TO ro_user;
```

Después, conectar desde DBine con `ro_user`.

**Idea de arreglo.**
- Al conectar, leer `getSetting('readonly')`. Si da 1, no mandar los settings de
  formato.
- Hacer ese ajuste del lado del cliente: los enteros de 64 bits y los decimales
  llegan sin comillas, así que hay que parsearlos sin perder precisión. Por
  ejemplo, pedir el formato `JSONCompactEachRowWithNamesAndTypes` y leer los
  números como texto.
- Otra opción: si el primer intento falla con "Cannot modify … in readonly
  mode", reintentar sin esos settings y recordarlo para el resto de la sesión.

---

## 2. Couchbase: una conexión de solo lectura puede crear y borrar buckets

**Gravedad:** alta. El modo solo lectura de DBine promete que no se escribe
nada, y esto borra datos.

**Qué pasa.** `create_database` y `drop_database` del driver de Couchbase no
miran `self.conn.read_only`. Con una conexión marcada como solo lectura, "Nueva
base" crea un bucket y "Borrar base" lo elimina con todos sus datos, siempre que
el usuario de Couchbase tenga el permiso en el servidor.

Otros caminos del mismo driver sí respetan el modo (por ejemplo, `execute`
alrededor de la línea 415 de `lib.rs`). Solo faltan estos dos.

**Dónde.** `crates/drivers/couchbase/src/lib.rs`, `async fn create_database`
(línea ~776) y `async fn drop_database` (línea ~795).

**Cómo reproducirlo.** Crear una conexión a `dbine-test-couchbase` con
"Solo lectura" activado y un usuario administrador. En el explorador, usar
"Nueva base" o "Borrar base": el bucket se crea o se borra.

**Idea de arreglo.** Al principio de las dos funciones:

```rust
if self.conn.read_only {
    return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
}
```

y lo mismo con "borrar". Así lo hacen `ReadOnlySession` en
`crates/dbine-driver/src/read_only.rs` y los demás drivers. Conviene revisar
también que la UI no ofrezca esas opciones en conexiones de solo lectura.

---

## 3. MongoDB: el profiler falla para un usuario con `dbAdmin` sobre una sola base

**Gravedad:** media. El profiler no muestra nada a un usuario que tiene
permisos de sobra para usarlo.

**Qué pasa.** Al iniciar, el profiler manda
`{ profile: 2, sampleRate: 1.0 }` a la base. Cambiar `sampleRate` afecta a todo
el servidor, así que MongoDB exige `enableProfiler` sobre todas las bases. Un
usuario con `dbAdmin` sobre una sola base recibe "not authorized", aunque
`sampleRate` ya esté en 1.

Entonces el driver cae al modo muestreado con `currentOp`, que ese usuario
tampoco puede leer (necesita `inprog`). Resultado: el profiler no muestra nada.

**Dónde.** `crates/drivers/mongodb/src/profiler.rs`, cerca de la línea 106:

```rust
let set = doc! { "profile": 2, "sampleRate": 1.0, "comment": s.tag.as_str() };
```

Y la restauración, cerca de la línea 124, que también manda `sampleRate`.

**Cómo reproducirlo** (en un MongoDB con autenticación):

```js
use app
db.createUser({ user: "dba", pwd: "x", roles: [{ role: "dbAdmin", db: "app" }, { role: "read", db: "app" }] })
```

Después, abrir el profiler de la base `app` con ese usuario.

**Idea de arreglo.** Mandar `sampleRate` solo si hace falta cambiarlo: si el
valor leído (`rate`, línea ~72) ya es 1.0, mandar solo `{ profile: 2 }`. Hacer
lo mismo al restaurar: si `rate` no cambió, no mandarlo. Así el usuario con
`dbAdmin` puede subir el nivel a 2 en su base.

---

## 4. CockroachDB v26.3: el profiler puede fallar para usuarios que no son admin

**Gravedad:** media. **Sin verificar.**

**Qué pasa.** El profiler y el Monitor de CockroachDB leen
`crdb_internal.cluster_queries`. En v26.3, el contenedor de prueba rechaza a los
usuarios que no son `admin` cualquier acceso a `crdb_internal` si no está
activado `allow_unsafe_internals`. Si pasa lo mismo en producción, un usuario
con el privilegio `VIEWACTIVITY` (que antes alcanzaba) ya no puede usar el
profiler ni el Monitor.

**Dónde.**
- `crates/drivers/postgres/src/profiler.rs` (líneas ~95 y ~239).
- `crates/drivers/postgres/src/monitor.rs` (línea ~949).

**Cómo verificarlo.** En `dbine-test-cockroach`:

```sql
CREATE USER viewer;
GRANT SYSTEM VIEWACTIVITY TO viewer;
```

Después, conectar como `viewer` y correr
`SELECT * FROM crdb_internal.cluster_queries`.

**Idea de arreglo.** Si falla, usar el reemplazo público y estable:
`SHOW CLUSTER STATEMENTS` o las vistas de `crdb_internal` que CockroachDB haya
movido a `information_schema` o a `system`. Si no hay alternativa, que el
profiler explique el motivo (falta `allow_unsafe_internals` o hace falta ser
admin) en lugar de fallar sin más.

---

## 5. Contenedores de prueba: `dbine-test-iotdb2` y `dbine-test-dragonfly` usan el mismo puerto

**Gravedad:** baja. Solo afecta las pruebas.

**Qué pasa.** Los dos publican el puerto 25407 del host:
- `dbine-test-iotdb2`: 18080 → 25407 (y 9092 → 25408).
- `dbine-test-dragonfly`: 6379 → 25407.

Si Dragonfly está prendido, IoTDB 2 arranca sin red y no se puede alcanzar. Las
pruebas en vivo de IoTDB 2 se tuvieron que hacer en un contenedor descartable
en otro puerto.

**Idea de arreglo.** Recrear `dbine-test-iotdb2` con puertos libres (por
ejemplo 27150 y 27151) y actualizar la variable `DBINE_TEST_IOTDB2_URL` o el
comentario de los tests de `crates/drivers/iotdb/tests/` que la documenta.

---

## 6. El comando documentado para levantar Scylla con autenticación ya no funciona

**Gravedad:** baja. Solo afecta las pruebas.

**Qué pasa.** Scylla 2026.x ya no crea el superusuario por defecto `cassandra`.
Con el comando del comentario, el contenedor arranca con autenticación, pero no
hay ningún usuario para entrar.

**Dónde.** `crates/drivers/cassandra/tests/security.rs`, línea 4 del comentario
del encabezado.

**Idea de arreglo.** Agregar al comando
`--auth-superuser-name cassandra --auth-superuser-salted-password '<hash>'`,
escapando los `$` del hash en el shell. Hay que generar el hash de `cassandra`
con el formato que espera Scylla (crypt SHA-512). Otra opción es fijar la imagen
en una versión anterior que todavía cree el superusuario.

---

## 7. Athena y DynamoDB: pruebas que fallan al crear el cliente de AWS

**Gravedad:** baja. Probablemente es del entorno y no del código, pero conviene
confirmarlo.

**Qué pasa.** Fallan varias pruebas unitarias que construyen un cliente de AWS:
- Athena: `tests::never_drops_its_own_database` y cinco de `transfer::tests::*`.
- DynamoDB:
  `transfer::tests::a_writer_dropped_during_runtime_shutdown_neither_panics_nor_hangs`.

El pánico es:

```
aws-smithy-http-client-1.4.2/src/client/tls/rustls_provider.rs:163
TrustStore configured to enable native roots but no valid root certificates parsed!
```

El cliente de AWS carga los certificados raíz del sistema, y en el entorno donde
corrieron (sesiones de Claude, posiblemente con acceso limitado al llavero de
macOS) no encontró ninguno. Es un `debug_assert!`, así que solo falla en builds
de debug.

**Cómo verificarlo.** Correr `cargo test -p dbine-driver-athena` desde una
terminal normal. Si ahí pasan, es del entorno.

**Idea de arreglo, si también fallan en una terminal normal o en CI.** Los
tests no necesitan TLS real: construir el cliente con un conector HTTP de prueba
(`aws_smithy_http_client::test_util`) o con una configuración que no cargue las
raíces del sistema.

---

## 8. Warning: tipo `Zone` sin usar en la clonación de tablas de PostgreSQL

**Gravedad:** muy baja. Es un warning de compilación.

**Dónde.** `crates/dbine-transfer/src/clone_table/pg.rs:611`:

```rust
type Zone = (Option<String>, String);
```

Viene de la función "Clonar tabla". Hay que borrarlo o usarlo donde se pensaba
usar.

---

## Pendiente de verificar contra un servidor real

No son bugs confirmados. Son consultas del chequeo de permisos que se
escribieron según la documentación del fabricante y nunca corrieron contra un
servidor. Si fallan, el chequeo deja las acciones habilitadas (no da error),
pero conviene confirmarlas:

- **SAP HANA** (`crates/drivers/hana/src/permissions.rs`): que
  `SYS.EFFECTIVE_PRIVILEGES` acepte el filtro `USER_NAME = CURRENT_USER`.
- **Snowflake** (`crates/drivers/snowflake/src/permissions.rs`): que
  `IS_ROLE_IN_SESSION` acepte una columna como argumento. Se usa para saber si
  el rol dueño de la base está en la sesión.
- **Cloud Spanner** (`crates/drivers/spanner/src/permissions.rs`): que
  `testIamPermissions` a nivel de instancia acepte los permisos
  `spanner.backups.*`.
- **BigQuery** (`crates/drivers/bigquery/src/permissions.rs`): el chequeo usa la
  API de Resource Manager, que tiene que estar habilitada en el proyecto. Si no
  lo está, todo queda habilitado.
- **Dremio Enterprise, Db2 LUW, SAP ASE, Redshift, RisingWave, CrateDB,
  Memgraph Enterprise, InfluxDB 3 Enterprise, Amazon DocumentDB, Open Distro:**
  implementados sin servidor de prueba.

## Inconsistencia menor del chequeo de permisos

Con una conexión en modo solo lectura de DBine, algunos drivers informan las
acciones de escritura como "falta: escritura (la conexión es de solo
lectura)": ClickHouse, MongoDB, Elasticsearch, Solr, CouchDB, Couchbase,
OrientDB, Cosmos DB y DynamoDB. Los demás solo informan lo que permite el
servidor. En la práctica no cambia nada, porque la UI ya oculta las escrituras
de las conexiones de solo lectura. Aun así, habría que elegir un criterio y
aplicarlo en todos. Lo más simple es que el chequeo de permisos no mire nunca
el modo solo lectura, porque de eso se encarga `ReadOnlySession`.

Relacionado: en DuckDB, con la conexión en solo lectura, "Borrar base" igual
desvincula la base y borra su archivo. Hay que decidir si el modo solo lectura
debería bloquearlo.

---

## Detalles menores de la pantalla de migración (septiembre de 2026)

Encontrados al probar el nodo "Migraciones". Ninguno rompe nada.

- **Selector de corridas mientras corre:** con la migración en curso, el
  selector de corridas del panel "Ejecución" dice "Sin ejecuciones en esta
  máquina". Cuando termina o se cancela, lista la corrida bien. Está en
  `web/src/views/MigrationView.vue`: el historial se carga desde
  `migration_runs` y la corrida en curso todavía no figura ahí. Hay que sumar
  la corrida viva a la lista.
- **Nota repetida:** después de dos corridas de Clone sobre la misma migración,
  la nota "Los dueños, permisos y tablespaces no se clonan…" aparece una vez por
  corrida. Hay que deduplicar las notas al mostrarlas.
- **Comentario del script en español:** el comentario del script generado
  ("Estructura de … convertida a …") sale en español aunque la UI esté en otro
  idioma. Hay que decidir si el script sigue el idioma de la UI.
