# Historial de consultas

Todo lo que se ejecuta desde el editor queda en el historial: la vista con el
reloj en la barra de actividad.

## Qué muestra

- Las ejecuciones, de la más nueva a la más vieja, **agrupadas por el
  servidor** donde corrieron: el host de la conexión o, en los motores de
  archivo, el nombre del archivo.
- Cada ejecución muestra la hora, la base, la cantidad de filas (devueltas o
  afectadas, cuando el motor lo informa), la duración y el texto. Las que
  fallaron llevan una marca roja con el error.
- La búsqueda filtra por el texto, el servidor, la base o el nombre de la
  conexión.

## Qué se puede hacer

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
