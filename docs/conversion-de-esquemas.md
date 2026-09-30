# Conversión de esquemas entre motores

`crates/dbine-schema` convierte tablas de un motor a otro: tipos, valores por
defecto, autoincrementales, claves, claves foráneas, índices y nombres. Es la
base de la migración de estructura entre motores. Desde la interfaz se usa con
**Migrar…** en el menú de una base ([migracion.md](migracion.md)).

Toma las tablas tal como las lee el driver de origen (`database_schema`) y
devuelve las mismas tablas en los términos del motor destino, más un reporte de
todo lo que cambió. El DDL lo escribe después el driver destino (`table_ddl`),
así que el crate no duplica la sintaxis de ningún motor.

```rust
let conv = dbine_schema::convert(&tablas, "postgres", "sqlserver", &Options::default())?;
// conv.tables:  Vec<TableSchema> en tipos de SQL Server
// conv.columns: columna de origen → columna y tipo de destino (para copiar datos)
// conv.issues:  el reporte
```

## Cómo convierte

1. `parse` separa cada tipo nativo en sus partes: nombre, argumentos,
   `unsigned`, zona horaria, arreglos y envoltorios como `Nullable(...)`.
2. El dialecto de origen lo clasifica en un tipo lógico neutral
   (`LogicalType`). El tipo lógico conserva lo que importa para no perder
   datos: tamaño y signo de los enteros, precisión y escala, fracciones de
   segundo, zona horaria, si el texto es unicode, valores de un enum.
3. El dialecto destino escribe ese tipo lógico con sus propios nombres. Si el
   valor no entra, usa un tipo más grande en vez de truncar; si igual se
   pierde algo, lo dice.
4. Los valores por defecto se traducen: "ahora", "fecha de hoy", "UUID nuevo",
   booleanos, secuencias. Un `nextval(...)` o `IDENTITY` pasa a ser
   autoincremental en el destino.
5. Claves foráneas, acciones `ON DELETE`/`ON UPDATE`, índices e índices
   filtrados pasan según lo que acepta el destino.
6. Los nombres se adaptan:
   - `CLIENTES` de Oracle queda `clientes` en PostgreSQL, porque ahí es la
     forma sin comillas. Un nombre con mayúsculas y minúsculas mezcladas se
     respeta.
   - Si un nombre supera el largo máximo del destino, se acorta sin repetirse.
7. El dialecto destino agrega lo que su motor exige. Por ejemplo: `ENGINE` y
   `ORDER BY` en ClickHouse, clave de partición en Cassandra, índice de tiempo
   en GreptimeDB, distribución en StarRocks.

Entre motores de la misma familia (por ejemplo PostgreSQL → CockroachDB) los
tipos, defaults y opciones se copian tal cual; solo se aplican las diferencias
de capacidades.

## El reporte

Nada se pierde en silencio. Cada cambio queda con su gravedad:

| Gravedad | Qué significa | Ejemplo |
|---|---|---|
| Info | Cambio fiel que conviene saber | `serial` → `IDENTITY`; UUID como `char(36)` |
| Aviso | Mismos valores, distinto comportamiento | un índice GIN queda con el tipo por defecto |
| Pérdida | Los valores pueden no entrar o perder detalle | `datetime2(7)` → `timestamp(6)`; la zona horaria en MySQL |
| Omitido | El destino no lo puede expresar | claves foráneas en ClickHouse; un índice único filtrado |

## Qué no hace (todavía)

- **No copia datos.** `conv.columns` dice qué columna de origen va a qué
  columna de destino, incluidas las que el destino agrega (índice de tiempo,
  `ROW_ID`) y las que renombra. Es lo que va a usar la copia de datos.
- **Vistas, procedimientos y triggers** no se convierten.
- **Enums de PostgreSQL creados con `CREATE TYPE`** llegan como tipo
  desconocido y el DDL del destino falla. Hay que convertirlos a mano.
- **Claves foráneas → relaciones** en motores de grafos: se omiten y se
  informa.
- **`STRUCT`, `ROW`, `Tuple`** se convierten a JSON, sin nota.

## Motores

Todos los drivers de DBine tienen dialecto, salvo los de la tabla "No aplica".
El test `tests/coverage.rs` falla si se agrega un driver sin dialecto ni
motivo.

**Solo como origen.** La conversión hacia estos motores devuelve un error que
explica por qué:

| Motor | Motivo |
|---|---|
| Apache Drill | Solo `CREATE TABLE … AS SELECT`, sin lista de columnas. |
| Apache Calcite Avatica | No tiene DDL propio: depende del motor que está detrás. |
| InfluxDB 1, 2 y 3 | Las measurements se crean al escribir datos. |
| NetSuite | Es de solo lectura. |
| Archivos CSV / Parquet / JSON | Cada archivo es una vista de solo lectura. |

**No aplica:**

| Motor | Motivo |
|---|---|
| Redis, Valkey, Dragonfly | Clave-valor: no hay tablas ni columnas. |
| etcd | Árbol de claves sin esquema. |
| Arrow Flight SQL | Protocolo genérico: el motor de atrás no se conoce. |

### Decisiones por familia

- **MySQL / MariaDB:**
  - `tinyint(1)` se toma como booleano, que es la convención de MySQL.
  - `TIMESTAMP` normaliza a UTC y solo cubre de 1970 a 2038, así que un
    timestamp con zona pasa a `DATETIME`, con aviso.
  - Si la fila supera 65.535 bytes, los `VARCHAR` más anchos pasan a
    `mediumtext`.
- **SQL Server:**
  - `timestamp` es `rowversion`.
  - `tinyint` va de 0 a 255.
  - Una columna `(max)` que forma parte de una clave se achica, con aviso.
- **Oracle:**
  - `DATE` guarda también la hora.
  - Se asume `AL32UTF8`: el texto va como `VARCHAR2(n CHAR)`.
  - `''` es `NULL`.
  - No hay `ON UPDATE`.
- **SQLite:** los tipos se eligen por afinidad. Solo una clave primaria
  `INTEGER` de una columna puede ser autoincremental.
- **ClickHouse:**
  - `ENGINE = MergeTree` y `ORDER BY` salen de la clave primaria.
  - Las fechas van a `Date32` y `DateTime64`, porque `DateTime` solo cubre de
    1970 a 2106.
  - No tiene claves foráneas.
- **MongoDB:**
  - Cada columna queda en un validador `$jsonSchema`, y `NOT NULL` pasa a
    `required`.
  - La clave primaria pasa a índice único. Mongo conserva su propio `_id`,
    porque renombrar la clave rompería las claves compuestas y las
    referencias.
- **Cassandra:**
  - La primera columna de la clave primaria es la clave de partición; las
    demás, de clustering.
  - No tiene `NOT NULL` ni defaults.
- **Elasticsearch:**
  - Los textos cortos pasan a `keyword` y los largos a `text`.
  - Los decimales de hasta 18 dígitos pasan a `scaled_float`.
- **Grafos:** cada tabla es una etiqueta y cada columna una propiedad. La clave
  y los índices únicos pasan a restricciones `UNIQUE`.

## Qué controla el conversor

- **Límites:** cada tipo se convierte a uno que admita todos sus valores. Por
  ejemplo, `INT UNSIGNED` de MySQL pasa a `BIGINT` de PostgreSQL, porque un
  `INTEGER` podría desbordar.
- **Tipos propios de cada motor:** JSON, enums y arreglos se traducen al tipo
  equivalente del destino, o se avisa cuando no lo tiene.
- **Funciones en defaults:** `NOW()`, `GETDATE()` o `NEWID()` se traducen a la
  función equivalente del destino, o se avisa cuando no la tiene.

## Pruebas

```sh
cargo test -p dbine-schema          # unitarios y de punta a punta, sin servidores
```

Los tests contra servidores reales están marcados `#[ignore]`. Leen las mismas
variables `DBINE_TEST_<MOTOR>_URL` que los tests de los drivers. Cómo correrlos
está en la cabecera de cada archivo: `live_core.rs`, `analytics.rs`,
`enterprise.rs`, `niche.rs`, `nosql.rs` y `odbc_engines.rs`.

Cada prueba en vivo hace lo mismo:

1. Crea en el origen una tabla con todos los tipos representativos.
2. La lee con el driver de origen y la convierte.
3. La crea en el destino con el DDL del driver destino y la relee.
4. Inserta la misma fila de valores límite de los dos lados (máximos,
   decimales, fechas con zona, unicode) y la compara.

Donde se puede, también hace el viaje de ida y vuelta (A → B → A).
