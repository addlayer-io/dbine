# Migrar una base a otro motor

**Migrar…**, en el menú de una base, abre una pestaña para pasar sus tablas y
sus datos a otra base, aunque sea de otro motor (por ejemplo, de SQL Server a
PostgreSQL). La conversión la hace `crates/dbine-schema`; los detalles por
motor están en [conversion-de-esquemas.md](conversion-de-esquemas.md).

Para obtener solo el script, sin ejecutarlo, está **Generar script…**. Por
defecto usa el motor de la base, pero se puede elegir otro (ver más abajo).

## Tres modos

Después de elegir el motor de destino se elige el modo. Solo se habilitan
los que sirven para ese par de motores; los otros muestran por qué no.

- **Migrar (convertir):** entre cualquier par de motores. Convierte la
  estructura al motor de destino, crea las tablas y copia los datos. Es lo
  que se describe en el resto de esta página.
- **Clonar (dejar el destino idéntico):** entre dos bases del mismo motor
  (hoy SQL Server y Azure SQL, y PostgreSQL con los derivados que mantienen
  su catálogo). En lugar de la conversión, el propio driver escribe el
  script que deja el destino igual al origen: esquemas, tipos, tablas con su
  almacenamiento y particiones, índices, restricciones, secuencias, vistas,
  rutinas, triggers y demás objetos, adaptado a lo que el servidor de
  destino soporta. La vista previa muestra ese script y lo que no se pudo
  dejar idéntico. La corrida crea las tablas, copia los datos con el motor
  de transferencia (cada tabla con sus índices apenas termina) y al final
  ejecuta el resto del script; una sentencia que depende de otra se
  reintenta en pasadas y las que siguen fallando se informan.
- **Sincronizar solo lo que cambió:** entre dos bases del mismo motor que
  lo permite (SQL Server y PostgreSQL). Las tablas tienen que existir en el
  destino con las mismas columnas: no se crea ni se vacía nada. Cada tabla
  se compara por su clave primaria, o por una clave única que se elige en la
  vista previa; las que no tienen ninguna se listan con el motivo. Se
  elige qué comparar:
  - **Completa:** todo el contenido de cada fila.
  - **Solo tamaños de columnas grandes:** las columnas grandes se comparan
    por su longitud; mucho más rápido, pero no ve un cambio que deja el
    mismo tamaño.
  - **Solo claves:** encuentra las filas nuevas y las borradas, no las
    modificadas.

  También los núcleos del servidor que puede usar el resumen (0: decide el
  servidor). El resultado muestra, por tabla, las filas insertadas,
  actualizadas y borradas, y las notas de cada sincronización (por ejemplo,
  una clave foránea que quedó sin confianza).

## La pestaña Migrar

1. **Destino:**
   - El motor de destino. Los que solo sirven como origen (Drill, InfluxDB,
     NetSuite…) aparecen deshabilitados, con el motivo.
   - La **conexión de destino**, obligatoria para migrar, y su base. Elegir
     el motor no conecta nada: la conexión se abre cuando se la elige. Una
     conexión de solo lectura no se acepta como destino.
   - **Esquemas**, en los motores que los tienen:
     - Por defecto, cada tabla queda en su esquema de origen y los esquemas
       se crean en el destino.
     - El esquema por defecto del origen pasa al del destino: `dbo` de SQL
       Server va a `public` de PostgreSQL, y al revés.
     - Si se completa "Esquema único", todas las tablas van a ese esquema.
       Si hay nombres repetidos, se desambiguan con un sufijo corto y el
       reporte lo avisa.
2. **Tablas:** todas marcadas de entrada, con un filtro.
3. **Opciones:**
   - copiar los datos;
   - índices y claves foráneas;
   - adaptar mayúsculas y minúsculas de los nombres al destino;
   - borrar antes las tablas que existan en el destino (`DROP`);
   - `IF EXISTS` / `IF NOT EXISTS`;
   - **opciones avanzadas** de la copia: tablas a la vez (8 por defecto, de 1
     a 32), orden (las más grandes primero, las más chicas primero o
     alfabético) y cada cuántas filas se confirma (100.000 por defecto).
4. **Vista previa:** el reporte (Omitido, Pérdida, Aviso, Info), la conversión
   de cada columna y el script de estructura. No toca nada.
5. **Migrar:** pide confirmación y ejecuta sobre la conexión de destino, en
   tres etapas:
   1. **Estructura.** Los esquemas que falten; el `DROP`, si se pidió, en
      pasadas (una tabla de la que dependen otras se borra cuando esas ya no
      están; si igual no se puede borrar, la migración se detiene antes de
      crear nada); y el `CREATE` de cada tabla **con sus columnas y su clave
      primaria**. Si falla una tabla, se informa y se sigue con las demás, sin
      copiar sus datos.
   2. **Datos**, con el motor de transferencia masiva
      ([transferencia-masiva.md](transferencia-masiva.md)):
      - varias tablas a la vez, las más grandes primero (según el catálogo
        del origen en SQL Server, PostgreSQL y MySQL);
      - por cada tabla, la vía más rápida disponible: copia directa dentro
        del driver cuando origen y destino son el mismo driver y lo permite,
        la carga masiva del destino, o `INSERT` por lotes con los ajustes de
        carga de su driver (`IDENTITY_INSERT`, secuencias…);
      - cada conexión es propia de su tabla, y la del origen es de solo
        lectura;
      - antes de copiar se verifican las columnas del destino; una tabla que
        ya existía y tiene filas no se toca;
      - **los índices de cada tabla se crean apenas termina su copia**,
        mientras las demás siguen copiando;
      - las columnas calculadas y `rowversion` del destino no se cargan.
   3. **Restricciones.** Las claves foráneas de las tablas copiadas y, de
      SQL Server a SQL Server, la identidad de cada tabla que la tiene queda
      en el mismo valor que en el origen (`DBCC CHECKIDENT … RESEED`). Si algo
      falla, se informa.

