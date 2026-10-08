# Renombrar con impacto

Clic derecho sobre una tabla, una vista, una rutina, un trigger, una columna,
un índice o un esquema › **Renombrar…**. DBine arma un solo script que cambia
el nombre y actualiza el código de la base que lo usa: las vistas, rutinas y
triggers que lo nombran se vuelven a crear con el nombre nuevo. El script se
ve completo antes de correrlo.

La opción aparece solo donde el motor sabe renombrar ese tipo de objeto y la
conexión no es de solo lectura. Qué renombra cada motor está en
[Soporte por motor](soporte-por-motor.md#renombrar-con-impacto).

## El diálogo

1. **Nuevo nombre.** Se escribe tal como va a quedar, con sus mayúsculas. Si
   el motor necesita comillas para conservarlo (mayúsculas en PostgreSQL,
   minúsculas en Oracle, espacios o palabras reservadas en cualquiera), el
   diálogo avisa cómo se va a escribir. Si ya hay un objeto con ese nombre,
   también avisa.
2. **Lo que depende del objeto.** Es la misma búsqueda que **Ver
   dependencias…**, separada en tres grupos:
   - **Se reescriben**: vistas, rutinas y triggers cuyo código nombra el
     objeto. Cada uno muestra las líneas que cambian (antes y después) y se
     puede destildar. Los que tienen partes que DBine no pudo decidir
     empiezan destildados y marcados **revisar a mano**, con la línea y el
     motivo; si se tildan, esas líneas quedan como estaban.
   - **Lo actualiza el motor**: claves foráneas, índices, restricciones y los
     objetos que el motor sigue por sí solo (en PostgreSQL, las vistas y los
     triggers). No hace falta hacer nada.
   - **Atención manual**: el SQL dinámico, las definiciones que no se
     pudieron leer y el código que nombra el objeto de una forma que DBine no
     reconoce. **Abrir definición** los muestra para revisarlos después.
3. **Script.** Se actualiza con cada casilla. Se puede copiar o abrir en una
   consulta para correrlo a mano.
4. **Renombrar.** Corre el script como una tarea (se puede seguir en segundo
   plano y cancelar). Donde el motor permite cambios de estructura dentro de
   una transacción (PostgreSQL, SQL Server, SQLite), corre todo junto: si una
   sentencia falla, no cambia nada. En los demás, lo hecho antes del error
   queda hecho, y el diálogo lo avisa antes de correr.

En una conexión de producción (etiqueta `prod` o parecida, o un entorno de
proyecto marcado para confirmar cada ejecución) hay que volver a escribir el
nombre nuevo para habilitar **Renombrar**.

Al terminar, el explorador se actualiza y las pestañas abiertas del objeto
(datos, estructura, definición, índices, dependencias) pasan a llamarse como
el objeto nuevo. Las pestañas de consulta no se tocan: si alguna nombra el
nombre viejo, DBine avisa cuántas son.

## Qué se reescribe y qué no

DBine solo cambia lo que está seguro de que es el objeto:

- Los comentarios y los textos entre comillas (SQL dinámico) nunca se tocan.
  Un texto que nombra el objeto deja la rutina para revisar.
- Un nombre calificado con otro esquema (`ventas.Clientes` cuando se
  renombra `dbo.Clientes`) no es el objeto. Un nombre sin esquema dentro de un
  objeto de otro esquema depende de la ruta de búsqueda: queda para revisar.
- Las palabras que solo contienen el nombre (`ClientesViejos`) no cuentan.
- Un nombre de tabla seguido de `(` puede ser una función con el mismo nombre:
  queda para revisar (salvo en `INSERT INTO Clientes (…)` y parecidos).
- Una columna se cambia cuando la califica su tabla o un alias de esa tabla
  (`c.Pepe` con `FROM Clientes c`), o cuando la sentencia lee solo esa tabla.
  Si la sentencia lee varias y la columna no tiene tabla, queda para revisar.
- En una vista, una columna renombrada que aparece sola en la lista del
  `SELECT` pasa a `nuevo AS viejo`, así la vista conserva sus nombres de
  columna y lo que la usa sigue andando. La casilla **Mantener los nombres de
  columna de las vistas** (activada) lo controla.
- Al renombrar un esquema solo cambian los calificadores (`ventas.tabla`);
  una columna que se llame igual que el esquema no.
- Las comillas originales se respetan: `[Clientes]` pasa a `[Nuevo]`.
- En PostgreSQL el cuerpo de las funciones y procedimientos `sql` y `plpgsql`
  se lee como código; el `EXECUTE '…'` dentro de ellos sigue siendo texto.
- En MongoDB se reescriben las colecciones de `viewOn`, `$lookup.from`,
  `$unionWith.coll`, `$out` y `$merge`. Renombrar un campo no reescribe las
  vistas.

Los objetos reescritos vuelven con la sentencia que conserva sus permisos
cuando el motor la tiene (`CREATE OR ALTER` en SQL Server, `CREATE OR REPLACE`
en Oracle, en las rutinas de PostgreSQL y en las vistas de MySQL). Donde no,
se borran antes de renombrar y se crean después, y el script avisa que se
pierden los permisos otorgados sobre ellos. En SQL Server, las vistas con
`SCHEMABINDING` siempre se borran antes, porque el motor no deja renombrar lo
que usan.

Lo que esté fuera de la base (otras bases, aplicaciones, reportes, scripts)
no se revisa: lo que use el nombre viejo deja de funcionar.

## Cómo funciona

- El contrato está en `crates/dbine-driver/src/rename.rs`: el driver dice qué
  renombra (`Driver::rename_spec`, un `RenameSpec`) y escribe solo la
  sentencia de renombrar (`Driver::rename_script`). La reescritura del código
  (`rewrite_references`, `rename_header`, `quote_new`) es común a todos los
  motores y vive al lado de la búsqueda de dependencias, así las dos
  clasifican un nombre igual.
- La app (`src-tauri/src/commands/rename.rs`) busca lo que depende del
  objeto, lee las definiciones, clasifica y arma el script alrededor del
  renombre con el mismo planificador de **Comparar esquemas**. Los comandos
  están en [api-comandos.md](api-comandos.md#renombrar-con-impacto).
- Los drivers que corren en su propio proceso responden `RenameScript` por el
  protocolo; uno publicado antes responde que no lo conoce, y su manifiesto no
  ofrece renombrar.
