# Tareas programadas

Una tarea corre sola, a la hora que elijas, **con DBine cerrado**: ejecuta un
script, exporta una consulta, compara dos esquemas o hace un backup, y avisa
con una notificación del sistema.

## Dónde está

El ícono del reloj despertador en la barra de actividad abre **Tareas
programadas**.

- **+** crea una tarea y la abre en su propia pestaña.
- Clic derecho sobre una tarea: **Abrir**, **Ejecutar ahora**,
  **Activar** / **Desactivar** y **Eliminar**. Al eliminar, la tarea deja de
  ejecutarse y se borra su historial.
- La lista muestra la próxima ejecución, el resultado de la última y, si
  corresponde, **No está programada en el sistema** o **Hay cambios sin
  aprobar**.

## La tarea

Una tarea tiene:

- un **nombre**;
- **Activa**: si está desactivada, no se programa en el sistema;
- **Cuándo**;
- **Avisar**;
- una lista de **Pasos**, que se ejecutan en orden.

### Cuándo

La hora es la local, en formato `HH:MM`.

- **Todos los días** a las `HH:MM`.
- **Algunos días** de la semana a las `HH:MM`.
- **Una vez al mes**: el día 1 a 28 de cada mes a las `HH:MM`.
- **Cada tantos minutos**: mínimo 5.

### Los pasos

Cada paso tiene un nombre opcional y qué hacer si falla:

- **Si falla, detener la tarea** (lo habitual): los pasos que siguen no corren.
- **Si falla, seguir con el próximo.**

La corrida termina en uno de tres estados:

- **Bien**: todos los pasos terminaron bien.
- **Con errores**: un paso falló y los demás siguieron.
- **Falló**: la tarea se detuvo en un paso que falló, o no pudo empezar.

Se pueden subir, bajar y quitar pasos.

## Tipos de paso

### Ejecutar un script

Corre sobre una conexión › base. El script se parte en sentencias igual que en
el editor ([`ejecucion-de-scripts.md`](ejecucion-de-scripts.md)).

- **Cada sentencia se confirma** (commit), sin importar la opción de
  autocommit de la conexión.
- **Seguir con la próxima sentencia si una falla**: opcional. Si alguna
  sentencia falla, el paso falla igual, y el error queda en el historial.
- Produce `statements`, `affected` y `errors`.

### Exportar a archivo

Escribe el resultado de una consulta en una carpeta, con el **Nombre del
archivo** que elijas (la extensión se agrega sola). Formatos: CSV, CSV con
punto y coma, CSV para Excel, TSV, Excel (.xlsx), JSON, JSON Lines, XML y SQL
(INSERT). Las opciones son las del diálogo de exportación (por ejemplo,
**Nombres de columna en la primera fila**).

- **Siempre corre en solo lectura**, aunque la conexión permita escribir.
- Si falla o se cancela, el archivo a medio escribir se borra.
- Produce `file` y `rows`.

### Comparar esquemas

Compara una **Base de referencia** con una **Base a comparar**, con las
opciones **Ignorar mayúsculas**, **Emparejar sin el esquema**, **Ignorar
comentarios** e **Incluir borrados** (las de
[`comparacion-de-esquemas.md`](comparacion-de-esquemas.md)).

