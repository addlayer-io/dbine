# Subconjunto de datos

Copia **algunas filas** de una tabla a otra base, con **todas las filas
padre que necesitan** (siguiendo las claves foráneas, de forma recursiva) y,
si querés, las filas que cuelgan de ellas. En el camino, los **datos
personales se enmascaran**. Sirve para armar una base de desarrollo o de
prueba chica, consistente y sin datos reales, a partir de una grande.

## Dónde está

Clic derecho sobre una **tabla** › **Copiar un subconjunto…**. Se abre en su
propia pestaña (**Subconjunto**).

El **origen nunca se modifica**: se lee con una sesión de solo lectura, y el
destino no puede ser la misma base (mismo servidor, puerto y base).

## Qué se copia

1. **Filas de la tabla de inicio:**
   - **Filtro:** **Sin filtro**; **Condición** (una condición en el lenguaje
     del motor, sin la palabra `WHERE`, por ejemplo
     `estado = 'activo' AND creado >= '2024-01-01'`), en los motores que
     tienen lenguaje de condiciones; o **Por columna**, con los filtros de
     la grilla de datos (es igual a, contiene, empieza con, es uno de, es
     nulo…).
   - **Cantidad:** **Todas las filas**, **Primeras N filas** o **N % de las
     filas**.
2. **Padres:** siempre. Cada fila elegida arrastra las filas de las tablas a
   las que apunta, y estas las suyas, hasta el final.
3. **Hijas (opcional):** **Incluir lo que cuelga de estas filas**, con
   **Niveles** (de 1 a 10; 1 son los hijos directos) y un **Tope por tabla**.
   Una tabla que llega al tope se marca **llegó al tope**.

Las tablas se escriben con los padres primero. Si hay un **ciclo de claves
foráneas**, se corta en una columna que acepte `NULL`: se copia con `NULL` y
se completa al final. Si no hay ninguna que lo acepte, el plan avisa que la
copia puede fallar si el destino valida las claves.

## El destino

Una **Conexión** y una base, del mismo motor o de otro.

- Las tablas que **ya existen** reciben las filas; las columnas del origen
  que no están en la tabla del destino no se copian (el plan dice cuáles).
- Las que **no existen se crean** con la estructura del origen, convertida al
  motor del destino cuando son distintos. Después de los datos se crean sus
  índices y claves foráneas. **Ver cómo se crea en el destino** muestra el
  DDL.
- Las columnas calculadas no se copian: las calcula el destino.
- Una tabla que no se puede crear ni copiar queda marcada **no se puede
  copiar**, con el motivo, y bloquea la copia.
- Un destino de **solo lectura** se rechaza.

## El plan

**Calcular el plan** lee origen y destino (sin escribir) y muestra, en el
orden de escritura:

- cada tabla, su rol (**Inicio**, **Padre**, **Hija**), cuántas filas, si
  **ya existe** o **se crea**, y el total de filas y de tablas;
- por columna, el **enmascaramiento**;
- los ciclos cortados y las notas (por ejemplo, que una tabla tiene más de
  un millón de filas).

La copia vuelve a leer los datos, porque pueden haber cambiado desde el plan.

## Enmascaramiento

Cada columna tiene una regla. Las que parecen guardar datos personales por su
**nombre y tipo** (en español e inglés) vienen sugeridas y marcadas como
**Dato personal**. **Volver a las reglas sugeridas** deshace los cambios.

| Regla | Qué hace |
|---|---|
| **Mantener** | Copia el valor. |
| **Inventado** | Un valor falso de un tipo: nombre completo, nombre, apellido, email, teléfono, dirección, ciudad o empresa. |
| **Inventado: documento** | Mantiene la forma del original (DNI, CUIT, IBAN, tarjeta…) y reemplaza cada dígito: `20-12345678-9` pasa a otro número con el mismo formato. |
| **Correr la fecha** | Mueve la fecha hasta N días para cualquiera de los lados; conserva la hora. |
| **Variar el número** | Mueve el número hasta un N %. |
| **Valor fijo** | El mismo valor para todas las filas. |
| **Nulo** | `NULL`. |
| **Hash** | Un hash del valor. Se sugiere para contraseñas y tokens. |

