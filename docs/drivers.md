# Cómo se escribe un driver de DBine

Cada motor vive en su propio crate, en `crates/drivers/<nombre>`, y cumple el
contrato de `crates/dbine-driver` (traits `Driver` y `Session`). Un crate puede
servir a varios motores que comparten el protocolo: el de PostgreSQL también
sirve a CockroachDB y a Redshift, y el de MySQL a MariaDB y a StarRocks.
`crates/dbine-drivers` los registra, con una feature de cargo por crate.

La UI se arma a partir de lo que declara cada driver en `DriverInfo`:

- el formulario de conexión, a partir de `fields`;
- las carpetas del explorador, a partir de `object_kinds`;
- el lenguaje del editor, a partir de `language` y `dialect`.

Por eso sumar un motor no toca el frontend.

## Estructura del crate

```
crates/drivers/<nombre>/
  Cargo.toml     # name = "dbine-driver-<nombre>"; depende de dbine-driver (path = "../../dbine-driver")
  src/lib.rs     # pub fn drivers() -> Vec<Arc<dyn Driver>>
  src/...        # los módulos que haga falta
```

- **`drivers()` es la única API pública obligatoria.** Devuelve un `Arc` por
  motor que sirve el crate. El id de cada motor (`DriverInfo::id`) es estable,
  en minúsculas y sin espacios: `postgres`, `cockroachdb`, `mongodb`…
- **Dependencias.** Usá `workspace = true` para las que ya están en
  `[workspace.dependencies]` (tokio, serde, serde_json, futures, async-trait,
  chrono, tracing, uuid, rusqlite, reqwest). Cualquier otra se declara con su
  versión en el `Cargo.toml` del crate.
- **Errores.** `dbine_driver::Error` no puede tener `impl From<ErrorDelCliente>`,
  por la regla de huérfanos. Cada crate mapea sus errores con una función
  propia (`fn err(e: X) -> Error`) y `.map_err(err)?`:
  - `Error::AuthFailed`: login rechazado.
  - `Error::Connect`: no se llega al servidor.
  - `Error::Query`: el servidor rechazó la sentencia.
  - `Error::Unsupported`: lo que el motor no ofrece.

  Los mensajes que ve el usuario van en español; si vienen del servidor, se
  pasan tal cual.

## Qué implementar

| Método | Qué devuelve |
|---|---|
| `info()` | `DriverInfo`: nombre, familia, lenguaje, puerto, campos del formulario, tipos de objeto. |
| `connect(cfg, database)` | Una `Session`: **una** conexión viva a esa base (keyspace, dataset, índice…). Sin pools. Con timeout de conexión (15–20 s). |
| `server_version()` | Producto y versión en una línea. |
| `list_databases()` | El nivel debajo de la conexión. Si el motor tiene un solo espacio, un único elemento (`["main"]`, `["default"]`) y `databases_label: ""`. |
| `list_objects()` | `DbObject`s con `kind` = uno de los `ObjectKindInfo::id` declarados. Excluir objetos del sistema. |
| `columns(obj)` | Columnas o campos. Si el motor no tiene esquema, se infieren de una muestra (p. ej. 100 documentos): `data_type` es el tipo observado y `nullable` es `true` si no aparece en todos. |
| `definition(obj)` | El código fuente, un mapping o la definición en JSON; `None` si no hay nada que mostrar. |
| `browse_query(obj, limit)` | El texto, **en el lenguaje del driver**, que muestra las primeras filas o documentos del objeto. La UI lo ejecuta con `execute`. |
| `filtered_browse(browse, filters)` | (en `Driver`, con implementación por defecto) La consulta de `browse_query` con los filtros de columna aplicados: un `WHERE` en SQL/CQL con las comillas y literales del dialecto, o el filtro nativo del motor. `Error::Unsupported` con el motivo si no se puede; la UI filtra entonces las filas cargadas. |
| `execute(text, max_rows, out)` | Ejecuta el script completo (ver el formato de resultados abajo). |
| `interrupter()` | Una función para cancelar desde otro hilo cuando soltar la sesión no alcanza. Ejemplos: `KILL QUERY`, un cancel request, `DELETE` del job por HTTP, o el interrupt de un hilo bloqueante. |

### Resultados

Todos los resultados son tabulares (`QueryOutcome`):

- **Formato de celdas.** Usá `out.begin_result(columnas)`, `out.push_row(celdas, max_rows)`
  y `out.push_affected(n)`. `push_row` ya limita la memoria: se sigue
  consumiendo el stream, pero solo se guardan `max_rows` filas.
- **Tipos de celda.** Las celdas son JSON:
  - `null`, `bool`, números con `json_i64`/`json_u64`/`json_f64` (los enteros
    más allá de 2^53 pasan a string);
  - decimales y fechas como string (fechas en formato ISO: `2024-01-31 13:45:00`);
  - binarios con `json_bytes`;
  - objetos o arrays anidados como string JSON compacto.
- **Documentos** (Mongo, CouchDB, Cosmos, DynamoDB, Elastic, Solr):
  - una fila por documento;
  - las columnas son la unión de las claves de primer nivel, en orden de
    aparición;
  - los valores anidados van como string JSON.
- **Clave-valor** (Redis): columnas según el comando. `GET` devuelve `value`;
  `HGETALL` devuelve `field, value`; un escalar va en una columna `result`.
