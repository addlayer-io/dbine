# Buscar en la base

Busca un texto en los **nombres** de los objetos de una base, en los **nombres
de columna** y en el **código** de vistas, procedimientos, funciones y
triggers. Sirve para saber dónde se usa una tabla, una columna o una función
antes de cambiarla.

## Dónde está

Clic derecho sobre una **base** › **Buscar en la base…**. Se abre en su propia
pestaña (**Buscar**).

## Qué se puede buscar

- El **texto** a buscar.
- **Nombres:** los nombres de los objetos (tablas, vistas, rutinas…) **y los
  nombres de columna**. Una columna aparece con su tabla y su tipo.
- **Código:** el texto de las definiciones de los objetos que tienen código.
  Las tablas no entran en la búsqueda de código (su definición es un DDL que
  genera el motor, no código que alguien escribió); sus nombres sí.
- **Distinguir mayúsculas** y **Palabra completa** (el texto no puede estar
  dentro de un identificador más largo: `id` no coincide con `user_id`).
- **Todos los tipos**, o un filtro por tipo de objeto (y **Columnas**).

## Los resultados

Los resultados se agrupan por objeto, en el orden en que se encuentran, y
llegan **a medida que avanza la búsqueda**, con el avance (**Revisando N de M
objetos**). Por cada objeto se ve si coincidió en el nombre (**en el
nombre**) y las líneas de código que coinciden, con su número de línea.

- Un clic sobre un resultado abre el objeto: su **definición** si tiene
  código, sus datos si es una tabla o colección, o su estructura. Un clic
  sobre una columna abre la **estructura de su tabla**.
- La búsqueda se puede **cancelar**: los resultados parciales quedan, con
  **Cancelada: resultados parciales**.
- Se corta en un máximo de 2.000 resultados (**Se cortó en el máximo de
  resultados**).
- Los objetos cuyo código no se pudo leer (por ejemplo, por falta de
  permisos) se cuentan aparte (**N objetos no se pudieron leer**); no frenan
  la búsqueda.
- Muestra cuántas definiciones se revisaron.

La búsqueda corre en una **sesión de solo lectura propia**: el explorador
queda libre mientras tanto.

## Particularidades por motor

- Las **columnas** se buscan con la estructura de la base (`database_schema`).
  Si el motor no la ofrece, la búsqueda sigue con nombres y código.
- El **código** se lee de dos maneras, con el mismo resultado:
  - **Por el catálogo del motor, en una sola consulta**, filtrada en el
    servidor: PostgreSQL y su familia, SQL Server, Oracle, SAP HANA,
    Firebird, ClickHouse, Snowflake, BigQuery, Databricks y los perfiles
    ODBC que lo implementan.
  - **Objeto por objeto**, pidiendo la definición de cada uno al motor, con
    avance y resultados parciales: el resto de los motores, y las variantes
    de los anteriores cuyo código solo se puede pedir de a uno.
- Los motores cuyos tipos de objeto no tienen definición (no hay código que
  leer) buscan solo en los nombres y las columnas.

## Contrato

Usa estos métodos de `Session`:

- `list_objects` y `database_schema` (los nombres y las columnas);
- `definition` (el código de un objeto) y, opcional,
  `search_code(&CodeSearch) -> Option<CodeSearchReport>`: la búsqueda en el
  catálogo del motor en una consulta. Por defecto devuelve `None` y la
  búsqueda se hace objeto por objeto. Las líneas se comparan con la misma
  regla (`line_matches`), así que ambos caminos dan los mismos resultados.

Comando: `search_database`, con el evento `code-search-progress` y
cancelable con `cancel_query` sobre `search:<searchId>`. Ver
[`api-comandos.md`](api-comandos.md).
