# Comparar esquemas

Compara la estructura de dos bases lado a lado, al estilo WinMerge, y aplica
las diferencias hacia un lado o hacia el otro. Se abre desde el menú de una
base en el explorador: **Comparar esquemas…**.

## Cómo se usa

1. **Elegí las dos bases.**
   - La izquierda es la base desde la que se abrió; la derecha, por defecto,
     otra base de la misma conexión.
   - Cada lado puede ser cualquier conexión y base, incluso de otro motor.
   - Si los dos lados eligen un esquema, se comparan esos dos esquemas entre
     sí (por ejemplo `ventas` contra `ventas_qa` en la misma base). Las tablas
     se emparejan por nombre.
2. **Comparar.** Mientras compara se ve el botón **Detener**, que cancela la
   comparación. A la izquierda aparecen las tablas, vistas, procedimientos,
   funciones y triggers con su estado:

   | Ícono | Estado |
   |---|---|
   | `≠` | distinta |
   | `◧` | solo a la izquierda |
   | `◨` | solo a la derecha |
   | `=` | igual (se ven destildando "Solo diferencias") |

3. **Revisá una tabla.** Al elegirla se ve lado a lado: columnas, índices,
   claves foráneas y clave primaria. Lo que difiere en cada columna (tipo,
   nulos, valor por defecto, autoincremento, comentario) queda resaltado. Las
   restricciones `CHECK` van en su propia sección, "Restricciones CHECK", y las
   opciones de la tabla y de sus columnas, en la fila "Propiedades de la
   tabla". Las
   vistas y procedimientos muestran su código con las líneas distintas
   marcadas.
