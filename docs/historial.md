# Historial de consultas

Todo lo que se ejecuta desde el editor queda en el historial: la vista con el
reloj en la barra de actividad. Tiene dos vistas: **Esta pestaña**, la
historia de la pestaña activa, y **Todas las ejecuciones**, el historial
completo de la máquina. La vista elegida se recuerda.

## Esta pestaña

Muestra la línea de tiempo de la pestaña donde estás parado, de lo más nuevo
a lo más viejo y agrupada por día. Sigue a la pestaña activa: al cambiar de
pestaña se recarga con la historia de la otra.

- En una **consulta guardada**:
  - **Guardado**: las versiones de su texto, con las líneas agregadas y
    quitadas respecto de la versión anterior.
  - **Ejecución**: cada vez que se ejecutó, con la duración, las filas y una
    marca roja si falló.
- En un **archivo de un proyecto**:
  - **Ejecución**: lo que se ejecutó desde ese archivo.
  - Los **commits** de git que lo tocaron (mensaje, hash corto y autor),
    siguiendo los renombres: si el archivo tenía otro nombre en ese commit,
    se indica.
- Las demás pestañas (datos de una tabla, diagramas, etc.) no tienen línea de
  tiempo.
- Los botones de arriba filtran por tipo (Versiones, Ejecuciones, Commits).

Un **clic** en una versión o un commit abre la comparación con el texto
actual del editor (a la izquierda el de entonces, a la derecha el de ahora),
con **Restaurar esta versión**:

- En una consulta guardada, el texto actual se guarda primero como versión
  y después se reemplaza por el restaurado, que también queda guardado. No
  se pierde nada: el texto anterior sigue en la línea de tiempo y el editor
  lo puede deshacer.
- En un archivo, el texto restaurado queda en el editor como cambio sin
  guardar; se escribe en el disco con ⌘S.

Una ejecución se abre como siempre: **doble clic** o **Enter** la abre en una
consulta nueva. Con **clic derecho** también se puede comparar su texto con
el actual o copiarlo; sobre un commit, copiar su hash.

### Cuándo se guarda una versión

- Al guardar a mano (⌘S), al ejecutar la consulta y al cerrar su pestaña,
  siempre que el texto haya cambiado.
- Mientras se escribe, el guardado automático deja como mucho una versión
  por minuto.
- El texto que tenía la consulta antes de editarla también queda, así que
  siempre se puede volver al estado de antes de un cambio.
- Se guardan todas las versiones de los últimos 7 días; de ahí hasta los 90
  días, una por día (la última de cada día); las más viejas se borran. Como
  mucho 300 versiones por consulta.
- Las versiones quedan solo en esta máquina, como el historial: no viajan con
  la sincronización en la nube. Se borran al borrar la consulta.

## Todas las ejecuciones

- Las ejecuciones, de la más nueva a la más vieja, **agrupadas por el
  servidor** donde corrieron: el host de la conexión o, en los motores de
  archivo, el nombre del archivo.
- Cada ejecución muestra la hora, la base, la cantidad de filas (devueltas o
  afectadas, cuando el motor lo informa), la duración y el texto. Las que
  fallaron llevan una marca roja con el error.
- La búsqueda filtra por el texto, el servidor, la base o el nombre de la
  conexión.

### Qué se puede hacer

- **Doble clic** o **Enter**: abre el texto en una consulta nueva de esa
  conexión y base.
- **Clic derecho**: abrir en una consulta nueva, copiar el SQL o borrar la
  entrada. Sobre un servidor: borrar todo su historial.
- El tacho de la vista borra todo el historial.

## Qué se guarda

- Lo que se ejecuta desde el editor, incluidos los planes de ejecución. No se
  guardan las lecturas que hace la app sola: cargar los datos de una tabla,
  el explorador, el Profiler o el Monitor.
- Se guardan las últimas 20.000 ejecuciones; las más viejas se borran solas.
- Queda solo en esta máquina, en el archivo de estado local. No viaja con la
  sincronización en la nube.
- El texto se guarda tal cual se ejecutó, incluso si contenía una contraseña
  (por ejemplo, un `CREATE USER … PASSWORD`). En esos casos conviene borrar
  la entrada.
- El nombre de la conexión se guarda con la entrada, así sigue legible si la
  conexión se renombra o se borra. Para volver a abrirla, la conexión tiene
  que existir.

## Comandos

- `execute_query` con `record: true` guarda la ejecución.
- `history_list { search?, before?, limit? }` devuelve las entradas, de la
  más nueva a la más vieja. `before` es el id de la última de la página
  anterior.
- `history_delete { ids }`: `null` borra todo.
- `history_of { queryId?, projectId?, filePath?, limit? }`: las ejecuciones
  de una consulta guardada o de un archivo de un proyecto. `execute_query`
  las marca con `query_id`, o con `project_id` y `file_path`.
- `save_query { query, checkpoint? }`: con `checkpoint` el texto siempre
  queda como versión; sin él, como mucho una por minuto.
- `query_versions { queryId }` (sin el texto), `query_version { id }` (con
  el texto) y `query_version_checkpoint { queryId }`, que guarda el texto
  actual como versión si cambió.
- `project_file_log { id, path, limit? }`: los commits de un archivo
  (`git log --follow`); vacío si no hay git o nada está commiteado.
  `project_file_at { id, commit, path }`: el archivo en ese commit.
