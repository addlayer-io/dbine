# Editor de consultas: navegación, parámetros y snippets

## Ir a la definición desde el código

- **⌘+clic** (Ctrl+clic en Windows y Linux) sobre el nombre de una tabla,
  vista, colección o rutina la abre. Mientras la tecla está apretada, los
  nombres que DBine reconoce se subrayan al pasar el mouse.
- Las tablas y colecciones abren su **estructura**; las vistas y las rutinas,
  su **definición**. Lo que no tiene columnas ni código (una clave de Redis,
  por ejemplo) abre sus datos.
- El menú del editor (clic derecho) suma **Ir a la definición** y **Mostrar
  en el explorador**, que despliega el árbol hasta el objeto y lo selecciona.

Los nombres se buscan entre los objetos de la base de la pestaña que conoce el
explorador (su caché primero, y lo que responde el servidor después). Se
reconocen nombres con esquema (`ventas.pedidos`), entre comillas o corchetes
(`"Pedidos"`, `[Pedidos]`, `` `pedidos` ``) y los alias del `FROM`/`JOIN`: un
clic en `p` o en `p.total` abre la tabla de `p`. Primero se busca la grafía
exacta y, si no está, sin distinguir mayúsculas. En MongoDB se reconoce la
colección de `db.pedidos.find(…)`; en las consolas HTTP, el índice de la ruta;
en Cypher, la etiqueta.

## Nombres desconocidos

Con [Calidad de código](calidad-de-codigo.md) activada, el editor marca como
**advertencia** (nunca como error, y nunca impide ejecutar):

- `unknown-table`: una tabla del `FROM`, `JOIN`, `UPDATE` o `INSERT INTO` que
  no existe en la base de la pestaña.
- `unknown-column`: una columna calificada (`alias.columna`,
  `tabla.columna`) o de la lista de columnas de un `INSERT` que la tabla no
  tiene. Solo cuando la tabla existe y sus columnas ya están cargadas.

No marca nada mientras los objetos de la base no se cargaron, ni lo que no se
puede saber desde el texto: tablas temporales (`#tmp`), variables de tabla
(`@t`), CTE, alias, lo que el mismo script crea (`CREATE TABLE`,
`SELECT … INTO`), funciones de tabla (`generate_series(…)`), catálogos del
motor (`pg_*`, `sys.*`, `information_schema`, `dual`…), nombres de otro
esquema que la base no tiene (otra base, un servidor vinculado) y el SQL
dinámico, que está dentro de textos.

Las dos reglas se apagan en **Configuración › Calidad de código**, como las
demás.

## Parámetros

Si el texto que se ejecuta tiene parámetros, antes de ejecutarlo se abre
**Parámetros de la consulta** con uno por fila: el tipo (**Texto**,
**Número**, **Fecha** o **NULL**) y el valor. A la derecha se ve el literal
que va a quedar en el texto, con las comillas del motor (`'O''Brien'`,
`N'…'` en SQL Server, `DATE '2024-05-01'`, `ISODate("…")` en MongoDB…).
**Cancelar** no ejecuta nada. Los valores se recuerdan por pestaña.

Qué se reconoce como parámetro:

- `:nombre`, fuera de textos y comentarios. Nunca un cast `::tipo`, un
  rango de arreglo `a[1:n]`, la asignación `:=`, ni lo que va pegado a otra
  palabra (`a:b`).
- `?` en los motores que lo usan como marcador y no como operador: MySQL,
  MariaDB, SQLite, Db2, Trino, Snowflake, Hive, Spark, Databricks, DynamoDB
  (PartiQL), Couchbase, Cassandra y los ODBC genéricos. No en PostgreSQL (`?`
  es un operador de jsonb) ni en ClickHouse (operador ternario). Cada `?` es
  un parámetro aparte; `?1` repetido es uno solo.
- `@nombre` solo en BigQuery, Spanner y Cosmos DB, donde es un parámetro. En
  SQL Server, Sybase y MySQL `@` es una variable y no se pide.

Un script que define código (`CREATE PROCEDURE`, `CREATE TRIGGER`,
`EXECUTE BLOCK`…) no pide parámetros: sus `:nombre` son variables del propio
código (por ejemplo `:new.columna` en un trigger de Oracle).

Pasa por el mismo diálogo todo lo que ejecuta: **Ejecutar**, la sentencia del
cursor, los planes y **Ejecutar en varias bases**. El diálogo tiene **No pedir
parámetros en esta pestaña**, que ejecuta el texto tal como está; se vuelve a
activar desde el menú del editor con **Pedir los parámetros al ejecutar**.

## Snippets

Escribí una abreviatura y apretá **Tab**: se expande y el cursor queda en el
primer campo; **Tab** y **Mayús+Tab** pasan de un campo a otro. Las
abreviaturas también aparecen en el autocompletado.

Cada motor tiene su juego incorporado en su sintaxis:

| Abreviatura | Inserta |
|---|---|
| `sel`, `selw` | `SELECT *` con el límite del motor (`TOP`, `LIMIT` o `FETCH FIRST`), con o sin `WHERE` |
| `selc`, `seld`, `grp` | `COUNT(*)`, `DISTINCT`, conteo agrupado |
| `ins`, `upd`, `del` | `INSERT`, `UPDATE … WHERE`, `DELETE … WHERE` |
| `cte`, `ij`, `lj`, `exi` | `WITH`, `INNER JOIN`, `LEFT JOIN`, `WHERE EXISTS` |
| `crt` | `CREATE TABLE` con la columna identidad del motor |
| `tran`, `try`, `proc`, `func`, `ups`, `blk` | Según el motor: transacción, `TRY/CATCH`, procedimiento, función plpgsql, upsert, bloque PL/SQL |

MongoDB (`find`, `agg`, `ins`, `upd`, `del`, `idx`…), Redis, etcd, Cypher,
Flux, Elasticsearch/OpenSearch/Solr, CouchDB y Cassandra tienen los suyos.

En **Configuración › Snippets** se agregan los propios: abreviatura, motores
(uno, todos los SQL o todos) y el texto, con campos `${1:texto}` (el número
da el orden, el texto es el valor inicial) y `${}` donde termina el cursor.
Uno propio con la misma abreviatura reemplaza al incorporado. Se guardan en la
configuración (`editor.snippets`), que viaja con la sincronización.
