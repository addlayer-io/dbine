# Transferencia masiva de datos (diseño)

> Estado: **implementado**. El motor de transferencia (`crates/dbine-transfer`),
> la carga masiva de cada driver y la copia directa, el clonado fiel de SQL
> Server y PostgreSQL y la sincronización por filas están hechos. La pestaña
> Migrar usa el motor para copiar los datos ([migracion.md](migracion.md)); el
> clonado y la sincronización existen en el motor y en los drivers, y todavía
> no tienen control propio en esa pestaña. Qué vía tiene cada motor, y sus
> límites: [soporte-por-motor.md](soporte-por-motor.md).

La migración funciona entre los 44 motores. Antes copiaba los datos
generando `INSERT` en texto, de a 500 filas y una tabla por vez. Ahora la
copia la hace un motor de transferencia pensado para mover volúmenes
grandes: carga masiva nativa de cada motor, tablas en paralelo, memoria
acotada, reanudación y, entre bases del mismo motor, copia directa sin
decodificar las filas.

**Objetivo de rendimiento:** SQL Server → SQL Server local, 3 millones de
filas mixtas, **700.000 filas/s o más** con la app compilada en release.
**No se pudo medir:** el único servidor disponible es una imagen `amd64`
emulada en una Mac `arm64`, y ahí el límite es el servidor (copia directa de
200.000 a 270.000 filas/s; leer y cargar decodificando, 140.000 a 240.000). La
vara sigue abierta para un servidor `x86` nativo.

---

## 1. Qué había antes y qué cambió

| Antes | Problema |
|---|---|
| Cada fila viaja del driver a la app en un mensaje propio (un `write` por fila) | Costo fijo por fila, la vía de lectura no escala |
| Las celdas son `serde_json::Value` | Pierde información: binarios cortados a 1 KiB, `money` como float, decimales y fechas como texto |
| Se escribe con `insert_script` (SQL en texto, 500 filas) | El destino parsea texto; ningún motor usa su vía de carga rápida |
| Una tabla por vez | No aprovecha el servidor ni la red |
| Canal lector→escritor sin límite | Si el destino es lento, la memoria crece sin tope |
| Sin estado por tabla | Un corte obliga a empezar de cero |

Errores de la copia anterior, corregidos en la fase 0:

1. **Bloqueo** en la app compilada cuando origen y destino usan el mismo
   driver (SQL Server → SQL Server): la fila llega en el hilo lector del
   driver y, en ese mismo hilo, se le pide al driver el `INSERT`, cuya
   respuesta solo ese hilo puede entregar. Lo mismo en "Generar script" con
   datos.
2. **Binarios de más de 1 KiB** llegan cortados y el `INSERT` falla.
3. Entre bases del mismo motor se intentan escribir **columnas calculadas y
   `rowversion`**, y la tabla falla entera.
4. **`money`** pierde precisión en valores grandes.

---

## 2. Arquitectura

```
                 app (src-tauri)
   ┌──────────────────────────────────────────────┐
   │ crates/dbine-transfer: orquestador genérico  │
   │  plan · cola · N tablas en paralelo · estado │
   │  reanudación · reintentos · cancelación      │
   └───────────────┬──────────────────────────────┘
          lotes tipados (filas + bytes acotados)
   ┌───────────────┴───────────────┐
   │ driver origen                 │ driver destino
   │  read_batches()  ───────────► │  bulk_writer()
   └───────────────────────────────┘
        mismo driver en los dos extremos:
        copy_native(origen, destino) dentro del proceso del driver,
        sin que las filas pasen por la app
```

### 2.1 Modelo de datos para transferir

Un lote tipado (`RowBatch`) en `crates/dbine-driver`, independiente del
`serde_json::Value` de la grilla:

- Celdas sin pérdida: nulo, booleano, enteros de 64 bits (con y sin signo),
  flotantes, decimal exacto (entero de 128 bits + escala), texto, binario
  completo, fecha, hora, fecha-hora con y sin zona, UUID y JSON.
- Un lote se cierra en **1.000 filas o 2 MiB**, lo que llegue primero.
- Entre el driver y la app los lotes viajan **como un solo mensaje por
  lote**, no uno por fila.
