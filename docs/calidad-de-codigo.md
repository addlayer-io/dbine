# Calidad de código

Mientras escribís en el editor de consultas, DBine marca lo que suele traer
problemas: consultas lentas, resultados que no son los esperados o cambios que
afectan más filas de las que pensás. El análisis es **solo sobre el texto**:
no consulta la base ni toca la conexión.

## Dónde está

- **En el editor:** los problemas se marcan en el margen y se subrayan. Al
  pasar el mouse sobre una marca se ve el mensaje, por qué importa y cómo
  corregirlo, y el id de la regla.
- **Ver problemas:** el botón de la barra del editor (con el conteo, por
  ejemplo **3 problemas**, o **Sin problemas**) abre la lista **Problemas del
  script**, con la línea de cada uno. **Configurar reglas…** abre la
  configuración.
- **Configuración › Calidad de código:** el interruptor **Revisar el código
  mientras escribo** y un interruptor por regla. **Activar todas** vuelve a
  prender las que apagaste.

Si el análisis está apagado, o la conexión de la pestaña no se puede
analizar (por ejemplo, un driver que no está instalado), el editor no marca
nada y el botón **Ver problemas** no aparece.

## Qué se marca

Cada problema tiene una **severidad**:

- **Error:** casi siempre un error de la persona, como una escritura sin
  filtro.
- **Advertencia:** suele dar resultados lentos o inesperados.
- **Sugerencia:** una mejora de estilo o de mantenimiento.

Lo que está dentro de un texto entre comillas, un comentario o un nombre entre
comillas **nunca se marca**: cada motor tiene su propio analizador léxico que
los reconoce.

Los interruptores se guardan en la configuración de DBine, en esta máquina:
`lint.enabled` y la lista `lint.disabled` de reglas apagadas.

## Reglas

Cada motor recibe las reglas de su familia. Los grupos son los de la
configuración.

### Todos los motores SQL

| Regla | Severidad | Detecta |
|---|---|---|
| `select-star` | Sugerencia | `SELECT *`. También en CQL. |
| `dml-without-where` | Error | `UPDATE` o `DELETE` sin `WHERE`. También en CQL. |
| `not-in-subquery` | Advertencia | `NOT IN (subconsulta)`: un solo `NULL` en la subconsulta deja el resultado vacío. |
| `leading-wildcard` | Advertencia | Un patrón `LIKE` que empieza con comodín, que no puede usar un índice. También en CQL y en las consolas de búsqueda. |
| `function-on-column` | Advertencia | Una función aplicada a una columna en el `WHERE`, que impide usar su índice. |
| `equals-null` | Advertencia | `= NULL` o `<> NULL`, que nunca da verdadero. |
| `order-by-ordinal` | Sugerencia | `ORDER BY 2`: ordena por la posición de la columna. |
| `order-by-random` | Advertencia | Un orden aleatorio (`RANDOM()`, `NEWID()`…) que ordena toda la tabla. |
| `implicit-cross-join` | Advertencia | Una tabla del `FROM` separada por coma y sin condición que la relacione: producto cartesiano. |
| `insert-without-columns` | Advertencia | `INSERT` sin lista de columnas. |
| `distinct-group-by` | Sugerencia | `DISTINCT` junto con `GROUP BY`. |
| `union-distinct` | Sugerencia | `UNION` donde `UNION ALL` evitaría ordenar o armar un hash. |

### SQL Server, Azure SQL, Fabric y Sybase

| Regla | Severidad | Detecta |
|---|---|---|
| `nolock` | Advertencia | `NOLOCK` o `READ UNCOMMITTED`: lee datos sin confirmar. |
| `cursor` | Sugerencia | Cursores, que procesan de a una fila. |
| `set-rowcount` | Advertencia | `SET ROWCOUNT`, que queda activo en la sesión y está obsoleto para `INSERT`, `UPDATE` y `DELETE`. |
| `global-identity` | Advertencia | `@@IDENTITY`, que puede devolver la identidad que generó un trigger. |
| `sp-prefix` | Advertencia | Procedimientos con prefijo `sp_`, el de los procedimientos del sistema. |
| `set-nocount` | Sugerencia | Un procedimiento sin `SET NOCOUNT ON`. |

### Familia PostgreSQL