- **Mensajes y avisos** del servidor van en `out.messages`.
- **Errores y scripts.** Si una sentencia falla, `execute` devuelve `Err`; lo
  que se ejecutó antes queda en `out`. Si el servidor acepta una sola
  sentencia por pedido, partí el script con `dbine_driver::sql::split_statements`
  (SQL) o por líneas o documentos, según el lenguaje.

### Solo lectura

`cfg.read_only`:

- **Drivers SQL:** el registro ya los envuelve en `ReadOnlySession`. Si el
  motor lo soporta, reforzalo además del lado del servidor, por ejemplo con
  `SET SESSION TRANSACTION READ ONLY`.
- **Resto de los drivers:** lo aplican ellos. Rechazan con `Error::Query` los
  comandos que escriben (lista blanca de comandos de lectura).

### Seguridad

- Nada de concatenar strings del usuario en consultas de catálogo: usá
  parámetros, o el helper de quoting de identificadores del dialecto
  (`dbine_driver::sql`).
- Los secretos (`password` y los campos con `.secret()`) llegan en `cfg`; no se
  loguean nunca.

## Campos de conexión

Los atajos `Field::host()`, `port()`, `database()`, `username()`, `password()`,
`encrypt()`, `trust_cert()` y `read_only()` se guardan en los campos tipados de
`ConnectionConfig`. `Field::server_set()` es el juego completo.

Cualquier otra clave (`region`, `project_id`, `auth_mode`, `api_key`,
`service_account_json`…) va a `cfg.options` y se lee con `cfg.option("clave")`.
Si es un secreto, lleva `.secret()`: la UI la guarda en el llavero.

## Pruebas

- **Tests unitarios** para los helpers puros: conversión de valores, armado de
  consultas, parseo de respuestas.
- **Test de integración contra un servidor real, si hay imagen de Docker.**
  - Ponelo en `tests/integration.rs`, marcado `#[ignore]`, y que lea la URL de
    una variable de entorno `DBINE_TEST_<MOTOR>_URL`.
  - Contenedores: nombre `dbine-test-<motor>`, puerto del host alto y libre, y
    borrarlo al terminar (`docker rm -f dbine-test-<motor>`).
  - **Nunca** tocar contenedores que no empiecen con `dbine-test-`: son de
    otros proyectos.
- Para validar el crate:
  - `cargo check -p dbine-driver-<nombre>`
  - `cargo test -p dbine-driver-<nombre>`
  - `cargo clippy -p dbine-driver-<nombre>`

## Motores sin cliente nativo en Rust

Para los motores que solo tienen driver JDBC u ODBC del fabricante (DB2,
Sybase, Informix, Teradata, Hive, Vertica…) se usa el crate `odbc`. Se apoya en
el driver manager (unixODBC en macOS y Linux, el nativo en Windows) y en el
driver ODBC que instale el usuario. En esos casos el formulario pide el nombre
del driver ODBC o un DSN.

## Librerías nativas de terceros: se descargan al usarlas

Un driver que se apoya en una librería nativa grande de un tercero (C o C++)
no la mete dentro de la app: la descarga la primera vez que alguien la usa y la
carga en tiempo de ejecución. Quien no usa ese motor no paga su peso. Es la
misma estrategia que el modelo de IA integrado.

Hoy aplica a DuckDB (`crates/drivers/duckdb/src/loader.rs`, unos 31 MB menos
en el ejecutable):

- **Qué se baja:** el build oficial de DuckDB para la plataforma
  (`libduckdb-<plataforma>.zip` de sus releases de GitHub), con la versión y el
  SHA-256 fijos en el código. Trae `parquet`, `json` e `icu` incluidos, así que
  el preset de archivos funciona sin más descargas.
- **Cuándo:** en el primer `connect`. La descarga se retoma si se corta, se
  verifica el SHA-256 y queda en `<datos de la app>/components/duckdb-<versión>/`.
  En macOS se guarda solo la arquitectura de la máquina.
- **Cómo lo ve el usuario:** el nodo de la conexión muestra
  «Descargando DuckDB, solo esta vez… 45 %» en lugar de «Conectando…». El
  progreso llega por el evento `component-download`
  (`dbine_driver::runtime::report_progress`).
- **Cómo se carga:** el crate `duckdb` se compila con `loadable-extension`, que
  hace pasar cada llamada a la API C por una tabla de funciones;
  `loader` la llena desde la librería con `libloading`
  (`src/api_table.rs`). El resto del driver usa el crate como siempre.
- **Sin internet:** la variable `DBINE_DUCKDB_LIB` apunta a una librería ya
  descargada.
- **Al actualizar DuckDB:** subir el crate `duckdb`, cambiar `VERSION` y los
  tamaños y SHA-256 de `ASSET` en `loader.rs`, y regenerar `api_table.rs` si no
  compila (el comando está en su encabezado).
- **macOS firmado:** si la app se firma con hardened runtime, necesita el
  entitlement `com.apple.security.cs.disable-library-validation` para cargar una
  librería firmada por otro equipo.

Un motor nuevo que dependa de una librería nativa de terceros sigue el mismo
camino: la carpeta y el progreso salen de `dbine_driver::runtime`.

## Drivers descargables

En los builds de release, cada crate de driver (salvo los de
`dbine_drivers::BUILT_IN`) se compila como un programa aparte que la app
descarga al usarlo. Un crate nuevo necesita su feature también en
`crates/dbine-plugin-host/Cargo.toml`, y un método nuevo del contrato necesita
su reenvío en `crates/dbine-plugin`. Detalles:
[`drivers-bajo-demanda.md`](drivers-bajo-demanda.md).
