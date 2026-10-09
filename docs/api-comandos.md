# Comandos Tauri: diseño, scripts, importación y bases

Contrato entre el backend (`src-tauri/src/commands/`) y la UI. Todos los
comandos reciben un único objeto `args` con campos en snake_case, igual que el
resto de la API (`web/src/api/client.ts`). Los tipos de Rust están en
`crates/dbine-driver/src/schema.rs`.

## Datos del driver (`list_drivers`)

Cada driver incluye, además de lo que ya informaba:

- `capabilities`: `{ create_database, drop_database, foreign_keys, monitor }`
- `designer`: `DesignerSpec | null`. Qué crea el diseñador, sus tipos de datos
  y sus opciones propias.
- `create_templates`: `[{ kind, label, template }]`. En el texto, `{schema}` y
  `{name}` los reemplaza la UI.
- `script_separator`: texto que va entre objetos en un script (`GO`, `/` o `""`).

## Estructura y DDL

| Comando | args | Devuelve |
|---|---|---|
| `database_schema` | `{ connection_id, database }` | `TableSchema[]` |
| `table_ddl` | `{ connection_id, table: TableSchema, parts: DdlParts }` | `string` |
| `insert_script` | `{ connection_id, target: ObjectRef, columns: string[], rows: Cell[][] }` | `string` |

| `update_script` | `{ connection_id, target: ObjectRef, changes: [{ key: [[col, valor]], set: [[col, valor]] }] }` | `string` |

`DdlParts` es `{ drop, if_exists, create, indexes, foreign_keys }`, todos booleanos.

`update_script` convierte las celdas editadas en la grilla en código del motor:
`UPDATE … WHERE <clave>` en SQL, `updateOne` en MongoDB, etc. Cómo funciona:

- `key` identifica la fila: su clave primaria o, si la tabla no tiene, todas las
  columnas con su valor original.
- `set` trae los valores nuevos.
- DBine solo genera el código: lo agrega a la query o lo abre en una nueva, y el
  usuario decide si lo ejecuta.
- Un motor que no puede actualizar filas devuelve `bad_request` con el motivo.

## Datos de una tabla con filtros (`filtered_browse_query`)

| Comando | args | Devuelve |
|---|---|---|
| `filtered_browse_query` | `{ connection_id, database, object: ObjectRef, limit, filters: ColumnFilter[] }` | `{ query, server_side, reason }` |

Es la fila de filtros que aparece bajo los encabezados de la grilla, en la
vista de datos de una tabla. `ColumnFilter` es `{ column, op, values, sql }`:

- `op` es uno de estos:
  - comparaciones: `eq`, `ne`, `gt`, `ge`, `lt`, `le`;
  - texto: `contains`, `not_contains`, `starts_with`, `ends_with`;
  - nulos y vacíos: `is_null`, `not_null`, `is_empty`, `not_empty`;
  - listas: `in`, `not_in`;
  - booleanos: `is_true`, `is_false`, `true_or_null`, `false_or_null`;
  - SQL a mano: `sql` (la condición entera) y `sql_right` (lo que sigue a la
    columna).
- `values` lleva los valores con su tipo: los números viajan como números.
- Los filtros se combinan con AND.

El driver agrega los filtros a su propia consulta de exploración
(`Driver::filtered_browse`): un `WHERE` en SQL y CQL, el documento de filtro
en Mongo, etc. Si no puede, devuelve `server_side: false` junto con la
consulta sin filtrar y el motivo. En ese caso la UI filtra las filas ya
cargadas y lo avisa en la barra de filtros.

Lo que se escribe en la caja de filtro de cada columna:

| Escrito | Significa |
|---|---|
| `texto` | contiene (texto); igual (números y booleanos) |
| `=x`, `<>x`, `!=x`, `>x`, `>=x`, `<x`, `<=x` | comparación |
| `1,2,3` o `=a,b` | cualquiera de esos valores |
| `!x`, `^x`, `x$` | no contiene, empieza con, termina con |
| `NULL`, `NOT NULL`, `EMPTY`, `NOT EMPTY` | nulos y vacíos |
| `2024-05-01` en una fecha | ese día entero |
| `true` / `false`, `1` / `0`, `sí` / `no` en un booleano | verdadero o falso |

El menú ⋮ de cada columna ofrece las mismas operaciones según su tipo, más
"Filtrar varios valores…" y las dos condiciones SQL.

## Importar conexiones de otras herramientas

