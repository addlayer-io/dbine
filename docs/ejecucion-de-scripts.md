# Ejecución de scripts

Un script con varias sentencias corre sentencia por sentencia, con los mensajes
del servidor en orden y en vivo, el error en su línea y la sesión de la
pestaña intacta. Vale para todos los motores que ejecutan texto. Las
diferencias por motor están en
[`soporte-por-motor.md`](soporte-por-motor.md#ejecución-de-scripts).

## Cómo se corta el script

DBine corta el texto con un lexer único, configurado con el dialecto del motor.
Un `;` dentro de un comentario, un texto entre comillas, un identificador
entre comillas o un cuerpo (`$$ … $$`, `BEGIN … END`) nunca corta.

Cada motor además tiene su propio terminador, que DBine entiende:

| Terminador | Motores | Qué hace |
|---|---|---|
| `GO` y `GO N` | SQL Server, Azure SQL, Babelfish, ODBC con T-SQL | Termina un lote. `GO N` lo repite N veces. `GO 0` no corre nada y da el error `GO: número de repeticiones inválido` en su línea. Un `GO` dentro de un comentario o un texto no corta. |
| `DELIMITER` | MySQL y familia | Cambia el terminador; los cuerpos de rutinas quedan enteros. Manticore no tiene `DELIMITER`. |
| `/` en una línea | Oracle | Termina una unidad PL/SQL (`BEGIN`, `DECLARE`, `CREATE PROCEDURE`, función, paquete, trigger, tipo, consulta con `WITH FUNCTION`). Las demás sentencias terminan en `;` o en `/`. |
| `SET TERM` | Firebird | Cambia el terminador. Los cuerpos (`EXECUTE BLOCK`, `CREATE OR ALTER`, paquetes) también corren sin él. |
| `--#SET TERMINATOR` | Db2 | Cambia el terminador. |
| `$$`, `$tag$` | PostgreSQL y derivados, DuckDB | Cuerpos entre dólares, con o sin etiqueta. |

PostgreSQL y compatibles además cortan el script con las reglas de psql
(metacomandos, filas de `COPY`): ver
[Scripts de psql](soporte-por-motor.md#scripts-de-psql-en-postgresql-y-compatibles).

## Seguir si hay un error

Cada pestaña tiene el interruptor **Seguir si hay un error**. Apagado, el
script se detiene en la primera sentencia con error; encendido, sigue con la
siguiente. El valor inicial depende del motor:

- **Sigue:** SQL Server, Babelfish, Oracle, PostgreSQL y derivados (salvo
  CockroachDB), SQLite, Firebird, Cassandra, ScyllaDB, Couchbase, Redis,
  Valkey y Elasticsearch.
- **Se detiene:** la familia MySQL, CockroachDB, ClickHouse, Flight SQL, Trino
  y Presto, MongoDB, Neo4j, etcd, Cosmos DB y CouchDB.

Un error fatal termina el script aunque el interruptor esté encendido: en SQL
Server, severidad 20 o más (el servidor cierra la conexión, DBine abre otra y
avisa que se perdió el estado de la sesión); en MySQL, los errores 1053, 1927 y
4031; en PostgreSQL, `\connect`. Cancelar también termina el script.

Los motores que reciben el script **entero en un solo pedido** (modo "Todo el
texto de una vez": Snowflake, BigQuery, InfluxDB 2 con Flux y los ODBC que
necesitan ese modo) no pueden seguir tras un error, porque la sentencia
siguiente nunca se envió por separado. El interruptor no tiene efecto ahí.

Un error de sintaxis que impide cortar el texto (una comilla sin cerrar en
Redis, una línea inválida en Elasticsearch o en MongoDB) rechaza todo el
script antes de correr nada.

## Mensajes

La pestaña **Mensajes** muestra, en el orden en que el servidor los produjo y
mientras corre el script, no al final:

- Avisos e información del servidor: `PRINT`, `RAISERROR` hasta severidad 10,
  `NOTICE` / `WARNING` / `INFO` de PostgreSQL, `DBMS_OUTPUT` de Oracle,
  `SHOW WARNINGS` de MySQL, notificaciones de Neo4j.
- Un conteo por sentencia (`N filas afectadas`, la etiqueta del comando) y
  el tiempo que tardó.
- Los errores, con el código del motor (`Msg 50000`, `ORA-`, SQLSTATE, número
  de MySQL, código gRPC, nombre de error de Trino o Athena…), el texto del
  servidor y la **línea del script**. La línea es un enlace: un clic lleva el
  cursor a ese lugar. Cuando el servidor da la posición dentro de la sentencia
  (errores de sintaxis), la línea es la exacta; si no la da, es la primera de
  la sentencia.

## Ejecutar la sentencia bajo el cursor

`Cmd+Shift+Enter` (macOS) o `Ctrl+Shift+Enter` ejecuta solo la sentencia donde
está el cursor, delimitada con las mismas reglas del lexer.

## Transacciones

Cada pestaña tiene un modo **Auto** (cada sentencia confirma sola) o
**Manual**. Con transacciones manuales:

- Un indicador junto al editor muestra el estado: sin transacción, abierta o
  fallida (cuando el motor la deja inutilizable).
- **Confirmar** y **Deshacer** cierran la transacción. Confirmar sobre una
  transacción fallida se rechaza; deshacer funciona.
- Si cerrás la pestaña, o cambiás la base de la pestaña, con una transacción
  abierta, DBine avisa antes.
- Un `UPDATE` o `DELETE` sin `WHERE` pide confirmación antes de correr. Solo
  mira las sentencias SQL: el texto de un `PROMPT`, un comentario o el interior
  de un bloque PL/SQL no se marcan.
- En Oracle, volver de Manual a Auto confirma lo pendiente.

Qué motores la ofrecen y cómo, en
[`soporte-por-motor.md`](soporte-por-motor.md#ejecución-de-scripts).

## `USE` y cambio de base

Un `USE` (o su equivalente: `ALTER SESSION SET CURRENT_SCHEMA` en Oracle,
`SELECT n` en Redis, `:use` en Neo4j, `USE` en MongoDB y Cassandra) cambia la
base de la pestaña: la pestaña la muestra y la sigue en las ejecuciones
siguientes, sin reconectar.

## Cancelar

Cancelar usa el mecanismo nativo de cada motor y **conserva la sesión**: los
`SET`, las tablas temporales y la transacción siguen. El script se detiene y la
sentencia siguiente no corre, aunque "Seguir si hay un error" esté encendido.

Dos excepciones, donde la cancelación cierra la sesión de la pestaña y el
estado (transacción abierta, esquema o `SET`) se pierde:

- **Oracle:** cancela matando la sesión desde otra conexión (`ALTER SYSTEM KILL
  SESSION`); el cliente no tiene una llamada de interrupción. Necesita el
  privilegio `ALTER SYSTEM`; sin él, la cancelación solo se registra en el log.
- **Babelfish:** cancela con un `KILL` verificado del backend, y la app abre
  una sesión nueva.

SQL Server cancela con una atención TDS y conserva la sesión.

## Contrato (drivers)

Todo entra por `crates/dbine-driver`, con valores por defecto que mantienen el
comportamiento anterior:

- `Driver::script_dialect()`: dialecto del lexer (genérico, PostgreSQL, MySQL,
  T-SQL, Oracle, Firebird, Db2).
- `Driver::split_script(text)`: corte propio del driver; por defecto usa el
  lexer del dialecto. Devuelve unidades `ScriptStatement { text, start, end,
  line, kind, repeat }`, con `kind` en `Sql`, `Block`, `Batch` o
  `ClientCommand`.
- `Driver::script_mode()`: `PerStatement`, `Batches` o `Whole` (por defecto).
- `Driver::script_defaults()`: `continue_on_error` y `confirm_unsafe_dml`.
- `Driver::supports_manual_transactions()` y, en `Session`,
  `transaction_state`, `set_autocommit`, `commit` y `rollback`.
- Resultados: `StatementResult` con `offset`, `line`, `tag` y `elapsed`;
  mensajes ordenados `{level, text, code, line, statement}`; los errores de
  una sentencia son `Error::Statement(ScriptError { message, code, sqlstate,
  offset, line, fatal })`.
- `QueryOutcome.database`: la base en la que quedó la sesión tras un `USE`.

Los hosts de plugins responden al corte propio del driver con la llamada
`SplitScript`; un host anterior contesta `Unsupported` y la app corta con el
dialecto.
