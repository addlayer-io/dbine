# Bloqueos

El Monitor de una conexión tiene un panel **Bloqueos**: quién está esperando
a quién ahora mismo, y la opción de terminar una sesión.

## Qué muestra

- Las cadenas de bloqueo como árbol: primero cada sesión que retiene lo que
  otras esperan (la **cabeza**, con cuántas sesiones la esperan) y debajo las
  que la esperan, directa o indirectamente.
- De cada sesión: su id, usuario y cliente, base, lo que espera (el tipo de
  bloqueo o el estado, como lo informa el motor), hace cuánto, el objeto
  bloqueado y la sentencia en curso o la última.
- Una cabeza **inactiva con transacción abierta** es el caso típico: alguien
  empezó una transacción y no la terminó. El panel lo dice así.
- Sin bloqueos, el panel muestra «Sin bloqueos». Se actualiza junto con el
  resto del Monitor.

## Terminar una sesión

**Terminar**, en cada fila, pide confirmación (con la sentencia de la sesión)
y termina la sesión en el servidor: su transacción en curso se deshace.

- Cada motor usa su forma nativa: `KILL` en SQL Server y MySQL,
  `pg_terminate_backend` en PostgreSQL, `ALTER SYSTEM KILL SESSION` en
  Oracle, `killOp` / `killSessions` en MongoDB, `TERMINATE TRANSACTION` en
  Neo4j, etc.
- El id se valida antes de armar el comando.
- No se puede terminar la sesión que usa DBine para mirar.
- Las conexiones de **solo lectura** muestran los bloqueos pero no permiten
  terminar sesiones.
- Hace falta el permiso correspondiente en el servidor (por ejemplo,
  `VIEW SERVER STATE` y `ALTER ANY CONNECTION` en SQL Server). Si falta, el
  panel muestra el mensaje del servidor.

## Contrato

- `Session::blocking()` devuelve las sesiones de las cadenas
  (`BlockedSession`: id, `blocked_by`, usuario, cliente, base, espera,
  tiempo, objeto y sentencia).
- `Session::kill_session(id)` termina una.
- Cada driver declara `Capabilities::blocking` y `kill_session` según lo que
  implementa, por variante. La interfaz muestra el panel y el botón solo
  donde corresponde.

Qué soporta cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#bloqueos).