| Comando | args | Devuelve |
|---|---|---|
| `import_connections_detect` | — | `{ [source]: ruta \| null }`: dónde está cada herramienta en esta máquina |
| `import_connections_scan` | `{ source, path?, text? }` | `{ path, items, warnings }` |
| `import_connections_apply` | `{ source, path?, text?, keys, passwords }` | `{ imported, folders, failed: [nombre, motivo][] }` |

`source` es uno de `dbeaver`, `dbgate`, `datagrip`, `azure_data_studio`,
`ssms` o `url`. Con `url`, lo que se lee es `text`, con una conexión por
línea. Las rutas de abajo son las de macOS; en Windows van en `%APPDATA%` y en
Linux en `~/.config`.

| Herramienta | Qué se lee | Contraseñas |
|---|---|---|
| DBeaver | El `.dbeaver/data-sources*.json` de cada proyecto del workspace (`~/Library/DBeaverData/workspace6`). Los proyectos que no son `General` pasan a ser una carpeta. | Sí: `credentials-config.json`, cifrado con la clave fija de DBeaver |
| DbGate | `~/.dbgate/connections.jsonl`, sin las conexiones sin guardar | Sí: `crypt:…`, con la clave de `~/.dbgate/.key` |
| DataGrip / JetBrains | Las globales (`JetBrains/<IDE><versión>/options/dataSources.xml`) y las de los proyectos recientes de cada IDE (`.idea/dataSources.xml`), sin duplicados. El usuario sale de `dataSources.local.xml`. | Sí, del llavero del sistema (`IntelliJ Platform DB — <uuid>`), solo al importar; el sistema puede pedir permiso |
| Azure Data Studio | `azuredatastudio/User/settings.json` (JSON con comentarios): `datasource.connections` y los grupos como carpetas | No: quedan en su propio almacén y se piden al conectar |
| SSMS | Los servidores registrados (`Microsoft/SQL Server Management Studio/<versión>/RegSrvr.xml`) o un `.regsrvr` exportado, con sus grupos y colores | No: están protegidas con la cuenta de Windows |
| URL | `postgres://…`, `mysql://…`, `mongodb+srv://…`, `redis(s)://…`, `sqlserver://…`, `jdbc:…`, `Server=…;Database=…` o la ruta de un archivo SQLite o DuckDB | Las que trae la URL |

Cómo se importa:

- **Contraseñas:** nunca llegan a la UI. `scan` solo informa:
  - `has_secret`, si la conexión trae una;
  - `keychain`, si está en el llavero de la otra herramienta.

  `apply` vuelve a leer el origen y las pasa directo al llavero de DBine.
  Con `passwords: false`, o si no hay, se piden al conectar.
- **Carpetas:** se reutilizan las que ya existen con el mismo nombre.
- **Conexiones repetidas:** `existing` avisa si ya hay una con el mismo
  motor, host, puerto, base y usuario. La UI no las marca por defecto.
- **Lo que no se importa:**
  - el túnel SSH;
  - la autenticación integrada de Windows, IAM o Entra ID con MFA;
  - los motores sin driver en DBine o que se configuran con credenciales de
    la nube: BigQuery, Spanner, Athena, Databricks, DynamoDB, Cosmos DB y
    Azure Data Explorer.

  Lo que se importa sin alguno de esos datos lo avisa en `notes`; lo que no
  se puede importar vuelve con `unsupported`.
- **Primer arranque:** si alguna herramienta tiene conexiones para importar,
  la app lo ofrece una vez. La respuesta queda en la preferencia
  `import.suggested`, que viaja con la sincronización.

## Comparar esquemas

Cómo funciona desde la UI: `docs/comparacion-de-esquemas.md`.

| Comando | args | Devuelve |
|---|---|---|
| `schema_compare_load` | `{ connection_id, database, schemas? }` | `{ driver, tables: TableSchema[], objects: CodeObject[], warnings }` |
| `schema_compare` | `{ left: DbModel, right: DbModel, options: { ignore_case, ignore_schema, ignore_comments } }` | `{ tables: TableDiff[], objects: ObjectDiff[] }` |
| `schema_compare_convert` | `{ from_driver, to_driver, tables, target_schema }` | `{ tables, warnings }` |
| `schema_sync_script` | `{ connection_id, tables: TableChange[], objects: ObjectChange[], views: CodeObject[] }` | `{ statements, warnings }` |
| `schema_sync_run` | `{ connection_id, database, statements, run_id, atomic? }` | `{ done, failed: [índice, error] \| null, rolled_back }` |

Los tipos:

- `CodeObject` es `{ kind, schema, name, definition }`: vistas, vistas
  materializadas, procedimientos, funciones y triggers, con su código.
- `schema_compare` es puro, sin conexión. La UI lo vuelve a llamar sobre sus
  copias después de cada cambio que pasa de un lado al otro.
- `TableDiff` tiene:
  - `key`;
  - `left` y `right`, índices en cada modelo o `null`;
  - `status`: `equal`, `changed`, `only_left` u `only_right`;
  - `columns`, `indexes` y `foreign_keys`, como `ItemDiff[]`;
  - `primary_key`, un estado;
  - `fields`: las propiedades de la tabla que difieren.
- `ItemDiff` tiene `name`, `left`, `right`, `status` y `fields`. `fields`
  lista lo que difiere: `type`, `nullable`, `default`, `auto_increment`,
  `comment`, `columns`, `unique`, `kind`, `filter`, `on_delete`, `on_update`.
- `TableChange` es uno de:
  - `{ op: "create", table }`;
  - `{ op: "drop", table }`;
  - `{ op: "alter", old, new }`.
- `ObjectChange` es `{ op: "create" | "drop" | "replace", object }`.
- `views` son las vistas del destino como van a quedar. Las que usan tablas
  cuyas columnas cambian de tipo o se borran se agregan solas al script: se
  borran antes y se recrean después.

El script lo arma el driver del destino (`Driver::sync_script`). Los motores
SQL usan el planificador común `dbine_driver::alter::sync_script` con su
`AlterStyle`. Los motores que no pueden aplicar cambios tienen
`supports_schema_sync: false` en `list_drivers`, y la UI deshabilita
"Sincronizar" con el motivo.

`schema_sync_run`:

- Rechaza las conexiones de solo lectura.
- Usa una sesión propia (`sync:<run_id>`), que `cancel_query` puede cortar.
- Ejecuta cada sentencia por separado y se detiene en la primera que falla.
- Con `atomic: true`, si el driver tiene transacciones manuales y su
  `RenameSpec` dice `transactional`, corre todo en una transacción: confirma
  al final y deshace ante un error o una cancelación (`rolled_back: true`).
  En los demás casos `atomic` no cambia nada.

## Renombrar con impacto

Cómo funciona desde la UI: [`renombrar.md`](renombrar.md). Qué ofrece cada
driver llega en `list_drivers` como `rename` (un `RenameSpec` o `null`).

| Comando | args | Devuelve |
|---|---|---|
| `rename_impact` | `{ connection_id, database, target: RenameTarget, new_name, keep_view_columns? }` | `RenameImpact` |
| `rename_script` | `{ connection_id, database, request: RenameRequest, rewrites: { object: CodeObject, schemabound }[] }` | `{ statements, warnings }` |

El script corre con `schema_sync_run` y `atomic: true`.

- `RenameTarget` es uno de `{ what: "object", object, parent? }`,
  `{ what: "column", table, column }`, `{ what: "index", table, index }`,
  `{ what: "constraint", table, constraint }` o
  `{ what: "schema", database?, schema }`.
- `RenameRequest` es `{ target, new_name, table?, definition? }`; `table` y
  `definition` vienen de `RenameImpact`.
- `RenameImpact` tiene `items` (`{ dependent, action, original }`),
  `scanned`, `unreadable`, `note`, `spec_note`, `collides`, `quoted_name`,
  `atomic`, `definition` y `table`.
- `action` es uno de:
  - `{ kind: "engine" }`: una clave, un índice o un check;
  - `{ kind: "tracked" }`: un objeto que el motor sigue solo;
  - `{ kind: "manual", reason, unresolved? }`, con `reason` `dynamic`,
    `unreadable`, `not_rewritten` o `no_match`;
  - `{ kind: "rewrite", object, edits, unresolved, schemabound, default_selected }`.
- `edits` son `{ line, before, after }`; `unresolved` son
  `{ line, text, reason }`, con `reason` `in_string`, `qualified`,
  `other_schema`, `case`, `maybe_function`, `ambiguous_column` o
  `alias_named_like_schema`.
- `rename_impact` rechaza un nombre vacío o igual al actual, y los motores o
  los objetos que el driver no renombra.
- `rename_script` es puro: no toca el servidor. Pone el renombre del driver en
  el medio, antes los dependientes que se borran (`drop_create` y los de
  SCHEMABINDING) y después los demás, ordenados para que cada uno se cree
  después de lo que usa.

## Bases de datos