- Entre el lector y el escritor de una tabla hay una **ventana de 16 lotes**
  (`CHANNEL_BATCHES`): si el destino es lento, el lector espera y la memoria
  no crece.

### 2.2 Contrato de los drivers (`crates/dbine-driver`)

Entra por el contrato, con implementación por defecto para que funcione en
**todos** los motores:

- `Session::read_batches(tabla, columnas, filtro)`: lectura en lotes
  tipados. Por defecto adapta el `execute` de siempre.
- `Session::bulk_load(tabla, columnas, opciones)`: carga masiva, con
  confirmación cada N filas o N bytes (`commit_rows`, `commit_bytes`),
  bloqueo de tabla (`table_lock`) y conservación de identidad
  (`keep_identity`) cuando el motor los tiene. Sin ella, la migración usa el
  `insert_script`, así que ningún motor se queda sin migración.
- `Driver::supports_bulk_load()` y `Driver::supports_native_copy(motor)`:
  capacidades para que la UI muestre qué vía se usa.
- `Driver::copy_native(origen, destino, tabla)`: copia dentro del proceso
  del driver cuando los dos extremos son el mismo driver (o una familia
  compatible). El origen solo se lee.
- `Driver::supports_clone()` y `Driver::clone_script(origen, destino, tablas)`:
  el clonado fiel (2.7).
- `Driver::supports_delta()`, `Driver::delta_filter(...)`,
  `Session::key_range`, `Session::delta_summary` y `Session::delta_apply`:
  la sincronización por filas (2.8).

Cada método nuevo lleva su variante en el protocolo de plugins, su reenvío
en `ReadOnlySession` y su campo de capacidad con `#[serde(default)]`.

### 2.3 Vía rápida de cada motor

Cada motor usa su vía propia de carga: `INSERT BULK` en SQL Server, `COPY`
binario en PostgreSQL, `LOAD DATA LOCAL` en MySQL y MariaDB, el Appender de
DuckDB, `RowBinary` en ClickHouse, DML por arreglos en Oracle, `insertMany`,
`_bulk_docs` y `_bulk` en los de documentos y búsqueda, trabajos de carga en
BigQuery, y así los demás. Los que no tienen vía propia escriben `INSERT`
multifila o por lotes con los ajustes de su driver.

