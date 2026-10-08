# Ejecutar en varias bases

Ejecuta el código del editor (o la selección) en **varias bases de la misma
conexión** y junta los resultados. Sirve, por ejemplo, para consultar el mismo
esquema en cada base de un cliente o para aplicar un cambio a todas.

## Dónde está

En el editor de consultas, botón **Ejecutar en varias bases…** de la barra.
Aparece solo en los motores que tienen varias bases por conexión (los que
muestran las bases en el explorador); la lista de los que no, está en
[`soporte-por-motor.md`](soporte-por-motor.md#ejecutar-en-varias-bases).

Si el editor está vacío, avisa **No hay código para ejecutar**.

## Cómo se eligen las bases

- El diálogo lista las bases de la conexión (si todavía se está abriendo,
  espera a que cargue).
- El **filtro** admite `*` (por ejemplo `*tenant-n*`). Sin `*` busca el texto
  dentro del nombre. No distingue mayúsculas.
- **Todas** y **Ninguna** actúan sobre lo que el filtro muestra, así que se
  pueden combinar varios filtros.
- Se ve cuántas bases hay elegidas, cuántas se ven y cuál es la **actual**
  (la de la pestaña).
- La elección se **recuerda por conexión**. La primera vez viene elegida la
  base de la pestaña.
- **Ejecutar en N bases** se habilita con al menos una elegida.

## Cómo corre

- Cada base usa una **sesión propia** (nunca la del explorador), de **a 4 a
  la vez**. El resto espera su turno.
- El código se ejecuta como lo hace el editor en ese motor: sentencia por
  sentencia, lote por lote o entero. Respeta el máximo de filas del editor,
  **por resultado y por base**, y la opción de seguir tras un error.
- Corre como **tarea**: si cerrás el diálogo sigue en segundo plano y se ve
  en el panel **Tareas**. El diálogo muestra el estado de cada base y
  **N de M bases terminadas**.

### Seguridad

- Una conexión de **solo lectura** sigue rechazando todo lo que no sea
  lectura, también acá.
- Si el código **no es solo de lectura**, pide confirmación antes de
  ejecutar, con la primera sentencia que escribe y en cuántas bases va a
  correr. Lo mismo si el motor no es SQL y no se puede verificar. Nunca se
  ejecuta sin que lo confirmes.

## Los resultados

- Si el **primer resultado** de todas las bases que terminaron bien tiene las
  **mismas columnas** (sin distinguir mayúsculas), se juntan en **una sola
  grilla**. Su primera columna es **`base`** y trae el nombre de la base de
  donde salió cada fila. Las filas siguen el orden en que elegiste las
  bases. La pestaña dice **Resultado (N bases)**.
- Si las columnas no coinciden, no se junta nada: hay **una pestaña por base**
  (y por cada resultado, si una base devolvió varios).
- Si hay grilla junta, los demás resultados de cada base van en pestañas
  aparte, con el nombre de la base y un número.
- La grilla junta se exporta y se copia como cualquier otra (las filas
  cargadas).
- La pestaña **Mensajes** trae una línea por base: estado, filas (o filas
  afectadas) y tiempo, los mensajes del servidor y el error si lo hubo. Arriba
  del resultado se ve el resumen, por ejemplo **3 de 4 bases bien, 1 con
  error**.

## Errores por base

Un error en una base **no frena a las demás**. Cada base termina en uno de
estos estados:

- **Bien.**
- **Error:** no se pudo abrir la base o el código falló (se informa el
  primer error). Sus resultados parciales quedan en su pestaña.
- **Cancelada:** se la interrumpió mientras corría.
- **No se ejecutó:** se canceló antes de que le tocara el turno.

Las bases con error, canceladas o sin ejecutar **no entran en la grilla
junta**; las que terminaron bien se juntan igual. Dentro de una base, tras un
error el código sigue o se corta según la opción de seguir tras un error del
editor (por defecto, la del motor); un error fatal o una conexión perdida
siempre lo corta.

## Cancelar

Desde el diálogo (**Cancelar**) o desde el panel **Tareas**. No arranca
ninguna base más (quedan **No se ejecutó**) y se interrumpen las que están
corriendo, como el Cancelar del editor. Si una sentencia no se detiene en 5
segundos, se cierra la conexión de esa base y queda **Cancelada**. Los
resultados de las bases que ya terminaron se conservan.

## Límites

- Solo **bases de una misma conexión**; no mezcla conexiones.
- **4 bases a la vez.**
- El máximo de filas del editor vale **por resultado y por base**, así que la
  grilla junta puede tener hasta ese máximo multiplicado por las bases.
- Solo se junta el **primer resultado** de cada base.
- No hay transacción entre bases: si el código escribe, las bases que ya
  terminaron **no se deshacen** al fallar o cancelar otra.

## Particularidades por motor

Usa solo la ejecución de cada driver (`Session::execute`) y la forma de
partir el script de cada motor, así que funciona igual en todos los que
tienen varias bases. Los que no, y lo probado contra servidores reales, están
en [`soporte-por-motor.md`](soporte-por-motor.md#ejecutar-en-varias-bases).

## Contrato

Comandos `run_multi_db` y `cancel_multi_db`
(`src-tauri/src/commands/multi_db.rs`). Evento `multi-db-progress` por cada
base que termina (estado, filas, tiempo, error, `done` / `total`). La
acción se ofrece según `DriverInfo::databases_label`. No agrega métodos al
contrato de los drivers.