| Regla | Severidad | Detecta |
|---|---|---|
| `for-update-wait` | Sugerencia | `FOR UPDATE` que espera el bloqueo, sin `SKIP LOCKED` ni `NOWAIT`. También en MySQL y Oracle. |
| `serial-identity` | Sugerencia | `serial` en lugar de `GENERATED … AS IDENTITY`. |

### Familia MySQL

| Regla | Severidad | Detecta |
|---|---|---|
| `for-update-wait` | Sugerencia | Ver arriba. |
| `group-by-nonaggregated` | Advertencia | Una columna que no está en el `GROUP BY` ni dentro de una función de agregación. |

### Oracle

| Regla | Severidad | Detecta |
|---|---|---|
| `for-update-wait` | Sugerencia | Ver arriba (en Oracle también se sugiere `WAIT n`). |
| `rownum-order-by` | Advertencia | `ROWNUM` con `ORDER BY` en el mismo bloque: se corta antes de ordenar. |
| `outer-join-plus` | Sugerencia | La sintaxis `(+)` de outer join. |

### InfluxDB (InfluxQL y SQL)

| Regla | Severidad | Detecta |
|---|---|---|
| `delete-without-time` | Advertencia | `DELETE` sin condición sobre `time`. |
| `drop-series` | Advertencia | `DROP SERIES` o `DROP MEASUREMENT`, que borran toda la historia. |

### Cassandra, ScyllaDB y Amazon Keyspaces

Además de `select-star`, `dml-without-where` y `leading-wildcard`:

| Regla | Severidad | Detecta |
|---|---|---|
| `allow-filtering` | Advertencia | `ALLOW FILTERING`, que recorre las particiones. |
| `no-partition-key` | Advertencia | Un `SELECT` sin `WHERE`: lee todas las particiones. |
| `batch-partitions` | Sugerencia | Un `BATCH` que escribe en varias tablas o particiones. |

### MongoDB, FerretDB y DocumentDB

| Regla | Severidad | Detecta |
|---|---|---|
| `write-all` | Error | Una escritura sin filtro. También en las consolas de búsqueda y en etcd. |
| `where-operator` | Advertencia | `$where`, que ejecuta JavaScript por documento. |
| `unanchored-regex` | Advertencia | Una expresión regular que no empieza con `^`. También en CouchDB. |
| `read-all` | Sugerencia | Una lectura sin filtro. También en CouchDB y etcd. |

### CouchDB

`unanchored-regex` y `read-all`.

### Elasticsearch, OpenSearch y Solr

`leading-wildcard` y `write-all`.

### Redis, Valkey y Dragonfly

| Regla | Severidad | Detecta |
|---|---|---|
| `keys-command` | Advertencia | `KEYS`, que recorre todas las claves y bloquea el servidor. |
| `flush` | Error | `FLUSHALL` o `FLUSHDB`. |
| `big-read` | Sugerencia | Una lectura completa de una clave (por ejemplo `HGETALL`, `SMEMBERS`). |

### etcd

`write-all` y `read-all`.

### Neo4j, Memgraph y Neptune

| Regla | Severidad | Detecta |
|---|---|---|
| `match-without-label` | Advertencia | Un `MATCH` sin etiqueta ni propiedades: recorre todos los nodos. |
| `cartesian-product` | Advertencia | Patrones desconectados, separados por coma. |
| `detach-delete-all` | Error | `DETACH DELETE` sin `WHERE`. |

El resto de los motores (por ejemplo InfluxDB 2 con Flux) no tiene reglas.
Qué motor recibe qué grupo está en
[`soporte-por-motor.md`](soporte-por-motor.md#calidad-de-código).

## Contrato

La calidad de código **no agrega métodos a los drivers**. Se apoya en
`DriverInfo` (`language`, `dialect`, `id`) y en `Driver::script_dialect()`,
que ya existían, para elegir el analizador y las reglas.

- El análisis está en `src-tauri/src/lint/` (un analizador léxico por familia
  y las reglas). Los textos de cada regla viven en la interfaz
  (`lint:rules.<id>`): un hallazgo lleva la regla, su rango y los valores que
  muestra el mensaje.
- Comandos: `lint_script` (recibe `connectionId` y `sql`; devuelve los
  hallazgos con posiciones en índices UTF-16 y la línea) y `lint_rules` (la
  lista de reglas para la configuración). Ver
  [`api-comandos.md`](api-comandos.md).