| Comando | args | Devuelve |
|---|---|---|
| `create_database` | `{ connection_id, name, options? }` | `void` |
| `create_database_script` | `{ connection_id, name, options? }` | `string` |
| `create_database_choices` | `{ connection_id }` | `FieldChoices[]` |
| `drop_database` | `{ connection_id, name }` | `void` |
| `drop_objects` | `{ connection_id, database, objects: ObjectRef[] }` | `{ dropped: ObjectRef[], errors: [ObjectRef, string][] }` |

`options` es un mapa `key` → valor con las opciones avanzadas del motor
(`DriverInfo.create_database_fields`); si falta o está vacío, la base se crea
solo con el nombre. `create_database_script` devuelve lo que ejecutaría
`create_database` (SQL, o la llamada a la API en BigQuery, Couchbase y otros
motores HTTP) y falla si el nombre está vacío. `create_database_choices`
devuelve las sugerencias del servidor (`{ key, default, values }`) y una lista
vacía si no tiene. Si un paso posterior a la creación falla, el error dice que
la base se creó y cuál fue el paso. Detalle: [`crear-bases.md`](crear-bases.md).

`drop_objects` borra tablas y colecciones con el DDL del driver (`table_ddl`
con `drop`). En los motores SQL también borra vistas, rutinas y triggers.
Trabaja en pasadas, así que un objeto del que dependen otros se borra cuando
esos ya no están. No acepta conexiones de solo lectura. En el explorador se
usa desde "Eliminar…" en el menú de un objeto, o desde "Eliminar N objetos…"
con varios seleccionados (Cmd/Ctrl+clic suma o saca, Shift+clic toma un
rango); con varios, pide escribir «eliminar» para confirmar.

## Monitor del servidor

| Comando | args | Devuelve |
|---|---|---|
| `monitor_snapshot` | `{ connection_id }` | `MonitorSnapshot` |
| `monitor_processes` | `{ connection_id }` | `ServerProcess[]` |
| `monitor_cancel_query` | `{ connection_id, id }` | nada |
| `monitor_kill_session` | `{ connection_id, id }` | nada |

`monitor_processes` es la lista de [Procesos](procesos.md), solo para los
drivers con `capabilities.processes`; sondea por una sesión propia
(`processes:<id>`) y, si la conexión se cae, la próxima llamada reconecta.
`monitor_cancel_query` (`capabilities.cancel_query`) detiene la sentencia de
otra sesión y la deja abierta; `monitor_kill_session` la termina. El `id` es
el que informa `monitor_processes`. Ninguno tiene eventos ni se cancela: son
llamadas cortas. La interfaz las oculta en las conexiones de solo lectura.

Solo para los drivers con `capabilities.monitor`. Usa una sesión propia de la
conexión (`monitor:<id>`), así el sondeo no espera detrás del explorador ni de
una consulta en curso; si la conexión se cae, la próxima llamada reconecta.

`MonitorSnapshot` = `{ metrics, tables, info, notes }`:

- `metrics`: `{ key, label, group, unit, value, max, counter }`. `unit` es
  `percent`, `bytes`, `count`, `millis` o `seconds`. Si `counter` es `true`,
  el valor es un total acumulado desde que arrancó el servidor, y la UI
  muestra la tasa por segundo entre dos lecturas.
- `tables`: `{ key, title, columns, rows }` (sesiones, consultas en curso,
  bloqueos, bases y tamaños, nodos…).
- `info`: pares `[etiqueta, valor]` (versión, rol, parámetros).
- `notes`: lo que el motor no puede informar y por qué.

## Script de la base (generador y exportación)

`generate_script`:

- **args:**
  - `script_id`
  - `connection_id`
  - `database`
  - `objects: ObjectRef[]`
  - `options: { drop, if_exists, create, indexes, foreign_keys, definitions, data, data_limit: number | null }`
  - `path: string | null`
- **Destino:**
  - con `path`, escribe el archivo y va avisando el progreso;
  - sin `path`, devuelve el texto para abrirlo en el editor (con un tope de tamaño).
- **Devuelve:** `{ script: string | null, objects: number, rows: number }`
- **Orden del script:**
  1. los DROP, si se pidieron;
  2. todas las tablas: CREATE e índices;
  3. las definiciones: vistas, rutinas y demás objetos, en el orden en que llegan;
  4. los datos, si se pidieron;
  5. las claves foráneas, después de los datos, para que la carga no falle por
     una fila padre que todavía no está (o que quedó afuera por el tope de
     filas);
  6. los triggers, al final, para que no se disparen al restaurar los datos.