### Durante y después de la corrida

Una grilla con una fila por tabla: estado, filas copiadas sobre las
estimadas con su barra, filas por segundo, la vía usada (copia directa,
carga masiva o `INSERT` por lotes), el cuello de botella (origen o destino)
y el error, si hubo. Por tabla se puede cancelar o, si está en cola,
ejecutar ya mismo sin esperar lugar.

Arriba, el progreso total, el tiempo transcurrido, las tablas a la vez
(se puede cambiar en vivo: bajarlo no corta las que están copiando),
**Cancelar todo** y, al terminar, **Reintentar las que fallaron**.

### Reanudar

Cada corrida guarda su estado: el de cada tabla en `dbine-transfer.sqlite`
y el de la migración en `migrations/<id>.json`, junto al archivo de estado
de la app. Si la app se cierra durante una migración, al volver a abrir la
pestaña **Migrar** de esa base aparece en **Interrumpidas**, con
**Reanudar** y **Descartar**:

- una tabla ya copiada no se vacía nunca: solo se terminan sus índices;
- una tabla copiada a medias se vacía (`TRUNCATE TABLE`, o `DELETE` en los
  motores que no lo tienen) y se copia de nuevo;
- si se cortó mientras se creaban las tablas, la migración empieza de nuevo;
- las claves foráneas y la identidad que faltaban se completan al final.

Probado de SQL Server a PostgreSQL (identity, `bit`, `uniqueidentifier`,
`money`, `datetime2`, unicode, comillas, NULL, FK e índice): los conteos
coinciden y las secuencias quedan después del último id copiado. De SQLite
a SQLite, en las pruebas de `src-tauri` (copia, índices, reintento de una
tabla que falló y reanudación después de un corte).

## Migraciones guardadas

Cada base tiene en el explorador un nodo **Migraciones**, al lado de
**Queries**, con las migraciones que se **iniciaron desde esa base** (solo
debajo del origen, nunca debajo del destino). Cada una guarda todo lo que
hace falta para volver a abrirla:

- **Borradores:** la configuración de la pestaña Migrar (motor de destino,
  modo, conexión y base de destino, tablas elegidas, opciones, esquema de
  destino, opciones avanzadas de la transferencia, profundidad y claves de la
  sincronización…) se guarda sola mientras se arma. La entrada aparece con el
  primer cambio (elegir un motor, destildar una tabla…), no por abrir la
  pestaña; si se deja a medias, no se pierde nada.
- **Ejecuciones:** al migrar, clonar o sincronizar (y al retomar o reintentar)
  la entrada queda enlazada con esa corrida. Si se vuelve a correr, la nueva
  pasa a ser la actual y las anteriores quedan en su historial.
- **Nombre:** automático (`→ <conexión de destino> · <base> · <modo>`) mientras
  no se lo cambie; se renombra desde el título de la pestaña o con
  **Renombrar…**.
- **Estado**, con su ícono: borrador, en curso, terminada, con errores,
  interrumpida o cancelada (el de su última corrida).

Al abrirla (clic o doble clic) se abre su propia pestaña Migrar, con la
configuración cargada y editable (los cambios se guardan en la misma
entrada) y el panel **Ejecución** mostrando la última corrida: sus tablas con
estado, filas y errores, y sus notas. Si esa corrida sigue en curso en la
app, el panel la sigue en vivo; si quedó interrumpida o se canceló, ofrece
**Retomar**, y si fallaron tablas, **Reintentar las que fallaron**. Se pueden
tener abiertas varias migraciones de la misma base a la vez, una pestaña cada
una.

Menú de una migración: **Abrir**, **Renombrar…**, **Duplicar** (la misma
configuración como un borrador nuevo, sin corridas) y **Eliminar del
listado**, que solo quita la entrada: no toca ni el origen ni el destino ni
los registros de las corridas. En el nodo **Migraciones** (y en el menú de la
base, **Migrar…**) está **Nueva migración…**.

Las migraciones guardadas viven en el archivo de estado junto a las queries,
se borran con su conexión y viajan en la sincronización en la nube como las
queries. Las corridas, en cambio, son de la máquina que las ejecutó: en otra
máquina la entrada muestra su configuración y avisa que no tiene ejecuciones
en esa máquina.

## Generar script para otro motor