La tabla completa (mecanismo, copia directa, clonado, sincronización) y lo que
cada motor no puede hacer están en
[soporte-por-motor.md](soporte-por-motor.md#transferencia-masiva-migrar-datos).
Las cargas en la nube no usan archivos en una etapa (`PUT` + `COPY`) donde el
cliente de su API no lo permite (Snowflake, Databricks): ahí la carga es
`INSERT`, con la limitación anotada.

**Cancelar y cortar:** ninguna carga confirma nada después de devolver el
control. Si falla o se cancela, los pedidos que ya estaban en camino se
esperan antes de volver; donde el motor no tiene transacciones, lo ya
confirmado queda y el error lo dice.

### 2.4 SQL Server: el cliente TDS propio

El driver de SQL Server pasa a usar una copia de tiberius 0.13 dentro del
repo (`vendor/tiberius/`), con estos cambios. Cada uno queda marcado en el
código con `PATCH(dbine)` y documentado en `vendor/tiberius/PATCHES.md`:

| # | Cambio | Por qué |
|---|---|---|
| 1 | Paso de filas como bytes: lectura de filas crudas y envío directo a una carga masiva, con verificación de que los tipos de cable coinciden | ~58 % menos CPU y ~30 % más rápido que decodificar y volver a codificar |
| 2 | `INSERT BULK` con hints (`TABLOCK`), lista de columnas exacta y `SELECT` de metadatos propio | Bloqueo de tabla como bcp; evita el error de columnas con `IDENTITY_INSERT`; permite declarar `xml`, `text`, `image`, espaciales y `hierarchyid` como tipos codificables |
| 3 | Tamaño de paquete configurable (32.767) | ~8 veces menos paquetes y registros TLS que con 4.096 |
| 4 | `money` y `smallmoney` exactos en la carga masiva | Hoy no se pueden cargar sin perder precisión |
| 5 | Byte de longitud de `date` en los metadatos | Desplazaba las columnas siguientes |
| 6 | Longitud del paquete en su propio encabezado | Corrupción latente al acumular paquetes |
| 7 | Escala de `time(n)` y `datetimeoffset(n)` | Escalas distintas de 7 fallaban |
| 8 | Sin control de longitud para columnas `(max)` | Valores de más de 65.535 bytes fallaban |
| 9 | Nombres de columna entre corchetes | Nombres con espacios o palabras reservadas rompían la carga |
| 10 | `nvarchar(n)` y `nchar(n)` en caracteres | Se declaraban con el doble de largo |

Cada cambio se revisó antes de portarlo contra lo que la 0.13 ya resuelve
(trae `packet_size` y opciones de carga masiva propias); lo que ya estaba no se
duplicó.

Otros ajustes de la conexión para transferir: `TCP_NODELAY`, conexiones
dedicadas por tabla (al cancelar, el servidor deshace el lote en curso) y
lectura con un único `SELECT` sin `ORDER BY`.

### 2.5 El orquestador (`crates/dbine-transfer`)

Genérico: no sabe de SQL Server ni de ningún motor.

- **Tablas en paralelo:** 8 por defecto, de 1 a 32, ajustable **en vivo** sin
  cortar las que están corriendo. Las más grandes primero.
- **Memoria acotada por bytes, no solo por filas:** como máximo 16 lotes de
  2 MiB en vuelo por tabla (~32 MiB), también con filas anchas. Los drivers
  que arman pedidos por su cuenta (BigQuery, Snowflake, Cosmos DB…) también
  se ajustan a ese tope.
- **Confirmación cada 100.000 filas o 512 MiB.** Lotes de 10.000 filas
  resultaron ~12 % más lentos.
- **Índices:** la tabla se crea con columnas y clave primaria, y el resto de
  los índices se crean al terminar su copia, mientras otras tablas siguen
  copiando. En una tabla que ya existía se desactivan los índices no
  agrupados durante la carga y se reconstruyen después, **aunque la copia
  falle**.
- **Claves foráneas al final**, y las restricciones que en el origen eran de
  confianza quedan de confianza en el destino.
- **Estado por tabla en SQLite** (pendiente, copiando, copiada, con error,
  cancelada), con filas confirmadas.
- **Reanudación después de un corte**, incluso de un `kill -9`: cada tabla es
  todo o nada. Una tabla a medias se vacía y se copia de nuevo; una tabla
  **ya copiada nunca se vacía**, solo se terminan sus índices.
- **Reintentos** solo ante errores transitorios: 3, empezando en 1 s y
  duplicando hasta 30 s. Botón "Reintentar las que fallaron".
- **Cancelar** una tabla o toda la corrida.
- **Cola en vivo:** se pueden sumar tablas a una corrida en curso, o correr
  una ya mismo fuera del límite.
- **Diagnóstico:** por tabla se mide cuánto esperó el lector al destino y el
  escritor al origen, y se informa cuál es el cuello de botella y las
  filas/s.
- **Progreso cada 5 s por tabla** y registro de la corrida con escritura
  agrupada. Las notas de la sincronización por filas (`DeltaResult.notes`, por
  ejemplo una clave foránea que quedó sin confianza) van a ese registro.
- **Verificación antes de copiar:** columnas y tipos del destino contra el
  origen; si difieren no se copia esa tabla y se dice qué columna difiere.
- Un error inesperado (panic) en una tabla queda como error de esa tabla y no
  tumba la corrida.

### 2.6 Reglas que no se negocian

1. **El origen es solo lectura, siempre.** Nada de DML, DDL, tablas
   temporales, `DBCC` ni cargas en el origen. Lo garantiza `ReadOnlySession`
   del lado de la app; la copia directa dentro del driver tiene que respetar
   lo mismo.
2. Una tabla ya copiada no se vacía nunca, ni al reanudar, ni al cancelar, ni
   al reintentar.
3. Como máximo un proceso por tabla, verificado **antes** de vaciar o cambiar
   su estado.
4. Solo se vacía una tabla por pedido explícito ("vaciar y copiar") o al
   reanudar una copia a medias.
5. Nunca se carga en una estructura distinta: se verifican columnas y tipos
   antes de cada copia.
6. Nada destructivo por defecto: "crear y copiar" nunca escribe sobre filas
   existentes.
7. Un cambio en la copia, la reanudación o la sincronización no está
   verificado hasta correrlo contra un servidor real, y la reanudación, con
   un corte real del proceso.

### 2.7 Clonado fiel

Entre dos bases del mismo motor, un modo "clonar" que deja el destino igual
al origen. Lo escribe cada driver como un `CloneScript` (`before`, por tabla
`create` / `after_data`, y `after`), con sentencias que se pueden correr de
nuevo (`IF NOT EXISTS`, `CREATE OR REPLACE`), así que una corrida reanudada
vuelve a ejecutar el script entero. El origen solo se lee; al destino solo se
le pregunta qué soporta (edición, versión, extensiones, *filegroups*).

- **SQL Server y Azure SQL:** esquemas, *filegroups*, funciones y esquemas de
  partición, colecciones de esquemas XML, tipos de usuario, secuencias con su
  valor actual, sinónimos; por tabla, columnas, clave, almacenamiento,
  *memory-optimized* con todos sus índices, tablas temporales (versionado del
  sistema), *columnstore*, índices con `INCLUDE`, filtro y opciones,
  estadísticas, índices deshabilitados, identidad; y al final vistas, funciones,
  procedimientos y triggers en orden de dependencia, `CHECK`, claves foráneas,
  versionado del sistema y *extended properties*. Fabric y Babelfish no clonan
  (no tienen buena parte de esos objetos); para ellos la migración genérica
  (columnas y clave) es la fiel.
- **PostgreSQL y derivados que mantienen su catálogo** (Timescale, Kingbase,
  AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu): esquemas, extensiones que el
  destino ofrece, colaciones, tipos enum / compuestos / dominio / rango,
  secuencias, funciones y procedimientos; por tabla, columnas (identidad,
  generadas, colaciones), clave, particionado declarativo, `INHERITS`,
  `UNLOGGED`, método de acceso y parámetros de almacenamiento, índices,
  restricciones; y al final claves foráneas, `CHECK` y exclusión, vistas y
  vistas materializadas, triggers, seguridad a nivel de fila, `TOAST`,
  identidad de réplica, comentarios y el valor actual de cada secuencia.
  No nombra dueños, *tablespaces* ni permisos: los objetos del destino son de
  quien corre el script. YugabyteDB, openGauss, Greenplum y derivados,
  CockroachDB, Redshift y los demás no clonan (su catálogo o su DDL no es el de
  PostgreSQL).

Entre motores distintos se sigue usando la conversión de `dbine-schema`.

### 2.8 Sincronizar solo lo que cambió

Para igualar el destino justo antes de salir a producción, sin vaciarlo:

- **Grupos por clave (baldes):** por rangos si la primera columna de la clave
  es entera (unas `ROWS_PER_BUCKET` filas por balde entre el mínimo y el
  máximo del origen; las filas que solo tiene el destino caen en los baldes de
  los bordes), o por un hash módulo un número primo (17 a 65.537, para que el
  hash no ignore las primeras columnas).
- **Resumen:** cada lado cuenta las filas y suma los hashes de las filas por
  balde, en paralelo. El origen solo se lee.
- **Comparar:** los baldes distintos son los que cambiaron.
- **Aplicar:** con pocos baldes distintos (y no más de la mitad), solo se leen
  del origen sus filas; si no, se lee la tabla entera. En el destino, las filas
  van a una tabla de trabajo y **una sola transacción** borra las que sobran,
  actualiza las que difieren e inserta las que faltan. Una lista de baldes
  **vacía significa todos**: se aplica sobre la tabla entera, sin filtrar por
  balde. Una sincronización que falla deja el destino como estaba y se corre
  de nuevo; la tabla nunca se vacía.
- **Tres profundidades:** todo el contenido, columnas grandes solo por su
  longitud, o solo claves. La profundidad decide qué baldes parecen cambiados;
  el aplicado siempre compara byte a byte.
- **Notas:** lo que el usuario tiene que saber de esa tabla (`DeltaResult.notes`,
  en español) va al registro de la corrida.

Por motor:

- **SQL Server y Azure SQL:** hash de fila con `HASHBYTES('MD5')` y baldes con
  `CHECKSUM`. La clave tiene que ser `NOT NULL` en los dos lados. Aplica con
  una tabla de trabajo clonada del destino, triggers apagados y un `MERGE`
  sobre las filas de los baldes que cambiaron. Si las *collations* de la
  clave difieren, se aplican todos los baldes.
- **Regla de confianza de las claves foráneas.** La sincronización no copia la
  confianza del origen (eso es esquema: clonar y comparar). Garantiza que una
  clave foránea que el destino *tenía en confianza* antes del aplicado la
  tenga después, o dice por qué no. Las claves entrantes con `ON DELETE` sobre
  exactamente las columnas de la clave siguen activas durante el `MERGE`, para
  que el borrado en cascada pase como en el origen; el resto se desactiva
  (`NOCHECK`) durante el `MERGE`, así las tablas se sincronizan en cualquier
  orden, y las que estaban en confianza se verifican de nuevo después del
  `COMMIT`. Una que falla (típico: la otra tabla todavía no se sincronizó)
  queda sin confianza, se marca con una propiedad extendida y una nota en el
  registro nombra la clave, las dos tablas y qué hacer; cada aplicado de
  cualquiera de las dos tablas vuelve a verificar las marcadas y las limpia
  cuando pasan. Una clave que ya estaba sin confianza y sin marca se deja como
  está.
- **PostgreSQL y derivados** (con YugabyteDB, desde PostgreSQL 11): hash de
  fila con `md5`, baldes con `hashtextextended`, columnas grandes por longitud
  sin leer las páginas fuera de línea, tabla de trabajo temporal cargada con
  `COPY` binario y, en una transacción, borrado, actualización e inserción (en
  PostgreSQL 17 y más, un solo `MERGE … RETURNING`). Triggers y claves
  foráneas de usuario se apagan si el rol puede; si no, disparan y el registro
  lo avisa.

Se coordina con "comparar datos": la sincronización masiva es la vía para
tablas grandes. Las medidas de referencia (20.000 filas nuevas sobre 3
millones en 0,4 s; ~12 veces más rápida la comparación de columnas grandes por
longitud) son de SQL Server.

---

## 3. Fases

| Fase | Qué | Estado |
|---|---|---|
| 0 | Lote tipado, lotes por mensaje entre driver y app, canal acotado, corrección de los 4 errores | Hecha |
| 1 | `dbine-transfer`: paralelo en vivo, confirmaciones por ventana, estado, reanudación, reintentos, cancelación, cola; UI de la corrida | Hecha |
| 2 | SQL Server: tiberius propio con sus cambios, `INSERT BULK`, paquetes grandes, copia directa con filas crudas | Hecha; la vara de 700.000 filas/s no se pudo medir (ver arriba) |
| 3 | Carga masiva nativa en el resto de los motores, y copia directa donde el motor la permite | Hecha; probada contra contenedores `dbine-test-*` donde los hay, el resto anotado en `soporte-por-motor.md` |
| 4 | Clonado fiel de SQL Server y PostgreSQL | Hecha en los drivers; falta el control en la pestaña Migrar |
| 5 | Sincronizar solo lo que cambió (SQL Server y PostgreSQL) | Hecha en el motor y los drivers; falta el control en la pestaña Migrar |

Cada driver que cambia sube su versión y se republica.

## 4. Notas de la implementación

- **Perfil de compilación:** el release usa `opt-level = 3` (el bucle de
  copia es CPU puro) y el `serde_json` de todo el proyecto lee los decimales
  con `float_roundtrip`, para no cambiar el último dígito de un `double`.
- **SQLite dentro de un mismo archivo:** copiar tablas por lotes entre dos
  conexiones al mismo archivo deja una lectura abierta mientras se escribe. En
  el modo de diario por defecto, esa lectura impide que la carga confirme y
  falla diciéndolo; en modo WAL (`PRAGMA journal_mode = WAL`) funciona. La copia
  directa de SQLite copia por rangos de `rowid`, en ventanas que no dejan el
  archivo bloqueado más de un tiempo acotado; con un filtro va también si
  origen y destino son el mismo archivo. Una vista o una tabla `WITHOUT ROWID`
  se copia con una sola sentencia y, si no cabe en ese tiempo, pasa a lotes.
  Como los rangos se leen en transacciones separadas, las filas que otro
  proceso escribe en el origen mientras tanto pueden copiarse o no (la
  lectura por lotes ve un solo estado).