- **Otro motor:** con `target_driver` (un driver distinto al de la base) las
  tablas se convierten con `dbine-schema` y el driver de destino escribe el
  DDL y los INSERT. Las vistas, rutinas y triggers no se convierten: el
  script lo aclara en un comentario, junto con las pérdidas de la conversión.
- **Progreso:** evento `script-progress` con `{ id, done, total, current }`.
- **Cancelar:** `cancel_query` con `session_id = "script:<id>"`.

## Importación de archivos

`preview_import_file`:

- **args:** `{ path, format, options: { delimiter, header, sheet } }`
- **`format`:** `auto | csv | csv_semicolon | tsv | json | json_lines | xlsx | xml`
- **Devuelve:** `{ format, columns: [{ name, inferred_type }], rows: Cell[][], sheets: string[] }`
  - `rows` trae las primeras 50 filas;
  - `inferred_type` es `integer | number | boolean | date | datetime | text`.

`import_file`:

- **args:**
  - `import_id`
  - `connection_id`
  - `database`
  - `path`
  - `format`
  - `options`
  - `target: ObjectRef`
  - `create_table: TableSchema | null`: la crea antes de importar;
  - `mapping: [{ source, target }]`
  - `batch: number`
- **Devuelve:** `{ rows, elapsed_ms }`
- **Progreso:** evento `import-progress` con `{ id, rows }`.
- **Cancelar:** `cancel_query` con `session_id = "import:<id>"`.

## Ejecutar un archivo de script (restaurar un volcado)

`run_script_file`:

- **args:** `{ run_id, connection_id, database, path, continue_on_error }`
- **Devuelve:** `{ statements, errors: string[], elapsed_ms }`
- **Cómo lo lee:** en partes, separando las sentencias según el driver (`GO` en SQL Server).
- **Progreso:** evento `script-run-progress` con `{ id, statements, bytes, total_bytes }`.
- **Cancelar:** `cancel_query` con `session_id = "run:<id>"`.

## Backups

Ver [backups.md](backups.md). `list_drivers` trae `backup` (`BackupSpec` o
`null`) por driver.

- `backup_list` `{ connection_id, database }` → `{ copies, native, native_error }`:
  - `copies`: las copias de DBine de esa base. Con `database` vacío, las de
    toda la conexión.
  - `native`: el historial del servidor, cuando el motor lo informa.
  - `native_error`: el error al leer el historial. Las copias igual se
    devuelven.
- `backup_default_path` `{ connection_id, database }` → la ruta sugerida
  para una copia nueva.
- `backup_copy` `{ backup_id, connection_id, database, objects, data, path }`
  → la copia guardada (`BackupCopy`):
  - **Progreso:** evento `script-progress` con `id = backup_id`.
  - **Cancelar:** `cancel_query` con `session_id = "script:<backup_id>"`.
- `backup_copy_delete` `{ id, delete_file }`: saca la copia de la lista y,
  con `delete_file`, también borra el archivo.
- `backup_script` `{ connection_id, action }` → el script del motor:
  - `action`: `{ action: "backup", database, options }`,
    `{ action: "restore", source, database, options }` o
    `{ action: "delete", source }`.
  - La UI lo ejecuta con `execute_query` en `script_database` (o en la base
    de la pestaña), sin guardarlo en el historial.
- Para restaurar una copia se usa `run_script_file`.

## Preferencias

| Comando | args | Devuelve |
|---|---|---|
| `list_settings` | — | `{ [clave]: valor }` |
| `set_setting` | `{ key, value }` | `void` |

- `value` en `null` borra la preferencia y vuelve al valor por defecto.
- Las claves `local.*` son de la máquina y no se aceptan.
- Claves en uso: `grid.copyFormat`, `query.maxRows`.

## Drivers descargables

Ver [`drivers-bajo-demanda.md`](drivers-bajo-demanda.md).

| Comando | args | Devuelve |
|---|---|---|
| `drivers_packages` | — | `{ on_demand, packages: [{ package, label, version, drivers, size, installed, available, previous, status: { kind, … }, min_app_needed }] }` |
| `drivers_install` | `{ package }` | `void` (el progreso llega por `component-download`) |
| `drivers_remove` | `{ package }` | `void` |
| `drivers_check_updates` | — | `void` (busca el índice ya; las versiones nuevas de los drivers instalados se bajan en segundo plano y avisan por `drivers-changed`) |
| `drivers_rollback` | `{ package }` | `void` (descarta la versión en uso y las conexiones nuevas usan la anterior) |

