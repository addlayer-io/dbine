# Caché del explorador

Al abrir una conexión, el árbol aparece de inmediato con lo que mostró la
última vez: las bases, los objetos de cada base y las columnas de cada
objeto. Mientras tanto DBine le pregunta al servidor, como siempre.

## Cómo funciona

Es *stale-while-revalidate*:

1. Al conectar, o al expandir una base o un objeto, la UI pide lo guardado
   (`get_cached`) y lo muestra en el acto.
2. En paralelo, el servidor responde de la forma habitual. Mientras espera,
   el nodo muestra el aviso **actualizando…**.
3. Cuando llega la respuesta, **gana el servidor**: reemplaza lo que estaba
   en pantalla y lo guardado. Un objeto que ya no existe desaparece.

Si no hay nada guardado (primera vez, o se limpió), el árbol se comporta como
antes: espera al servidor. Si el servidor falla, el error se muestra igual que
sin caché.

## Qué se guarda

- Las bases de la conexión, los objetos de cada base y las columnas de cada
  objeto.
- Solo nombres y estructura. **No** se guardan filas ni datos de las tablas, ni
  contraseñas ni ningún secreto.

## Dónde vive

En `cache.db`, un archivo SQLite al lado del estado de la app. No es parte del
estado:

- no se sincroniza con el backup en la nube;
- si se pierde o se borra, no se pierde nada: se vuelve a llenar a medida que
  se navega.

Si el archivo no se puede abrir, DBine sigue sin caché y lo deja en el log.

## Cuándo se borra

- Al **eliminar la conexión**, se borra todo lo suyo.
- Al **guardar la conexión con otro** driver, host, puerto, base, usuario u
  opciones: apunta a otro servidor o a otro login, así que su árbol guardado
  ya no le corresponde. Cambiar el nombre, la carpeta o el color no lo borra.

## Qué no se cachea

- Las búsquedas de keys en Redis y etcd.
- Los datos de las tablas (las filas).

## Código

- `crates/dbine-core/src/cache.rs`: `ExplorerCache` (`get`, `put`, `remove`,
  `forget_connection`). Cada entrada se identifica por conexión, base, tipo
  (`databases`, `objects`, `columns`) y objeto.
- `src-tauri/src/commands/explorer.rs`: `get_cached` y la escritura de la
  respuesta del servidor en `list_databases`, `list_objects` y `get_columns`
  (las bases también se guardan al conectar, en `connections.rs`).
- `src-tauri/src/commands/connections.rs`: el borrado al eliminar o cambiar la
  conexión.
- `web/src/stores/connections.ts`: los estados `fromCache` (bases) y `stale`
  (objetos y columnas) que la UI muestra como "actualizando…".

## Contrato

No toca a los drivers: la caché guarda lo que ya devuelven `list_databases`,
`list_objects` y `get_columns`. Es igual en todos los motores.
