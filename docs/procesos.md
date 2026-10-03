# Procesos

El Monitor de una conexión tiene un selector **Panel / Procesos**. **Procesos**
es la lista de lo que el servidor está haciendo ahora mismo: sesiones y
sentencias en curso, con la opción de cancelar una consulta o terminar una
sesión.

## Qué muestra

- Una fila por proceso. Qué es un «proceso» depende del motor: en la mayoría
  es una sesión (con su sentencia en curso, si tiene); en los motores que no
  tienen sesiones es una consulta o una tarea en curso (ver
  [Según el motor](#según-el-motor)).
- Las columnas **aparecen solo si el motor las informa**: sesión, estado,
  usuario, host, programa, base, comando, duración, CPU, lecturas,
  escrituras, espera, bloqueada por y sentencia. Lo que un motor no sabe
  queda vacío, y si ninguna fila trae un dato, la columna no se muestra.
- La sesión con la que DBine lista los procesos se marca como **DBine**.
- Los procesos propios del servidor (checkpointer, replicación, tareas
  internas) se marcan como del sistema.
- Ordenás por cualquier columna con un clic en su encabezado.
- El contador muestra «N de M»: las filas visibles contra el total.

## Actualización

- La lista consulta cada **2, 5, 10 o 30 s** (el mismo selector de intervalo
  del Monitor) y se puede **pausar**.
- Sondea por una conexión propia, así que no espera detrás de una instantánea
  del panel ni de una consulta tuya en curso. Si la conexión se cae, la
  próxima lectura reconecta.
- Mientras se ve la lista, el **panel deja de sondear**; al volver a **Panel**
  retoma.

## Filtros

- **Solo activos**: los que están ejecutando algo ahora (no inactivos ni
  dormidos).
- **Ocultar los del sistema**: saca los procesos propios del servidor.
- **Base**, **usuario** y **host**: listas con los valores presentes en ese
  momento («Todas las bases», «Todos los usuarios», «Todos los hosts»).
- **Búsqueda de texto** sobre las filas.

Los filtros se combinan.

## Bloqueos

Una sesión que otras esperan se resalta con **bloquea N** (cuántas esperan).
Es la cabeza de una cadena: el detalle la muestra completa (ver abajo). El
panel [Bloqueos](bloqueos.md) sigue siendo la vista por cadenas; acá la
información aparece en la lista.

## Detalle de una fila

Al elegir una fila se abre su detalle:

- La **sentencia entera** (no cortada como en la tabla). Si el proceso no
  ejecuta nada, lo dice.
- La **cadena de bloqueo**: a quién espera y quiénes lo esperan.
- **Abrir en una consulta**: pone la sentencia en una pestaña de consulta
  nueva, sin ejecutarla.
- **Copiar**: la copia al portapapeles.
- **Cancelar consulta** y **Terminar sesión** (abajo).

## Cancelar consulta y terminar sesión

- **Cancelar consulta** detiene la sentencia que ejecuta la sesión y **deja
  la sesión abierta**.
- **Terminar sesión** cierra la sesión en el servidor y **deshace su
  transacción en curso**.

Las dos:

- piden confirmación, con el id y el usuario de la sesión;
- no aparecen en las conexiones de **solo lectura**;
- siguen la comprobación de permisos de `kill_session`: si al usuario le
  falta el permiso en el servidor, el botón se deshabilita y dice cuál falta;
- se rechazan para la sesión propia de DBine (la que lista los procesos).

Cada botón aparece solo donde el motor tiene la acción (ver
[`soporte-por-motor.md`](soporte-por-motor.md#procesos)).

## Según el motor

- **SQL Server, Azure SQL, Fabric y Babelfish** solo **terminan sesiones**:
  `KILL` es lo único que existe y cierra la sesión. Muestran la sentencia
  parametrizada tal como la ve el servidor; Fabric no trae el texto. Una
  sesión inactiva con transacción abierta muestra su último lote.
- **Oracle** cancela con `ALTER SYSTEM CANCEL SQL` (18c o posterior). Si la
  sesión es de **otra pestaña de DBine** se niega a cancelarla (la biblioteca
  cliente nunca retorna de esa interrupción y la pestaña quedaría colgada) y
  manda a usar **Terminar sesión** o a cancelar desde esa pestaña.
- **ksqlDB**: cancelar una consulta **persistente** la **pausa** (`PAUSE`);
  se retoma con `RESUME`. Terminarla (`TERMINATE`) la detendría para
  siempre. Una consulta *push* sí termina, para su cliente.
- **TDengine** solo **cancela**: sus conexiones son el pool compartido de
  taosAdapter, así que cerrar una cortaría a otros clientes. Ninguna fila se
  marca como propia de DBine.
- **Snowflake**: el sondeo **mantiene encendido el warehouse** (la función de
  historial lo necesita). DBine no reanuda uno suspendido: si está apagado,
  la lista no lo despierta. **Terminar sesión** cierra la sesión que ejecuta
  la consulta elegida.
- **Redis, Valkey y Dragonfly** solo ven como «en ejecución» los clientes
  parados en un comando bloqueante (`BLPOP`, `XREAD BLOCK`…): Redis ejecuta un
  comando a la vez. Cancelar es `CLIENT UNBLOCK` (el comando falla, la
  conexión sigue); no trae los argumentos del comando. Dragonfly no tiene
  `CLIENT UNBLOCK`: no cancela.
- **Neo4j y Memgraph** no pueden detener una consulta y conservar su
  transacción: cancelar es `TERMINATE TRANSACTION`, que la deshace (la
  conexión del cliente sigue abierta).
- **MongoDB**: cancelar una operación en curso es `killOp`; una sesión
  inactiva con transacción abierta se termina con `killSessions`.
- **Firebird** muestra solo las conexiones del usuario si éste no es SYSDBA /
  `RDB$ADMIN` ni tiene `MONITOR_ANY_ATTACHMENT`.
- **CouchDB** lista tareas (indexado, compactación, replicación); solo se
  cancelan las replicaciones transitorias. Una definida en un documento de
  `_replicator` se detiene cambiando ese documento.
- **Los motores centrados en consultas** no tienen sesiones: listan **consultas
  en curso**, no sesiones. Son ClickHouse, Trino, Presto, Starburst,
  BigQuery, Athena, Databricks, Dremio, Drill, Spanner, Couchbase y
  Elasticsearch / OpenSearch (tareas). Para ellos, «Terminar sesión» no
  existe y «Cancelar consulta» detiene la consulta elegida. Drill corta el
  texto a 150 caracteres.

## Contrato

- `Session::processes()` devuelve `Vec<ServerProcess>`: `id`, `status`,
  `active`, `system`, `own`, `user`, `host`, `program`, `database`,
  `command`, `elapsed_ms`, `cpu_ms`, `reads`, `writes`, `wait`, `blocked_by`
  y `sql`. Cada motor completa lo que informa. El `id` es el que toman
  `cancel_query` y `kill_session`.
- `Session::cancel_query(id)` detiene la sentencia y deja la sesión.
- `Session::kill_session(id)` termina la sesión (el mismo método de
  [Bloqueos](bloqueos.md)).
- Capacidades: `Capabilities::processes`, `cancel_query` y `kill_session`,
  según lo que implementa cada motor (por variante o versión cuando cambia).
  La interfaz muestra la lista y cada botón solo donde corresponde.
- Comandos Tauri: `monitor_processes`, `monitor_cancel_query` y
  `monitor_kill_session`, en [`api-comandos.md`](api-comandos.md#monitor-del-servidor).

Qué soporta cada motor y por qué falta en los demás está en
[`soporte-por-motor.md`](soporte-por-motor.md#procesos).