- `on_demand` es `false` en los builds que traen todos los drivers adentro
  (desarrollo). En ese caso la lista viene vacía.
- `installed` son los bytes en disco, o `null` si todavía no se descargó.

## Sincronización en la nube

Cómo funciona: `docs/sincronizacion.md`. Los proveedores son
`google_drive | onedrive | folder`.

| Comando | args | Devuelve |
|---|---|---|
| `sync_status` | — | `{ config, providers, status, dirty, last_sync_at }` |
| `sync_connect` | `{ provider, folder }` | `{ account, remote: { updated_at, device, app_version } \| null }` |
| `sync_cancel_connect` | — | `void` |
| `sync_setup` | `{ passphrase, mode: "upload" \| "restore" }` | `SyncAction` |
| `sync_now` | — | `SyncAction` |
| `sync_upload_now` | — | `SyncAction` |
| `sync_restore_now` | — | `SyncAction` |
| `sync_set_auto` | `{ auto }` | `void` |
| `sync_set_passphrase` | `{ passphrase }` | `void` |
| `sync_change_passphrase` | `{ current, new }` | `SyncAction` |
| `sync_disconnect` | `{ delete_remote }` | `void` |
| `sync_local_backups` | — | `[{ path, updated_at, device, size }]` |
| `sync_restore_local` | `{ path }` | `void` |

Qué hace cada uno:

- `sync_connect`: inicia sesión en el navegador (OAuth) o valida la carpeta, y
  busca un backup existente. Todavía no sincroniza nada.
- `sync_setup`: hace la primera subida o restauración y después guarda la frase
  en el llavero.
- `sync_now`: la sincronización automática (sube, restaura o resuelve un
  conflicto).
- `sync_upload_now`: reemplaza el backup con lo de esta máquina.
- `sync_restore_now`: reemplaza lo de esta máquina con el backup.
- `sync_set_passphrase`: guarda en esta máquina la frase cambiada en otra. Antes
  la verifica contra el backup.
- `sync_change_passphrase`: vuelve a cifrar el backup con una sal nueva.
- `sync_disconnect`: olvida la cuenta y la frase en esta máquina y, si se pide,
  borra el backup.
- `sync_local_backups`: lista las copias guardadas antes de cada restauración.

`SyncAction` es uno de estos:

- `{ action: "up_to_date" }`
- `{ action: "uploaded", previous_kept }`
- `{ action: "downloaded", local_backup, device }`

Errores: `wrong_passphrase` (la frase no abre el backup), `sync_auth` (hay que
conectar la cuenta de nuevo) y `sync` (falla del proveedor o de la carpeta).

Eventos:

- `sync-status`, con `{ running, last_error, last_error_kind, last_action, last_run_at }`.
- `sync-applied`, cuando una restauración reemplazó el estado local: la UI
  vuelve a cargar conexiones, queries y preferencias.

## Calidad de código ([`calidad-de-codigo.md`](calidad-de-codigo.md))

| Comando | args | Devuelve |
|---|---|---|
| `lint_script` | `{ connection_id, sql }` | `LintFinding[]` |
| `lint_rules` | — | `Rule[]` |

- `LintFinding`: `{ rule, severity, start, end, line, params }`. `start` y
  `end` son índices de cadena de JS (UTF-16), `line` es la línea de `start`
  (desde 1) y `params` trae los valores que muestra el mensaje.
- `severity` es `error`, `warning` o `info`.
- `Rule`: `{ id, severity, groups }`. `groups` son los grupos de motores a los
  que se aplica (`sql`, `tsql`, `postgres`, `mysql`, `oracle`, `influxql`,
  `cql`, `mongodb`, `couchdb`, `search`, `redis`, `etcd`, `cypher`).
- Es solo análisis de texto: no consulta la base. Los textos de cada regla
  están en la interfaz.

## Documentar la base ([`documentar-la-base.md`](documentar-la-base.md))

| Comando | args | Devuelve |
|---|---|---|
| `dbdocs_outline` | `{ connection_id, database }` | `{ schemas, kinds, foreign_keys, dependencies }` |
| `dbdocs_generate` | `{ connection_id, database, run_id, path, options }` | `{ path, tables, objects, bytes, notes }` |
| `dbdocs_open` | `{ path, reveal }` | `void` |

- `dbdocs_outline`: lo que ofrece el diálogo. `kinds` es la cantidad de
  objetos por tipo.
