# Comparación de datos

Compara las filas de dos tablas y arma el script que iguala una con la otra.
Las tablas pueden estar en la misma conexión, en conexiones distintas o en
motores distintos. Complementa a la [comparación de esquemas](comparacion-de-esquemas.md).

## Cómo se usa

- Clic derecho sobre una tabla › **Comparar datos…**: abre una pestaña con
  esa tabla a la izquierda. También está en el menú de la base.
- Se elige la tabla de la derecha (conexión, base y tabla). Si en la otra
  base hay una tabla con el mismo nombre, se propone sola.
- **Comparar** muestra:
  - las filas **iguales** (solo la cantidad);
  - las **distintas**, con cada valor distinto marcado (el de la izquierda
    arriba, el de la derecha abajo);
  - las que están **solo a la izquierda** y **solo a la derecha**.
- En cada fila de las tres vistas hay dos flechas, como en la comparación de
  esquemas: **→** deja la fila del lado derecho igual a la izquierda
  (actualiza la distinta, inserta la que falta o borra la que sobra) y **←**
  hace lo mismo hacia la izquierda. Otro clic sobre la flecha activa
  la deja sin elegir. Al pie de cada vista, **← Todas**, **Todas →** y
  **Ninguna** eligen todas las filas de esa vista, también las que no se
  muestran (la vista carga hasta 2.000).
- **Igualar la derecha →** / **← Igualar la izquierda** marcan de un paso las
  filas distintas y las que faltan de ese lado. Las que habría que borrar no
  se marcan solas: se eligen una por una o con **Todas** en su vista.
- Con lo elegido, el botón de sincronizar arma **un script por cada lado que
  cambia**, en pestañas. Se puede copiar, abrir en una consulta o ejecutar.
  **Ejecutar** corre primero el de la izquierda y después el de la derecha, y
  se detiene en el primer error (lo ya ejecutado no se deshace). Nada corre
  sin ese clic. Si sale bien, se vuelve a comparar.

## Cómo compara

- Las filas se emparejan por la **clave primaria** de la tabla de la
  izquierda. Si no tiene, o se quiere otra, se eligen las columnas de la
  clave después de la primera comparación.
- Se comparan las columnas que tienen las dos tablas (por nombre, sin
  distinguir mayúsculas). Las que tiene una sola se listan y no se comparan.
- Los valores se comparan por lo que valen, no por cómo los devuelve cada
  motor: `1`, `1.0` y `"1.00"` son iguales; una fecha con `T` o con espacio,
  también. Un texto con ceros adelante (`"007"`) se compara como texto.
- Se leen hasta 200.000 filas por lado. Si una tabla tiene más, el resultado
  lo avisa: la comparación es parcial.
- Se muestran hasta 2.000 filas de cada tipo; los totales y el script
  incluyen todas.
- Si una clave se repite en un lado, la fila se compara una sola vez y el
  resultado lo avisa.

## El script

Lo escribe el driver del destino, en su lenguaje (`insert_script`,
`update_script` y `delete_script` del contrato): SQL en los motores SQL,
`insertMany`/`updateOne`/`deleteOne` en MongoDB, etc. El orden es borrar,
actualizar e insertar.

- Las actualizaciones cambian solo las columnas distintas.
- Un borrado siempre lleva la clave: nunca se genera un `DELETE` sin `WHERE`.
- Las conexiones de solo lectura rechazan el script al ejecutarlo.

## Comandos

- `data_compare { left, right, key, columns, limit }` → los totales, las
  filas de muestra y un `id`.
- `data_compare_script { id, choices }` → un script por cada lado que
  cambia: `{ connection_id, database, side: "left" | "right", script,
  inserts, updates, deletes }`. `choices` tiene `changed`, `only_left` y
  `only_right`; cada uno lleva `all` (`"left"`, `"right"` o `"none"`) y
  `rows`, las excepciones `[índice, dirección]` sobre la lista completa de
  esa vista.