- Un `NULL` sigue siendo `NULL` con cualquier regla.
- El resultado depende solo de la semilla de la corrida, de la regla y del
  valor original: **el mismo valor se enmascara igual en todas las tablas**.
  Así, un email repetido en dos tablas queda igual y los joins por columnas
  enmascaradas siguen coincidiendo.
- Las claves nunca se sugieren. Si enmascarás una, las columnas que la
  referencian tienen que usar **la misma regla**; el plan lo avisa (**Clave
  enmascarada**).
- El enmascaramiento se hace en DBine, antes de que el dato llegue al
  destino.

### El hash lleva sal por corrida

La sal del hash (y la de todas las reglas) es una **semilla aleatoria nueva en
cada corrida**. Dos copias del mismo dato dan hashes **distintos**, así que no
se pueden comparar ni unir entre corridas. Si necesitás
valores estables entre copias, usá **Valor fijo** o no enmascares esa columna.

## Producción

Si el destino tiene la etiqueta `prod`, `production`, `producción` o `prd`,
**Copiar** pide escribir el nombre de la base de destino (si no tiene, el de
la conexión) para confirmar. El backend lo exige también: sin ese texto la
copia no corre.

La confirmación resume cuántas filas de cuántas tablas se copian, cuántas
tablas se crean y cuántas columnas se enmascaran.

## Límites

- **Con N % de las filas** se leen **las primeras 1.000.000 de filas** de la
  tabla de inicio (que cumplan el filtro) y de ahí se toma una muestra
  **pareja** del N %. No es una muestra de toda la tabla: si tiene más de un
  millón, el plan avisa que se tomaron las primeras. Con **Todas las filas**
  rige el mismo tope de 1.000.000 y el mismo aviso; **Primeras N filas** tiene
  como máximo ese valor.
- **Tope de 2.000.000 de filas en memoria** en total (inicio, padres e
  hijas). Las filas se juntan en DBine antes de escribirlas. Al pasarlo, la
  copia se rechaza: achicá el filtro, los niveles o el tope por tabla.
- **Una copia parcial no se deshace.** No hay una transacción alrededor de la
  copia: se escribe tabla por tabla, y la primera que falla corta las
  siguientes. Las tablas creadas y las filas ya escritas **quedan en el
  destino**. El resultado dice por tabla si quedó **Copiada**, con **Error**,
  **Cancelada** o **No se copió** (no se llegó), con las filas escritas.
  Cancelar tiene el mismo efecto.
- Las filas se escriben en lotes de 500, y las claves se buscan en el origen
  de a 500 por consulta.
- Los motores sin claves foráneas (documentos, clave-valor, series de
  tiempo) copian la tabla o colección elegida con su filtro y el
  enmascaramiento, sin padres ni hijas.

## Resultado y tareas

**Copiar** corre en segundo plano, con avance por tabla en el panel de
tareas, y se puede cancelar. Al terminar se ve el **Resultado**, por tabla:
filas copiadas, columnas enmascaradas, si la tabla fue **creada** y las notas.

Qué motores pueden ser origen o destino está en
[`soporte-por-motor.md`](soporte-por-motor.md#copiar-un-subconjunto).

## Contrato

No agrega métodos a los drivers: usa `database_schema` (estructura y claves
foráneas), `Driver::filtered_browse` (el filtro `IN` de las claves),
`table_ddl` y `insert_script` y `update_script` (los mismos que los datos de
prueba y la migración), y el conversor de `dbine_schema` cuando los motores
difieren.

Comandos: `subset_plan` y `subset_run`, con el evento `subset-progress`;
`cancel_query` sobre `subset:<runId>:src` y `subset:<runId>:tgt`. Ver
[`api-comandos.md`](api-comandos.md).
