# Backups

Clic derecho sobre una base › **Backups…** abre una pestaña con todo lo de
backups de esa base: el historial, hacer uno nuevo y restaurar. En los
motores cuyos backups son de todo el servidor (Redis, los snapshots de
Elasticsearch…), la opción también está en el menú de la conexión.

La pestaña tiene dos partes.

## Copias de DBine

Sirven para **todos los motores**. Una copia es un script con la estructura
y, si se pide, los datos de la base, guardado en un archivo local.

- **Nueva copia:** se elige el archivo y si lleva los datos. Por defecto va a
  `Documentos/DBine/Backups/<conexión>/<base>-<fecha>.sql`.
  - El script borra y vuelve a crear cada objeto (`DROP … IF EXISTS` y
    `CREATE`). Después vienen los datos (`INSERT` en lotes, en la sintaxis
    del motor) y, al final, los índices y las claves foráneas.
  - Es lo mismo que **Generar script…** con todos los objetos marcados.
  - Se ve el avance y se puede cancelar. Si se cancela o falla, el archivo a
    medio escribir se borra.
- **Historial:** fecha, si tiene datos, cantidad de objetos y de filas,
  tamaño y ruta del archivo. Queda en el estado local de esta máquina: no
  viaja con la sincronización.
- **Restaurar:** ejecuta el script en la base elegida, que puede ser la
  misma u otra. Es lo mismo que **Ejecutar archivo…**.
  - Pide confirmación, porque reemplaza los objetos de la copia que ya
    existan en esa base.
  - Se puede seguir aunque falle una sentencia; los errores se muestran al
    final.
- **Eliminar:** saca la copia de la lista y, si se elige, también borra el
  archivo.

## Backups del servidor

Son los que hace el propio motor (`BACKUP DATABASE` en SQL Server,
`BACKUP … TO Disk(…)` en ClickHouse, los snapshots de Elasticsearch…),
donde existen:

- **Historial:** lo que el servidor tiene registrado (fecha, tipo, base,
  tamaño, ubicación y estado), en los motores que lo informan.
- **Hacer backup, Restaurar y Eliminar:** cada motor pide sus opciones (tipo,
  destino, compresión, repositorio…). Con esas opciones DBine arma el
  **script en el lenguaje del motor**, lo muestra y lo ejecuta solo con el
  clic en **Ejecutar**. El script se puede copiar para correrlo en otro lado.
- Los archivos quedan donde los deja el servidor, no en esta máquina. La
  pestaña avisa qué necesita cada motor (por ejemplo, un disco de backups
  configurado en ClickHouse o un repositorio registrado en Elasticsearch).
- Estos scripts no se guardan en el historial de consultas.

Las conexiones de **solo lectura** muestran el historial, pero no ofrecen
hacer backups ni restaurar.

Qué soporta cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#backups).

## Contrato

- `Driver::backup()` dice qué ofrece el motor (`BackupSpec`): las opciones de
  un backup y de una restauración, y si puede restaurar, borrar y listar el
  historial. También dice si los backups son de todo el servidor, en qué
  base se ejecutan los scripts y una nota para la pestaña. `None`: solo las
  copias de DBine.
- `Session::backups(base)` lee el historial del servidor.
- `Driver::backup_script(acción)` escribe el script de un backup, una
  restauración o un borrado.

Las copias de DBine no pasan por el driver: usan la generación de scripts
(`generate_script`) y la ejecución de archivos (`run_script_file`).
