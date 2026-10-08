# Constructor de consultas

Arma un `SELECT` sin escribirlo: arrastrás tablas a un lienzo, las unís,
marcás columnas y completás una grilla con alias, agregados, orden y filtros.
DBine escribe la consulta en el SQL (o el CQL) del motor.

## Dónde está

- Clic derecho sobre una **base** › **Diseñar consulta…**.
- Clic derecho sobre una **tabla o vista** › **Diseñar consulta…**: abre el
  constructor con esa tabla ya en el lienzo.
- El botón **Diseñar** de la barra del editor de consultas.

Se abre en su propia pestaña (**Diseñar consulta**). Solo está en los motores
con lenguaje SQL o CQL; los demás (documentos, clave-valor, grafos…) no lo
ofrecen. El diseño se guarda con la pestaña.

## El lienzo

- La lista de tablas de la izquierda tiene búsqueda. Una tabla se agrega
  **arrastrándola** al lienzo o con **doble clic**.
- El lienzo se desplaza y se acerca igual que el diagrama de la base. Cada
  tabla muestra sus columnas, con las claves primarias y foráneas marcadas.
  Doble clic sobre el nombre cambia el **alias**.
- **Uniones:**
  - Al agregar una tabla, se crean solas las uniones por **claves foráneas**
    con las tablas que ya están.
  - También se une **arrastrando una columna sobre una columna de otra
    tabla**.
  - Un clic sobre la unión permite cambiar el tipo (`INNER`, `LEFT`,
    `RIGHT`, `FULL`), invertir el orden de las tablas o quitarla.
  - Una tabla sin unión se combina con todas las filas de las otras
    (`CROSS JOIN`), y la consulta lo avisa.
- Los motores que consultan **una tabla a la vez** (ver
  [`soporte-por-motor.md`](soporte-por-motor.md#constructor-de-consultas))
  solo dejan una tabla en el lienzo.

## La grilla

Una fila por columna. Se agregan marcando la columna en la tabla, con
**Agregar columna…**, o con `*` (**todas las columnas**).

| Campo | Qué hace |
|---|---|
| **Columna** | La columna de una tabla del lienzo. |
| **Alias** | El nombre de la columna en el resultado. |
| **Mostrar** | Si está en la lista del `SELECT`; desmarcada, la columna solo filtra u ordena. |
| **Agregado** | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` o `COUNT DISTINCT`. Las columnas sin agregado pasan a `GROUP BY`. |
| **Orden** y **N.º** | Ascendente o descendente, y la prioridad cuando se ordena por varias. |
| **Filtro** | Una condición por celda: `=`, `<>`, `<`, `<=`, `>`, `>=`, `LIKE`, `NOT LIKE`, `IN`, `NOT IN`, `BETWEEN`, `IS NULL`, `IS NOT NULL`. |

- Los filtros se agrupan: las celdas del mismo grupo (la columna de filtros
  **Filtro**, o **O** para los grupos siguientes) se combinan con `AND`, y
  los grupos entre sí con `OR`. **Agregar un grupo O** suma un grupo.
- Una condición sobre una columna con agregado va a `HAVING`; las demás, a
  `WHERE`. Cada parte combina sus grupos O por separado, y el constructor lo
  avisa.
- El valor se escribe como texto o número según el tipo de la columna. Con
  el botón de expresión (**El valor es una expresión SQL**) se escribe tal
  cual, sin comillas. En `IN` y `NOT IN` los valores van separados por comas.
- Arriba están **Límite** (o **Sin límite**) y `DISTINCT`.

## La consulta

El SQL generado se ve en vivo, con los **avisos** de lo que el motor no hace
(por ejemplo, **este motor no ofrece DISTINCT en esta consulta**). Las
funciones que el motor no tiene no se ofrecen en la interfaz.

- **Ejecutar vista previa:** trae hasta **100 filas** en una **sesión de solo
  lectura** propia, y dice si hay más filas. Se puede cancelar.
- **Abrir en una consulta:** le pasa el SQL a una pestaña de consulta para que
  lo ejecutes, lo edites o lo guardes.

El constructor **nunca cambia la base** y solo escribe `SELECT`. El SQL no se
vuelve a leer para armar el diseño: si lo editás en la pestaña de consulta, el
diseño no se entera.

## Particularidades por motor

Cada motor pone sus nombres, comillas y límite de filas:

- Las tablas se nombran como la consulta de **Ver datos** del motor (un
  dataset de BigQuery, una ruta de IoTDB, un keyspace de Couchbase, un
  contenedor de Cosmos DB…), y el límite de filas es el del motor: `LIMIT`,
  `TOP`, `FETCH FIRST` o `FIRST`.
- Los identificadores se entrecomillan con `"…"`, `` `…` `` o `[…]` según el
  motor; los textos de SQL Server y Sybase llevan prefijo `N'…'`.
- En **CQL**, un filtro fuera de la clave primaria agrega `ALLOW FILTERING`
  con un aviso, porque recorre la tabla.

Qué uniones, agregados y operadores tiene cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#constructor-de-consultas).

## Contrato

No agrega métodos a los drivers. Los nombres y el límite de filas salen de
`Session::browse_query` (la consulta de **Ver datos**); lo que ese texto no
dice (uniones, agrupación, `HAVING`, operadores) está por dialecto en
`features()` de `src-tauri/src/commands/query_builder.rs`. Para armar las
uniones usa las claves foráneas que ya informa `database_schema`.

Comandos: `build_query` y `preview_built_query` (cancelable con
`cancel_query` sobre `qb-preview:<sessionId>`). Ver
[`api-comandos.md`](api-comandos.md).