4. **Pasá los cambios con las flechas.**
   - `→` deja la derecha igual a la izquierda; `←`, al revés. Hay flechas por
     tabla y por columna, índice, clave foránea o clave primaria.
   - Si del lado de origen el objeto no existe, la flecha lo **borra** del
     otro lado. El tooltip de cada flecha dice qué va a hacer.
   - **Eliminar** borra un objeto de un lado sin pasarlo desde el otro; si
     existe en ambos lados, se puede eliminar de cada uno. Antes de ejecutar,
     "Sincronizar" muestra qué depende de lo que se borra. Qué motores y tipos
     lo permiten: [`soporte-por-motor.md`](soporte-por-motor.md#eliminar-en-la-comparación).
   - Pasar un cambio no toca la base: solo modifica la copia en memoria de ese
     lado. El objeto queda marcado con un punto y el pie cuenta los cambios
     sin aplicar de cada lado.
   - **Deshacer** vuelve atrás el último paso; **Descartar** olvida todos los
     cambios de un lado.
5. **Sincronizar.**
   - Genera el script del motor de ese lado (`CREATE`, `ALTER`, `DROP`) y
     lo muestra con los avisos: datos que se pierden, cambios que pueden fallar
     con filas existentes, cosas que el motor no permite.
   - **Abrir como query** lo deja en un editor.
   - **Ejecutar** lo corre, previa confirmación, sentencia por sentencia. Se
     detiene en el primer error y dice cuál fue.
   - Después vuelve a leer esa base. Si algo falló, lo que no se aplicó sigue
     pendiente.
   - No se puede sincronizar sobre una conexión de solo lectura.

## Qué genera el script

El orden evita errores de dependencias:

1. los tipos, dominios, secuencias y catálogos de texto completo nuevos o
   que cambian (los tipos, ordenados por dependencia: primero los que usan
   los demás);
2. las claves foráneas que cambian o dependen de columnas que cambian;
3. las tablas que se borran;
4. los índices y claves primarias que cambian;
5. las columnas;
6. la clave primaria y los índices otra vez;
7. las tablas nuevas;
8. las claves foráneas;
9. el borrado de los tipos, dominios, secuencias y catálogos que ya no están,
   después de las tablas que los usaban.

Además:

- **Vistas:** las que usan una tabla cuyas columnas cambian de tipo o se
  borran se borran antes y se vuelven a crear después. PostgreSQL, por
  ejemplo, no deja cambiar una columna que usa una vista. Las vistas sobre
  tablas que se borran solo se borran.
- **Vistas y procedimientos:** se reemplazan borrando la versión anterior y
  ejecutando la definición del otro lado. Solo se pasan entre bases del mismo
  motor.

Cómo cambia una columna cada familia (el detalle por motor está en
`docs/soporte-por-motor.md`):

| Familia | Cambio de columna |
|---|---|
| PostgreSQL y compatibles | `ALTER COLUMN … TYPE … USING`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT` |
| SQL Server, Azure SQL | `ALTER COLUMN … [NOT] NULL`. El default (una restricción con nombre) y los índices sobre la columna se sacan antes y se reponen después. |
| MySQL y compatibles | `MODIFY COLUMN` con la columna completa |
| Oracle | `MODIFY (…)` |
| SQLite, libSQL | La tabla se reconstruye: tabla nueva, copia de los datos, borrado y renombre. Agregar columnas se hace en el lugar. |

Cuando los dos lados son de motores distintos, lo que se pasa se convierte con
el mismo conversor de la migración (tipos, defaults, nombres), y los avisos
de la conversión se muestran al pasar el cambio. Lo que el motor de destino no
tiene se descarta con un aviso:

- `CHECK`, opciones de índice, índices de texto completo, proyecciones y
  `EXCLUDE`: se descartan con aviso.
- Las columnas `INCLUDE` de un índice se conservan solo si el destino las
  soporta.

## Comparación

- **Emparejamiento:** las tablas se emparejan por esquema y nombre, o solo
  por nombre si se comparan dos esquemas.
  - Por defecto sin distinguir mayúsculas (opción "Ignorar mayúsculas").
  - Columnas: por nombre.
  - Índices: por nombre y, si no, por sus columnas, porque los nombres
    generados suelen cambiar.
  - Claves foráneas: por lo que vinculan (columnas, tabla y columnas
    referenciadas).
- **Tipos:** son iguales si se escriben igual, sin importar mayúsculas ni
  espacios, o si significan lo mismo: `int` = `integer` = `int4`. Entre
  motores distintos se compara el tipo lógico.
- **Valores por defecto:** se comparan sin el envoltorio que agrega cada
  catálogo (`('x')`, `'x'::text`).
- **Comentarios:** cuentan salvo con "Ignorar comentarios".
- **Código:** vistas y procedimientos se comparan con los espacios colapsados.
- **Índices:** además de las columnas cuentan las columnas `INCLUDE` (o
  `STORING`), el orden (`DESC`, `NULLS`), el filtro, las opciones propias del
  motor (fillfactor, compresión, visibilidad, parámetros de almacenamiento…)
  y el tipo: texto completo, espacial y los específicos de cada motor (por
  ejemplo `gin`, `gist`, `bitmap`, columnstore, `EXCLUDE`). Un método que
  DBine no conoce se conserva tal cual, no se recrea como btree.
- **Restricciones CHECK:** se leen del catálogo del motor. En algunos el
  nombre lo genera el motor (`SYS_C…` en Oracle, `INTEG_n` en Firebird) y
  DuckDB no guarda nombres.
- **Propiedades de la tabla:** entre dos bases del mismo motor se comparan las
  opciones de la tabla y de sus columnas (por ejemplo la clave de clustering
  en Snowflake, `SHARD KEY` en SingleStore, `ttl` en GreptimeDB o el
  `ASSUME` de ClickHouse).
- **Otros objetos de código:** además de vistas, procedimientos, funciones y
  triggers, según el motor se comparan secuencias, sinónimos, tipos,
  dominios, catálogos y listas de palabras irrelevantes de texto completo
  (SQL Server), tablas virtuales (SQLite: FTS5, FTS4, R\*Tree) y diccionarios
  (ClickHouse). Se reemplazan con `DROP` y `CREATE`, y solo entre bases del
  mismo motor.

Qué se compara en cada motor y qué no: `docs/soporte-por-motor.md`, sección
"Comparar esquemas".

## Comandos

Están en `docs/api-comandos.md`, en la sección "Comparar esquemas".
