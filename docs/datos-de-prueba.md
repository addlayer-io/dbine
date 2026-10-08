# Datos de prueba

Llena una tabla con filas inventadas pero verosímiles: nombres, emails,
fechas, importes, claves foráneas que apuntan a filas que existen. Sirve
para probar una aplicación o una consulta sin copiar datos reales.

## Dónde está

Clic derecho sobre una **tabla** › **Generar datos de prueba…**. El menú
aparece en las tablas (y objetos con columnas que se pueden leer) de
conexiones que **no** son de solo lectura.

## El diálogo

- **Filas:** cuántas filas generar (de 1 a 10.000.000).
- Una grilla con una fila por **columna**: su **Tipo**, el **Generador**,
  sus **Ajustes** y el **% nulos** (solo en columnas que aceptan `NULL`).
- **Muestra:** 8 filas de ejemplo con los generadores elegidos, para ver cómo
  quedará. **Actualizar muestra** la recalcula, y **Otra muestra** cambia la
  semilla.

**Generar** pide confirmación (**Se van a insertar N filas en «tabla» de la
conexión «…»**) y corre en segundo plano, como una tarea con avance y
opción de cancelar. Al terminar dice cuántas filas se insertaron.

## Los generadores

Cada columna tiene **Automático** (el valor por defecto) o uno elegido:

| Generador | Qué hace | Ajustes |
|---|---|---|
| **Automático** | Elige según el **nombre** (email, nombre, ciudad…) y, si no, el **tipo** de la columna. | |
| **Omitir (valor por defecto)** | Deja la columna fuera del `INSERT`. | |
| **Nulo** | `NULL` (falla si la columna no acepta nulos). | |
| **Valor fijo** | El mismo valor en todas las filas. | valor |
| **Secuencia** | Un número que crece. | inicio, paso |
| **Entero** | Un entero al azar. | mín., máx. |
| **Decimal** | Un decimal al azar. | mín., máx., decimales |
| **Verdadero / falso** | Un booleano. | |
| **Fecha**, **Fecha y hora** | Una fecha en un rango (por defecto, el último año). | desde, hasta (`AAAA-MM-DD`) |
| **UUID** | Un UUID. | |
| **Texto** | Palabras al azar. | mín., máx. (palabras) |
| **Nombre**, **Apellido**, **Nombre completo**, **Email**, **Teléfono**, **Ciudad**, **País**, **Empresa**, **Dirección** | Valores de listas internas, en español. | |
| **De una lista** | Uno de los valores que escribas, separados por coma (no puede estar vacía). | valores |
| **De la tabla referenciada** | Un valor que ya existe en la tabla padre de la clave foránea. | |

### Lo que hace "Automático"

- Columnas de **identidad o autoincrementales**: se omiten.
- Columnas con **clave foránea de una sola columna**: toman valores de la
  tabla referenciada (hasta 1.000 valores leídos de ella). Si la tabla padre
  no tiene filas, **De la tabla referenciada** falla con un mensaje. Las
  claves foráneas de varias columnas no se resuelven.
- Texto: por el nombre de la columna (`email`/`correo`, `nombre`,
  `apellido`, `telefono`/`celular`, `ciudad`, `pais`, `empresa`,
  `direccion`, `uuid`); si no, palabras al azar.
- Números: enteros chicos, o decimales con la escala del tipo (hasta 6);
  fechas de los últimos tres años; booleanos al azar; JSON como `{}`.
- Los textos **respetan el largo** de la columna (se recortan).
- **Clave primaria de una sola columna:** los valores no se repiten dentro de
  lo generado (si es entera, empieza en un número alto al azar para no chocar
  con los ids que suele tener la tabla). El generador solo verifica contra lo
  que generó él: no consulta las filas que la tabla ya tiene, y no detecta
  como únicas las demás restricciones `UNIQUE` ni las claves compuestas. Si la
  tabla tiene otras, elegí un generador que no se repita (por ejemplo
  **Secuencia** o **UUID**).

## Cómo se inserta

Las filas se generan en DBine y se insertan de a **500** con el script de
`INSERT` del motor, en una sesión propia, como una importación. Si un lote
falla, la generación se corta con el error y el rango de filas
(**filas 501–1000: …**); **los lotes anteriores quedan insertados**, porque
no hay una transacción alrededor de todo.

La corrida usa la misma semilla que la muestra del diálogo; **Otra muestra**
la cambia.

## Particularidades por motor

Los generadores viven en DBine y no en los drivers, así que funciona en
**todos los motores que permiten insertar** desde DBine. Lo único que depende
del motor es:

- el script de inserción (`insert_script`) y los tipos de las columnas;
- la lectura de claves foráneas, que solo existe en los motores con claves
  foráneas.

## Contrato

No agrega métodos a los drivers: usa `Session::columns`,
`database_schema` (claves foráneas), `browse_query` (los valores del padre),
`Driver::insert_script` y `Session::execute`.

Comandos: `datagen_preview` y `datagen_run` (evento `datagen-progress`,
cancelable con `cancel_query` sobre `datagen:<genId>`). Ver
[`api-comandos.md`](api-comandos.md).
