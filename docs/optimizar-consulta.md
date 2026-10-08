# Optimizar consulta

Busca versiones de una consulta que puedan correr más rápido, las **compara**
con la original en la base (tiempos, filas y resultado) y sugiere índices a
partir del plan de ejecución. **Nada de esto cambia la base**: los scripts y
las reescrituras solo llegan a un editor, y los ejecutás vos.

## Dónde está

El botón **Optimizar** de la barra del editor de consultas. Toma la
**selección** o, sin selección, la sentencia donde está el cursor (si no hay
ninguna, avisa **Poné el cursor en una consulta o seleccionala**). Se abre en
su propia pestaña, **Optimizar**, y **Volver a analizar** repite el análisis.

Si la consulta **modifica datos** (`UPDATE`, `DELETE`…), nunca se ejecuta:
**Comparar** compara solo sus planes estimados.

## Qué muestra

1. **Consulta original**, con su plan y su costo estimado cuando el motor
   da planes de ejecución.
2. **Alternativas**: las reescrituras de las reglas, las de la IA y las
   tuyas.
3. **Observaciones**: cosas a saber que no son una reescritura segura.
4. **Índices sugeridos** y **Avisos del plan**.

Lo que no se pudo leer (la estructura de las tablas, el plan) queda listado
en **No se pudo leer**, con el motivo.

## Las reglas

Una regla solo ofrece una reescritura cuando puede asegurar que es
**equivalente**. Si no lo puede probar con el texto de la consulta y la
estructura de las tablas, la deja como está o la informa como observación.
Se conserva el texto que escribiste: la reescritura cambia solo lo necesario.

| Regla | Qué hace | Cuándo se aplica |
|---|---|---|
| `in_to_exists` | `x IN (subconsulta)` pasa a `EXISTS` con la condición dentro. | Subconsulta simple, de una sola columna y sin agregados ni más subconsultas, y `x` sigue siendo la columna de afuera. |
| `not_in_to_not_exists` | `NOT IN (subconsulta)` pasa a `NOT EXISTS`. | Solo si las **dos columnas** son `NOT NULL` según la estructura. |
| `exists_to_in` | `EXISTS` correlacionado pasa a `IN`. | Subconsulta sobre una tabla, sin agregados, correlacionada por igualdades de columnas. |
| `scalar_to_join` | Subconsultas escalares en el `SELECT` (`COUNT`, `SUM`, `MIN`, `MAX`, `AVG`) pasan a un `LEFT JOIN` con una tabla agrupada. Un `COUNT` sin filas sigue dando 0. | Sin `GROUP BY`, `HAVING` ni agregados en el bloque, y sin tablas unidas con coma. |
| `or_to_union` | `a = 1 OR b = 2` pasa a `UNION ALL`, donde cada rama excluye las filas de las anteriores. | `OR` de 2 a 4 condiciones sobre **columnas distintas**, consulta de un solo bloque sin `DISTINCT`, `TOP`/`LIMIT`, `GROUP BY`, `ORDER BY`, agregados ni funciones que dan otro valor en cada llamada (`RAND`, `NEWID`, `NOW`…). |
| `redundant_distinct` | Quita un `DISTINCT` innecesario. | La lista incluye una clave de cada tabla (la primaria o un índice único sin filtro sobre columnas `NOT NULL`), o su `*`; todas las tablas conocidas; sin `GROUP BY` ni `TOP`. Necesita la estructura. |
| `function_to_range` | `YEAR(col) = 2024` (o `EXTRACT`, `DATE(col)`, `CAST(col AS DATE)`, `col::date`, `TRUNC(col)`) pasa a un rango `col >= … AND col < …`. | PostgreSQL, MySQL, SQL Server y Oracle; la columna es de fecha, un índice **empieza** por ella y el valor es una fecha literal. |
| `count_to_exists` | `(SELECT COUNT(*) …) > 0` (y las formas equivalentes con `= 0`, `>= 1`, `<> 0`…) pasa a `EXISTS` / `NOT EXISTS`. | Subconsulta simple, sin `DISTINCT`, y que nada más ligue a la comparación. |
| `mongo_where` | `$where` pasa a operadores de consulta. | Solo MongoDB, y solo si la expresión son comparaciones de campos con constantes unidas por `&&`. La candidata pide **comparar antes de usarla**, porque con campos que son arrays o mezclan tipos el resultado puede cambiar. |

### Observaciones

No reescriben, pero valen la pena:

- **`SELECT *`**: avisa cuántas columnas trae la tabla, con **Abrir con las
  columnas** para abrir una versión que las lista (para que dejes las que
  usás). Solo con una tabla en el `FROM` y estructura conocida.
- **`NOT IN` con una columna que acepta `NULL`**: si la subconsulta devuelve
  un `NULL`, no sale ninguna fila.
- **Comparar un texto con un número** (`columna_texto = 5`): el motor
  convierte la columna en cada fila y no usa su índice.
- **Comparar con un texto `N'…'`** una columna `char`, `varchar` o `text`
  (SQL Server): el motor convierte la columna.

Según el código, las reglas **no se aplican** a la consulta de Cosmos DB,
PartiQL (DynamoDB), ksqlDB, IoTDB, TDengine, InfluxQL, OrientDB ni N1QL: el
análisis responde **Las reglas de reescritura no se aplican al lenguaje de
este motor** y quedan la IA y las alternativas propias. En los motores que no
son SQL, solo está `mongo_where`.