- `options`: `{ format: "html" | "markdown", schemas, tables, views, routines,
  triggers, others, source, indexes, foreign_keys, dependencies, diagram,
  labels }`. `labels` son los textos del documento en el idioma de la
  interfaz; sin ellos, español.
- `dbdocs_generate` corre en una sesión de solo lectura. Agrega la extensión
  del formato si el nombre no la tiene.
- `dbdocs_open` abre el archivo (o lo muestra en su carpeta con
  `reveal: true`) y solo acepta archivos que escribió esta ejecución de DBine.

Evento: `dbdocs-progress`, con `{ run_id, done, total, phase }` (`phase` es
`objects`, `schema`, `columns`, `source` o `writing`).
Cancelar: `cancel_query` con `session_id: "docs:<run_id>"`.

## Constructor de consultas ([`constructor-de-consultas.md`](constructor-de-consultas.md))

| Comando | args | Devuelve |
|---|---|---|
| `build_query` | `{ connection_id, spec, session_id? }` | `{ sql, warnings, features }` |
| `preview_built_query` | `{ connection_id, spec, session_id }` | `{ sql, columns, rows, truncated, elapsed_ms }` |

- `spec`: `{ database, tables, joins, columns, distinct, limit }`.
- `features` es lo que el motor ofrece (uniones, `GROUP BY`, `HAVING`,
  agregados, `DISTINCT`, `ORDER BY`, límite, grupos `OR`, operadores); la
  interfaz oculta el resto. `warnings` son avisos de lo que el motor no hace.
- `preview_built_query` trae hasta 100 filas en una sesión de solo lectura.
  Cancelar: `cancel_query` con `session_id: "qb-preview:<session_id>"`.

## Subconjunto de datos ([`subconjunto-de-datos.md`](subconjunto-de-datos.md))

| Comando | args | Devuelve |
|---|---|---|
| `subset_plan` | `SubsetArgs` | `SubsetPlan` |
| `subset_run` | `SubsetArgs` + `{ masks, confirm, seed? }` | `SubsetReport` |

- `SubsetArgs`: `{ run_id, connection_id, database, table, filter, children,
  target_connection_id, target_database }`. `filter` es `{ expression,
  columns, limit }`, con `limit` `{ kind: "all" }`, `{ kind: "rows", count }` o
  `{ kind: "percent", percent }`. `children` es `{ depth, max_rows }` o
  `null`.
- `masks`: por tabla (`{ schema, name, columns }`), una regla por columna:
  `keep`, `fake` (con `kind`), `shift_date` (`days`), `noise` (`percent`),
  `fixed` (`value`), `null` o `hash`.
- `subset_plan` solo lee (origen y destino). Devuelve las tablas en orden de
  escritura, el total de filas, los ciclos cortados, las notas y
  `confirm_label` (el texto a escribir si el destino es de producción).
- `subset_run` rechaza un destino de solo lectura y, si es de producción,
  una `confirm` distinta de `confirm_label`. Sin `seed`, usa una al azar por
  corrida.
- `SubsetReport`: `{ tables, notes, elapsed_ms, cancelled }`. Cada tabla trae
  `status`: `done`, `error`, `cancelled` o `skipped`.

Evento: `subset-progress`, con `{ runId, phase, table, rows, total }`
(`phase`: `read`, `collect`, `create`, `insert`, `cycles`, `constraints`,
`done`). Cancelar: `cancel_query` con `session_id: "subset:<run_id>:src"` y
`"subset:<run_id>:tgt"`.

## Optimizar consulta ([`optimizar-consulta.md`](optimizar-consulta.md))

| Comando | args | Devuelve |
|---|---|---|
| `optimizer_analyze` | `{ connection_id, database, sql, run_id }` | `Analysis` |
| `optimizer_ai` | `{ connection_id, database, sql, run_id, provider, model?, plans }` | `{ candidates, none, sent }` |
| `optimizer_compare` | `{ connection_id, database, run_id, versions, runs?, max_rows? }` | `Measure[]` |
| `optimizer_cancel` | `{ run_id }` | `void` |

- `Analysis`: `{ language, dialect, engine, writes, supports_explain,
  candidates, notes, hints, warnings, plans, cost, skipped }`.
- `Candidate`: `{ id, source, rule, params, title, explanation, sql, verify }`
  con `source` `rule`, `ai` o `user`.
- `versions` de `optimizer_compare`: `[{ id, sql }]`, la original primero.
  `runs` va de 1 a 20 (3 por defecto) y `max_rows` es 100000 por defecto.