- Si son iguales, el paso termina bien y no escribe nada.
- Si difieren, **guarda en un archivo `.sql` el script de sincronización**
  que dejaría la base a comparar como la de referencia, y levanta un aviso
  (ver [Avisar](#avisar)).
- **El script nunca se ejecuta.** Revisarlo y aplicarlo es una decisión de
  una persona.
- Sin **Incluir borrados**, los objetos que están solo en la base a comparar
  no se borran: el script los menciona en un comentario.
- Produce `differences` y, si hubo diferencias, `file`.

### Backup

- **Backup del motor:** el backup propio del motor, con las mismas opciones
  que la pestaña **Backups** ([`backups.md`](backups.md)). Corre el script
  del motor en el servidor; los archivos quedan donde los deja el servidor.
- **Copia de DBine:** un script con la estructura y, si se marca **Con los
  datos**, las filas, como el de la pestaña **Backups**. Se guarda en una
  **Carpeta**, con el **Nombre del archivo** que elijas.

Los motores sin backups propios ofrecen solo la copia de DBine, así que
cualquier motor se puede respaldar.

Las opciones **secretas** de un backup del motor (por ejemplo, una contraseña
de cifrado) **se guardan aparte, en el llavero del sistema**, nunca en la
tarea. Al volver a abrir la tarea el campo queda vacío y dice **Se guarda
aparte, en el llavero**; si lo dejás vacío, se conserva la contraseña que ya
estaba.

La copia produce `file` y `rows`.

## Variables

En los nombres de archivo, las carpetas y los scripts se pueden usar:

| Variable | Valor |
|---|---|
| `{task}` | el nombre de la tarea |
| `{date}` | `AAAA-MM-DD` |
| `{time}` | `HHMMSS` |
| `{datetime}` | `AAAA-MM-DD_HHMMSS` |
| `{year}`, `{month}`, `{day}` | las partes de la fecha |
| `{steps.N.clave}` | lo que produjo el paso número `N` |

El nombre por defecto de un archivo es `{task}-{datetime}`. Los caracteres que
algún sistema no admite en un nombre (`/ \ : * ? " < > |`) se reemplazan por
`_`.

Lo que produce cada paso:

| Paso | Claves |
|---|---|
| Ejecutar un script | `statements`, `affected`, `errors` |
| Exportar a archivo | `file`, `rows` |
| Comparar esquemas | `differences`, `file` |
| Copia de DBine | `file`, `rows` |

Ejemplo: un paso 1 que exporta y un paso 2 que guarda una copia como
`copia-{steps.1.rows}` genera `copia-1204.sql` si el paso 1 exportó 1204
filas. Una variable que no existe se deja tal cual, sin reemplazar.

## Avisar

- **Si falla o encuentra diferencias** (por defecto): avisa cuando la corrida
  no terminó **Bien**, o cuando un paso levantó un aviso (la comparación que
  encontró diferencias).
- **Siempre.**
- **Nunca.**

Es una notificación del sistema, porque con DBine cerrado no hay ventana donde
mostrarla: el Centro de notificaciones en macOS, un toast en Windows y
`notify-send` en Linux. Si el sistema no la puede mostrar, queda en el log y
nada más.

## Los cambios se aprueban

Una tarea con pasos **Ejecutar un script** que **cambian datos o estructura**
corre sola, sin que nadie la mire. Por eso hay que aprobarla.

- Al guardar, DBine muestra **Esta tarea cambia datos o estructura**, con cada
  paso, su conexión › base y la primera sentencia que escribe (`DELETE`,
  `ALTER`…). **Aprobar y guardar** la deja lista.
- En los motores que no usan SQL no se puede distinguir una lectura de una
  escritura: **cualquier script cuenta** como cambio.
- Las conexiones con la etiqueta `prod`, `production`, `producción` o `prd`
  se marcan como **PROD**.
- Si después se edita la conexión, la base o el script de uno de esos pasos,
  la tarea **no los ejecuta** hasta que se apruebe de nuevo. La lista muestra
  **Hay cambios sin aprobar** y la corrida falla diciendo que hay que abrir
  la tarea y volver a guardarla. Cambiar la hora, el nombre o los demás pasos
  no pide aprobar otra vez.
- Una conexión de **solo lectura** sigue siendo de solo lectura en la tarea y
  no necesita aprobación.
- Exportar, comparar y hacer backups no piden aprobación: no cambian la base
  (la comparación escribe un script en un archivo, no lo ejecuta).

## Cómo corre con DBine cerrado

Al guardar, DBine registra la tarea en el programador del sistema. A la hora
indicada, el sistema inicia DBine en un **modo sin ventanas**: carga la
tarea, la ejecuta y sale. En ese modo no arranca nada más de la app (ni el
servidor MCP, ni el actualizador, ni la telemetría, ni la sincronización en la
nube).

La entrada del sistema lleva **solo el id de la tarea**, nunca conexiones ni
contraseñas.

| Sistema | Qué se registra | Condiciones |
|---|---|---|
| macOS | Un LaunchAgent en `~/Library/LaunchAgents` | Corre mientras el usuario tiene la sesión iniciada. DBine tiene que estar en **Aplicaciones**: se niega a programar desde la imagen de disco o desde una copia temporal que macOS haya hecho. |
| Windows | El Programador de tareas, bajo la carpeta `DBine` | "Solo cuando el usuario ha iniciado sesión". Si la computadora estaba apagada, corre al encenderla. |
| Linux | Un timer de usuario de systemd; si no hay systemd, una línea de `crontab` | Con cron, el llavero puede no estar accesible: una tarea que necesita contraseñas guardadas puede fallar. |

- Al iniciar, la app **vuelve a registrar** las tareas activas. Eso cubre que
  DBine se haya movido de carpeta o actualizado.
- Si el sistema rechaza la tarea, se guarda igual y la lista dice **No está
  programada en el sistema**, con el motivo.
- En una versión de desarrollo no se registran tareas.

### Contraseñas

Las tareas usan las **contraseñas guardadas** de las conexiones. Una conexión
que no guarda su contraseña **no puede correr sin atención**.

En macOS, después de una actualización el sistema puede volver a pedir acceso
al llavero. Como no hay nadie para contestar, la corrida falla a los 45
segundos con un mensaje que pide abrir DBine una vez y elegir **Permitir
siempre**. Esto dejará de pasar cuando las versiones se firmen con un
Developer ID estable.

## Ejecutar ahora

**Ejecutar ahora** corre la tarea dentro de la app, en segundo plano, y la
registra en el historial igual que una corrida programada. Con cambios sin
guardar en la pestaña, el botón queda deshabilitado (**Guardá los cambios
antes de ejecutarla**).

## Historial

Cada tarea guarda sus **últimas 200 corridas**. De cada una se ve:

- el estado (**En curso**, **Bien**, **Con errores**, **Falló**);
- si fue **programada** o **manual**;
- el resumen de cada paso, por ejemplo cuántas filas se exportaron y a qué
  archivo;
- los mensajes del servidor (`PRINT`, avisos) y los errores.

**No se guardan** los resultados ni las filas de datos, ni las contraseñas.

## Solo en esta máquina

Las tareas apuntan a las conexiones y las carpetas de esta máquina. **No
forman parte del backup ni de la sincronización en la nube**, y restaurar un
backup no las borra.

Qué soporta cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#tareas-programadas).

## Contrato

Las tareas no agregan métodos a los drivers: cada tipo de paso usa lo que ya
existe.

- **Ejecutar un script:** el ejecutor de scripts de la consulta.
- **Exportar:** `Session::execute` con el exportador de resultados, en una
  sesión de solo lectura.
- **Comparar esquemas:** la carga del esquema y `Driver::sync_script`.
- **Backup del motor:** `Driver::backup()` y `Driver::backup_script()`.
  **Copia de DBine:** `list_objects` y la generación de scripts.

El modelo (`ScheduledTask`, `Step`, `TaskRun`) está en
`crates/dbine-core/src/tasks.rs`. Cada tipo de paso es una función en
`src-tauri/src/tasks/steps.rs` que recibe su configuración en JSON, así que un
tipo nuevo no cambia el modelo ni las tablas.