## Índices sugeridos

Salen del **plan estimado** de la consulta, sin ejecutarla:

- **El motor lo sugiere** (SQL Server, con su aviso de índice faltante): se
  muestra el `CREATE INDEX` con el **impacto estimado** en %.
- **Recorrido completo** de una tabla que la consulta filtra o une por
  columnas con las que ningún índice empieza: se sugiere un índice en esas
  columnas. En MongoDB, un `COLLSCAN` genera el índice de los campos del
  filtro.
- **Recorrido completo sin columnas conocidas**: en los motores cuyo lenguaje
  no se analiza (N1QL, Cosmos DB, PartiQL), solo se avisa que la tabla se
  recorre completa.

El script está en el lenguaje del motor y **solo se abre en una consulta**
(**Abrir en una consulta**): lo revisás y lo ejecutás vos. Un índice acelera
las lecturas y hace más lentas las escrituras. Los motores sin planes de
ejecución dicen **Este motor no muestra planes de ejecución, así que no hay
sugerencias de índices**. **Avisos del plan** reúne lo que el plan informa
(desbordes a disco, conversiones, recorridos…).

## Alternativas de la IA

**Pedir alternativas a la IA** usa el asistente configurado (ver
[`asistente-ia.md`](asistente-ia.md)). Si no hay ninguno, ofrece abrirlo.

- Se envían **solo**: la consulta, la **estructura** de las tablas que
  nombra (columnas, claves e índices) y un resumen del plan. **Nunca se
  envían filas.** La pantalla dice qué se mandó y a qué proveedor.
- Propone hasta **3** reescrituras equivalentes, cada una con su título y su
  explicación. No propone índices ni cambios de estructura.
- Cada una queda marcada **IA** y con el aviso de que **no está probado que
  sea equivalente**: se compara antes de usarla. La IA nunca ejecuta nada.
- Si no ve mejoras, lo dice (**La IA no encontró mejoras para esta
  consulta**).

Con un modelo local se envía menos estructura (hasta 12.000 caracteres) que
con uno en la nube (hasta 60.000).

También podés **Agregar una alternativa** propia: una versión equivalente que
escribís vos, que se compara igual que las demás (**Tuya**).

## Comparar

**Comparar** ejecuta la original y cada alternativa en una **sesión de solo
lectura** propia y compara:

- **Ejecuciones:** cada versión corre **una vez para calentar** y después N
  veces (1 a 20; 3 por defecto). Se muestra el tiempo mínimo y el promedio.
- **Filas máx.** (100.000 por defecto): cuántas filas se leen y se comparan
  de cada versión.
- **Resultado:** a medida que llegan, las filas se **resumen en una huella**
  (nunca se guardan). Las huellas de la original y de cada versión se
  comparan:
  - **Mismo resultado**: mismas filas y mismo contenido. Si la original no
    ordena, el orden de las filas no cuenta; si tiene `ORDER BY` (o un
    `sort` en MongoDB) en el nivel de arriba, sí.
  - **NO equivalente**: el resultado difiere. La versión queda marcada.
  - **Más de N filas: no se comparó**: devolvió más del máximo, así que su
    resultado no se verifica.
  - **Sin verificar**, **Error** o **Solo plan** (lo que modifica datos).
- **Costo:** el costo estimado del plan, si el motor lo da.
- **vs. original:** cuánto más rápida o lenta es, y cuánto cambia el costo.

Se marca **Recomendada** la versión más rápida **solo si** su resultado es
equivalente y su promedio es al menos 10 % menor que el de la original. Una
versión que no se probó equivalente nunca se recomienda.

Se puede cancelar mientras compara. La comparación tarda lo que tarda la
consulta multiplicada por las ejecuciones: elegí pocas con consultas pesadas.

## Usar esta versión

El botón **Usar esta versión** de cada alternativa tiene dos opciones:

- **Abrir en una consulta nueva** (la acción principal).
- **Reemplazar la consulta en su editor**: cambia el texto de la selección o
  la sentencia original en el editor de donde salió, **sin ejecutarla**. Si
  ese editor se cerró o el texto cambió, no pisa nada: abre la versión en una
  consulta nueva y lo avisa.

Antes de usarla, DBine pregunta si la versión **NO devolvió el mismo
resultado** que la original, o si **todavía no se verificó**. Solo se evita
la pregunta para una regla probada equivalente, o una versión que ya dio
**Mismo resultado** (o **Solo plan**).

## Particularidades por motor

- Los planes y los índices sugeridos dependen de que el motor dé planes de
  ejecución ([`soporte-por-motor.md`](soporte-por-motor.md#planes-de-ejecución)).
- Las reglas cambian con el dialecto (ver arriba), y `function_to_range` solo
  está en cuatro.
- Lo que cada motor no hace está en
  [`soporte-por-motor.md`](soporte-por-motor.md#optimizar-consulta).

## Contrato

No agrega métodos a los drivers: usa `Session::explain` (el plan estimado) y
`supports_explain()`, `database_schema` (la estructura para las reglas y la
IA), `table_ddl` con los índices (el `CREATE INDEX` del motor) y
`Session::execute` con el receptor de filas de la exportación (para la huella
del resultado).

Comandos: `optimizer_analyze`, `optimizer_ai`, `optimizer_compare` (evento
`optimizer-progress`) y `optimizer_cancel`; `cancel_query` sobre
`optimize:<runId>` también los detiene. Ver
[`api-comandos.md`](api-comandos.md).