- `Measure`: `{ id, executed, error, runs_ms, min_ms, avg_ms, rows, truncated,
  checksum, equivalent, cost, plans, plan_error }`. `equivalent` es `null`
  cuando no se pudo verificar.
- La consulta que escribe datos no se ejecuta: solo se mide su plan estimado.
- `optimizer_ai` envía la consulta, la estructura de las tablas y un resumen
  del plan; nunca filas.

Evento: `optimizer-progress`, con `{ run_id, measure }`, uno por versión.
Cancelar: `optimizer_cancel`, o `cancel_query` con
`session_id: "optimize:<run_id>"`.

## Correo de las tareas programadas ([`tareas-programadas.md`](tareas-programadas.md#configuración--correo))

| Comando | args | Devuelve |
|---|---|---|
| `mail_settings_get` | — | `{ settings, password_saved }` |
| `mail_settings_save` | `{ settings, password? }` | `{ settings, password_saved }` |
| `mail_test` | `{ settings, password?, to }` | `string` |

- `settings`: `{ host, port, security, user, from_address, from_name }` con
  `security` `starttls`, `tls` o `none`. `settings` es `null` si todavía no
  hay servidor guardado.
- La contraseña va al llavero. Si `password` viene vacío, se conserva la
  guardada; sin `user`, se borra.
- `mail_test` usa el formulario tal cual, guardado o no, y devuelve el
  resumen del envío.

## Buscar en la base ([`busqueda.md`](busqueda.md))

| Comando | args | Devuelve |
|---|---|---|
| `search_database` | `{ connection_id, database, search_id, query, names?, code? }` | `{ hits, scanned, unreadable, truncated, cancelled, from_catalog }` |

- `query`: `{ text, case_sensitive, whole_word, kinds, max_hits }`. Sin
  `kinds`, busca en todos los tipos con código; con `"column"`, en los
  nombres de columna. Sin `max_hits`, corta en 2000.
- Un hit es `{ kind, schema, name, parent, line, text }`. `line` 0 es una
  coincidencia en el nombre; en una columna, `parent` es la tabla y `text`
  su tipo.

Evento: `code-search-progress`, con `{ search_id, done, total, hits }` (los
hits nuevos). Cancelar: `cancel_query` con `session_id: "search:<search_id>"`.

## Chequeo de salud ([`chequeo-de-salud.md`](chequeo-de-salud.md))

| Comando | args | Devuelve |
|---|---|---|
| `database_health` | `{ connection_id, database, run_id }` | `{ checks, checked_at, skipped }` |

- Un chequeo es `{ id, category, title, severity, detail, objects, fix }`, con
  `severity` `ok`, `info`, `warning` o `critical`. Vienen ordenados del más
  grave al menos grave.
- `fix` es un script que la interfaz abre en una consulta; nunca se ejecuta
  solo.
- Cancelar: `cancel_query` con `session_id: "health:<run_id>"`.

## Datos de prueba ([`datos-de-prueba.md`](datos-de-prueba.md))

| Comando | args | Devuelve |
|---|---|---|
| `datagen_preview` | `DataGenArgs` | `{ table_columns, generators, columns, rows }` |
| `datagen_run` | `DataGenArgs` | `{ rows, elapsed_ms }` |

- `DataGenArgs`: `{ connection_id, database, table, columns, rows, seed?,
  gen_id, batch }`. Cada elemento de `columns` es `{ name, generator, params,
  null_percent }`; las columnas que no se nombran van en `auto`.
- `datagen_preview` devuelve hasta 20 filas de muestra. `datagen_run` acepta de
  1 a 10.000.000 filas, inserta de a `batch` (1 a 5000) y rechaza las
  conexiones de solo lectura.

Evento: `datagen-progress`, con `{ id, rows, total }`. Cancelar:
`cancel_query` con `session_id: "datagen:<gen_id>"`.

## Propiedades de la base ([`propiedades-de-la-base.md`](propiedades-de-la-base.md))

| Comando | args | Devuelve |
|---|---|---|
| `database_properties` | `{ connection_id, database }` | `DatabaseProperties` |
| `alter_database_script` | `{ connection_id, database, changes }` | `string` |
| `alter_database` | `{ connection_id, database, changes }` | `void` |

- `DatabaseProperties`: `{ fields, values, info, choices, warnings }`.
- `changes` es campo → valor nuevo, solo lo que cambió. Sin cambios,
  `alter_database` no hace nada. `alter_database_script` devuelve el script
  que se muestra antes de aplicar.
