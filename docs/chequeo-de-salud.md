# Chequeo de salud

Revisa una base y lista lo que conviene atender, ordenado por gravedad: lo
que ya es un problema, lo que puede serlo y lo que está bien. Cada hallazgo
dice qué significa, qué objetos involucra y, cuando hay una, trae una
**corrección** para abrir en una consulta.

## Dónde está

Clic derecho sobre una **base** › **Chequeo de salud**. Se abre en su propia
pestaña (**Salud**) y corre sola; **Volver a revisar** lo repite.

## Qué muestra

- Los hallazgos agrupados por gravedad: **Críticos**, **Advertencias**,
  **Información** y **Bien**. Lo que está bien se muestra al elegir
  **Mostrar lo que está bien**, para que se vea que se revisó.
- Cada hallazgo tiene su categoría (por ejemplo, Configuración, Rendimiento,
  Espacio, Actividad, Backups, Diseño), un título de una línea, el detalle de
  qué significa y qué hacer, y los **objetos** involucrados (hasta unos
  cientos; el resto, «y N más»).
- **Abrir en una consulta** abre el script de corrección. **DBine nunca lo
  ejecuta**: lo revisás y lo ejecutás vos.
- **No se pudieron revisar** lista los chequeos que no corrieron y por qué
  (falta de permisos, una versión del motor que no lo tiene…). Un chequeo
  que falla se saltea y no detiene a los demás.
- Cuándo se revisó (**Revisado el…**).

El chequeo corre en una **sesión de solo lectura** propia y se puede
cancelar.

## Chequeos comunes

Los que DBine puede responder en cualquier motor con lo que ya lee:

| Chequeo | Se ve en | Gravedad |
|---|---|---|
| `connections` | El uso de conexiones contra el máximo (donde el Monitor informa ambos). | Advertencia desde el 75 %; crítico desde el 90 %. |
| `cache_hit` | El porcentaje de aciertos de caché. | Información bajo 90 %; advertencia bajo 80 %. |
| `long_queries` | Consultas activas de más de **5 minutos** (no cuenta las de DBine ni las del sistema). | Advertencia. |
| `blocking` | Sesiones bloqueadas por otras. | Advertencia hasta 4; crítico desde 5. |
| `idle_in_transaction` | Sesiones con una transacción abierta sin actividad hace más de **10 minutos**. | Advertencia. |
| `last_backup` | El último backup que registra el motor. | Advertencia sin backups registrados o con más de **7 días**. |

Dependen de que el motor tenga Monitor, procesos y backups propios
respectivamente (ver [`bloqueos.md`](bloqueos.md), [`procesos.md`](procesos.md)
y [`backups.md`](backups.md)). El de backups usa el historial del motor: los
hechos con otras herramientas fuera del servidor no aparecen.

## Chequeos propios de cada motor

Cada driver agrega los suyos. Todos son consultas de lectura sobre el
catálogo.

| Motor | Qué revisa |
|---|---|
| SQL Server, Azure SQL | Configuración (`AUTO_SHRINK`, `AUTO_CLOSE`, `PAGE_VERIFY`, recuperación `FULL` sin backups del log, nivel de compatibilidad, VLFs), estadísticas, índices sin uso (con la ventana en que se observaron), restricciones no confiables, claves foráneas sin índice, heaps e índices deshabilitados. |
| PostgreSQL y su familia | Autovacuum apagado (global o por tabla), tuplas muertas, tablas nunca analizadas, el límite de ID de transacción (*wraparound*), índices sin uso (`idx_scan = 0`, con la ventana de los contadores), índices inválidos y duplicados, claves foráneas sin índice, tablas sin clave primaria y secuencias cerca de su límite. En CockroachDB, la recolección automática de estadísticas apagada; en Redshift, estadísticas vencidas y filas sin ordenar. |
| MySQL, MariaDB, TiDB, OceanBase | Tablas sin clave primaria, tablas MyISAM en un servidor con InnoDB, índices sin uso y redundantes, tablas fragmentadas, claves foráneas sin índice y tablas con una collation distinta de la de la base. |
| Oracle | Objetos inválidos, índices inutilizables, tablespaces casi llenos, estadísticas vencidas o faltantes, claves foráneas sin índice, tablas sin clave primaria, secuencias cerca de su límite y la papelera de reciclaje. |
| SAP HANA | Objetos inválidos, tablas columnares cuya parte *delta* necesita fusión (o con la fusión automática apagada), tablas sin clave primaria y tablas virtuales sin estadísticas. |
| Firebird | La distancia entre la transacción interesante más vieja y la siguiente, escrituras forzadas, estadísticas de índices nunca calculadas, índices inactivos y tablas sin clave primaria. |
| ClickHouse | Particiones con demasiadas partes activas, partes desprendidas o rotas, réplicas atrasadas o de solo lectura, mutaciones que fallan o no terminan y tablas grandes con fechas pero sin TTL (solo información). |
| Snowflake | Retención de Time Travel en 0, tablas grandes sin clave de clustering, tablas chicas que pagan reclustering automático, retención larga en tablas grandes, tablas borradas que Time Travel todavía retiene y *warehouses* que no se suspenden. |
| BigQuery | Tablas grandes sin particionar ni agrupar, particionadas que no exigen filtro de partición, particionadas por tiempo sin vencimiento, el modelo de cobro de almacenamiento que costaría menos y una ventana larga de *time travel*. |
| Databricks | El autoapagado del *warehouse*, la optimización predictiva apagada, tablas Delta que retienen archivos borrados mucho tiempo y tablas que no son Delta. |
| Db2 LUW (ODBC) | Objetos inválidos, tablas pendientes de reorganización o en integridad pendiente, tablas sin RUNSTATS y sin clave primaria. |
| Sybase ASE (ODBC) | Opciones de `sp_dboption` que importan para recuperación o acceso, el log compartiendo dispositivo con los datos y tablas sin ningún índice. |
| Informix, GBase 8s (ODBC) | Base sin registro de transacciones, tablas sin `UPDATE STATISTICS` y sin clave primaria. |

Los demás motores muestran solo los chequeos comunes. Los demás perfiles
ODBC (Db2 for i y z/OS, Teradata, Vertica y el perfil genérico) no tienen
chequeos propios porque lo que se necesita está en catálogos que DBine
todavía no lee.

Algunos chequeos dependen de la variante: por ejemplo, el *vacuum* y el
*wraparound* no se aplican a CockroachDB ni a YugabyteDB, cuyo almacenamiento
no los tiene; y los de índices sin uso dicen **No concluyente** si los
contadores cubren menos de 14 días.

## Contrato

Un método de `Session` con implementación por defecto:

- `health_checks(&mut self, database: &str) -> Result<Vec<HealthCheck>>`.
  Por defecto no devuelve nada y se agrega a los chequeos comunes. Un
  `HealthCheck` lleva `id`, `category`, `title`, `severity`
  (`ok`, `info`, `warning`, `critical`), `detail`, `objects` y `fix`.

Los chequeos comunes salen de `monitor()`, `processes()` y `backups()` en el
comando `database_health` (en `src-tauri/src/commands/db_health.rs`), que
recibe `connectionId`, `database` y un `runId` para cancelarlo con
`cancel_query` sobre `health:<runId>`. Ver
[`api-comandos.md`](api-comandos.md).