En **Generar script…**, "Motor del script" arranca en el motor de la base. Si
se elige otro:

- las tablas se convierten y su DDL lo escribe el driver de ese motor;
- los datos (si se piden) salen como `INSERT` de ese motor;
- las vistas, rutinas y triggers no se convierten; el script lo aclara en un
  comentario, junto con las pérdidas de la conversión;
- el script se abre en una conexión de ese motor o se guarda en un archivo.

En cualquier motor, el script pone las claves foráneas **después** de los
datos, así la carga no falla por una fila padre que todavía no está.

## Qué falta

- Vistas, procedimientos y triggers al convertir entre motores distintos
  (al clonar, sí).
- En los drivers que no leen por columnas (la lectura por defecto hace un
  `SELECT *`), una tabla con columnas que la conversión deja afuera no se
  copia bien: la lectura trae todas las columnas.

## Comandos

Cada comando recibe un solo `args`.

| Comando | args | Devuelve |
|---|---|---|
| `migration_targets` | `{ source_connection_id }` (opcional) | `[{ id, name, family, supported, reason, clone, sync }]` |
| `migration_plan` | `{ connection_id, database, tables, target_driver, options, mode, sync, target }` | `{ tables, columns, issues, script, available, sync_tables }` |
| `migration_run` | lo mismo que `migration_plan`, más `{ migration_id, target_connection_id, target_database, transfer }` | la corrida (ver abajo) |
| `migration_set_parallel` | `{ run_id, n }` | las tablas a la vez que quedaron |
| `migration_cancel_table` | `{ run_id, table }` | `true` si la encontró |
| `migration_run_now` | `{ run_id, table }` | `true` si estaba en cola |
| `migration_cancel` | `{ run_id }` | — |
| `migration_runs` | `{ limit, ids }` (`ids`: solo esas corridas, sin límite) | `[{ id, status, stage, created_at, finished_at, source_connection_id, source_database, target_connection_id, target_database, target_driver, parallel, resumable, tables, foreign_key_errors, after_errors, notes, mode }]` |
| `migration_resume` | `{ run_id }` | la corrida |
| `migration_retry_failed` | `{ run_id }` | la corrida |
| `migration_forget` | `{ run_id }` | — |
| `list_saved_migrations` | `{ connection_id, database }` | `[{ id, connection_id, database, name, config, run_ids, created_at, updated_at }]`, las más nuevas primero |
| `get_saved_migration` | `{ id }` | la migración guardada |
| `save_saved_migration` | `{ migration }` (crea o actualiza; `id` vacío = uno nuevo) | la migración guardada |
| `rename_saved_migration` | `{ id, name }` | la migración guardada |
| `link_saved_migration_run` | `{ id, run_id }` | la migración guardada (la corrida pasa a ser la actual) |
| `duplicate_saved_migration` | `{ id, name }` | la copia, como borrador |
| `delete_saved_migration` | `{ id }` | — |

- `options` es `{ fold_case, target_schema, drop, if_exists, indexes, foreign_keys, data, keep_schemas }`.
- `mode` es `convert` (por defecto), `clone` o `sync`. `clone` y `sync` de
  `migration_targets` dicen, para el motor de la conexión de origen dada,
  `{ available, reason }`.
- `sync` es `{ depth, max_cores, keys }`: `depth` es `Full`, `Sizes` o `Keys`, y
  `keys` una lista de `{ schema, name, columns }` con la clave elegida por tabla.
  `sync_tables` trae por tabla `{ schema, name, primary_key, unique_keys, key, reason }`.
- `target` es `{ connection_id, database }`: la vista previa del clonado le
  pregunta a esa conexión qué soporta (solo la consulta).
- `transfer` es `{ parallel, order, commit_rows }` (`order`: `largest_first`, `smallest_first` o `alphabetical`).
- `config` de una migración guardada es el formulario de la pestaña tal cual:
  `{ target_driver, mode, target_connection_id, target_database, tables, options, sync, sync_keys, transfer }`
  (`tables`: `null` = todas). `run_ids` son sus corridas, de la más vieja a la
  más nueva.
- `tables` es una lista de `{ schema, name }`; vacía significa todas las tablas.
- La corrida es `{ run_id, status, tables, foreign_key_errors, after_errors, notes, elapsed_ms, cancelled, mode }`
  (`after_errors`: las sentencias finales del clonado que no se pudieron ejecutar);
  `status` es `running`, `interrupted`, `done`, `failed` o `cancelled`, y cada tabla es
  `{ name, source, target, status, rows_done, rows_total, path, stats, attempts, error }`.
- El progreso llega en el evento `migration-progress`, siempre con el `id` de la corrida:
  - los eventos del motor tal cual (`run_started`, `table_started`, `table_phase`,
    `table_progress`, `table_done`, `table_failed`, `table_cancelled`, `run_finished`, `log`);
  - `{ event: "plan", tables, parallel }` al empezar la copia;
  - `{ event: "step", phase, table, done, total }` en la estructura y las restricciones
    (`phase`: `schemas`, `drop`, `create`, `foreign_keys`, `identity`, `script`, `before`,
    `check`, `after` o `done`).
