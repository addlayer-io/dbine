# Documentar la base

Genera el **diccionario de datos** de una base en un solo archivo: tablas,
columnas, claves, índices, vistas, rutinas, triggers y, en HTML, un diagrama
entidad-relación. Sirve para entregar, versionar o imprimir la documentación
sin armarla a mano.

## Dónde está

Clic derecho sobre una **base** en el explorador › **Documentar la base…**.
También existe como paso de una [tarea programada](tareas-programadas.md#documentar-la-base),
para regenerar la documentación todas las noches.

## El diálogo

- **Formato:**
  - **HTML:** un solo archivo con índice, búsqueda, tema claro u oscuro y el
    diagrama. Se puede imprimir como PDF desde el navegador.
  - **Markdown:** texto para un repositorio o una wiki. No lleva diagrama.
- **Esquemas:** los que querés documentar; vacío, **Todos los esquemas**. Solo
  aparece en los motores con esquemas.
- **Incluir:** tablas; vistas; procedimientos y funciones; triggers;
  secuencias, tipos y otros objetos; el código fuente de vistas, rutinas y
  triggers; índices; claves foráneas; dependencias («Usada por»); y el
  diagrama.
- **Archivo:** dónde guardar. Si el nombre no tiene la extensión del formato
  (`.html` o `.md`), se la agrega.

Las partes que la base no tiene, o que el motor no ofrece, aparecen
deshabilitadas con **Este motor no lo tiene**.

**Generar** cierra el diálogo y corre como tarea en segundo plano, con el
avance por fase (lista de objetos, tablas, columnas y claves, código,
escritura) y con opción de cancelar. Al terminar muestra la cantidad de
tablas y de objetos, y una notificación con **Abrir** y **Mostrar en la
carpeta**, que solo funcionan con archivos generados por esta ejecución de
DBine. Las notas de lo que no se pudo leer van al registro de la tarea.

## Qué lleva el documento

- **Encabezado:** la base, la conexión, el motor y su versión, y cuándo se
  generó.
- **Por tabla:** columnas (tipo, nulo, valor por defecto, clave,
  autoincremental, comentario), clave primaria, claves foráneas con sus
  acciones al borrar y actualizar, índices, restricciones `CHECK` y los
  triggers.
- **Filas (aproximadas):** debajo de cada tabla, la cantidad de filas que el
  motor ya tiene anotada en sus **estadísticas**. Nunca se hace un `COUNT`:
  no recorre la tabla ni la bloquea. El número puede estar desactualizado
  hasta que el motor refresque sus estadísticas (`ANALYZE` y equivalentes,
  según el motor). Si el motor no guarda estadísticas de esa tabla, la línea
  no aparece. Qué fuente usa cada motor está en
  [`soporte-por-motor.md`](soporte-por-motor.md#filas-aproximadas-y-comentarios).
- **Comentarios de los objetos:** las vistas, procedimientos, funciones,
  triggers, secuencias y tipos muestran el comentario que tengan en el motor
  (los de tablas y columnas ya salían).
- **Usada por:** quién depende de la tabla. Une las claves foráneas que lee
  el catálogo con las vistas, rutinas y triggers cuyo código nombra la
  tabla. Cuando el nombre aparece dentro de un texto y no se puede asegurar,
  lo marca como **la nombra dentro de un texto: revisar**.
- **Vistas, rutinas, triggers y otros objetos**, agrupados por tipo en el
  orden del motor, con su código si se pidió.
- **Diagrama (HTML):** SVG en capas, con las tablas referenciadas a la
  izquierda. Cada caja muestra hasta 12 columnas y enlaza a su tabla. Pasadas
  las **150 tablas** se arma un diagrama por esquema; el esquema que supera
  ese máximo queda sin diagrama, con un aviso en el documento.
- **Notas:** lo que no se pudo leer (por ejemplo, el código de algún
  objeto) queda escrito al final, no se pierde en silencio.

Todos los nombres, comentarios y códigos se escapan: un comentario con
`<b>` aparece como texto, no como HTML.

El idioma del documento es el de la interfaz; si la tarea programada no manda
sus textos, sale en español.

## Cómo se lee la base

Con una **sesión de solo lectura propia**, aparte de las pestañas abiertas.
Solo la lista de objetos es obligatoria: lo que el motor no tiene o falla al
leer pasa a las notas.

## Como paso de una tarea programada

El paso **Documentar la base** de [`tareas-programadas.md`](tareas-programadas.md)
usa las mismas opciones del diálogo (formato, esquemas y partes). Escribe en
una **Carpeta** con el **Nombre del archivo** que elijas (admite
[variables](tareas-programadas.md#variables)) y produce `file` y `tables`.

## Particularidades por motor

El documento se arma con lo que el motor informa por el contrato, así que
cada motor sale con lo que tiene: sin claves foráneas no hay enlaces ni líneas
en el diagrama, y sin dependencias no hay **Usada por**. Qué le falta a cada
motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#documentar-la-base).

## Contrato

Usa los métodos de sesión `row_estimates` y `object_comments` (por defecto devuelven vacío) y los que ya existían:
`Session::list_objects`, `database_schema` (tablas, claves e índices),
`columns` (lo que `database_schema` no trae, como vistas),
`definition` (el código), `row_estimates` (filas aproximadas, solo de estadísticas del motor), `object_comments` (comentarios de objetos que no son tablas ni columnas), `server_version`, `list_schemas`, y las capacidades
`capabilities().foreign_keys` y `supports_dependencies()`.

Comandos: `dbdocs_outline`, `dbdocs_generate` (evento `dbdocs-progress`,
cancelable con `cancel_query` sobre `docs:<runId>`) y `dbdocs_open`. Ver
[`api-comandos.md`](api-comandos.md).
