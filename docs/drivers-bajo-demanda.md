# Drivers bajo demanda

El instalador de DBine trae un solo driver, SQLite. Los demás se descargan la
primera vez que alguien se conecta a ese motor. Quien usa solo SQL Server
baja el driver de SQL Server y nada más.

Un driver descargado se comporta igual que uno incluido: las mismas
funciones, los mismos errores, la misma cancelación y el mismo streaming de
filas. Los tests de paridad lo comprueban (`crates/dbine-plugin-host/tests/parity.rs`).

## Cómo lo ve el usuario

- **Primera conexión a un motor:** el nodo de la conexión muestra
  «Descargando el driver de SQL Server, solo esta vez… 45 %» en lugar de
  «Conectando…». Es el mismo aviso que el de DuckDB.
- **Las siguientes:** el driver ya está en disco y arranca directo.
- **Configuración › Drivers:** la lista de drivers descargables. Cada uno se
  puede descargar antes de usarlo (por ejemplo, para trabajar sin internet) o
  borrar. «Descargar todos» los baja uno por uno. La sección aparece solo en
  los builds con drivers descargables.
- **Sin internet:** la variable `DBINE_DRIVERS_DIR` apunta a una carpeta que
  ya tiene los drivers (`dbine-driver-<crate>[.exe]`), y entonces no se
  descarga nada.

## Cómo funciona

Cada crate de `crates/drivers/` se compila como un programa aparte, el
*host* (`crates/dbine-plugin-host`, con la feature de ese crate). La app lo
arranca como proceso hijo y le habla por stdin/stdout.

| Pieza | Dónde | Qué hace |
|---|---|---|
| Protocolo | `crates/dbine-plugin/src/proto.rs` | Marcos con longitud (u32) + MessagePack. Saludo con versión de protocolo y de app. Llamadas multiplexadas por id. |
| Host | `crates/dbine-plugin/src/host.rs` | Atiende las llamadas con los drivers reales. Las sesiones viven en el host. Manda las filas al sink de a una, con contrapresión. |
| Proxy | `crates/dbine-plugin/src/remote.rs` | `RemoteDriver` y `RemoteSession` implementan `Driver` y `Session` reenviando cada método al host. |
| Descarga | `crates/dbine-plugin/src/install.rs` | Baja el host del release, lo retoma si se corta, verifica el SHA-256 y lo instala con un rename atómico. |
| Catálogo | `crates/dbine-drivers` (feature `plugins`) | La lista de drivers descargables que la app lleva adentro, con el `DriverInfo` de cada uno, el archivo, el tamaño y el SHA-256. |

Detalles:

- **La app conoce todos los drivers sin descargarlos.** El catálogo lleva lo
  que cada driver dice de sí mismo (campos de conexión, capacidades,
  plantillas, etc.), así que el formulario de conexión, el explorador y los
  menús funcionan antes de la descarga.
- **Un host por crate, no por motor.** El crate `sqlserver` sirve a SQL
  Server, Azure SQL y Fabric con un solo proceso y una sola descarga.
- **El host se arranca con la primera conexión** y se reutiliza para las
  siguientes. Muere cuando se cierra la app, porque se le cierra la entrada
  estándar.
- **Si el host se cae**, las sesiones abiertas devuelven un error de
  conexión. La conexión siguiente arranca un host nuevo. Si en ese momento
  había un profiler activo, la configuración que el profiler cambió en el
  servidor queda como estaba, igual que si se cortara la red.
- **Versiones:** cada driver tiene su propia versión, separada de la de la
  app (ver [Versiones de los drivers](#versiones-de-los-drivers)). Los hosts
  se guardan en `<datos de la app>/components/drivers/`, con la versión en el
  nombre. Una actualización de la app que no cambia un driver no lo vuelve a
  descargar; las versiones que ya no usa se borran al arrancar.
- **Al cerrarse la app**, cada host termina solo cuando se le cierra la
  entrada estándar, aunque la app se haya caído.
- **Seguridad:** el tamaño y el SHA-256 de cada host van fijos dentro de la
  app. Un archivo que no coincide se borra y no se ejecuta. Las contraseñas
  viajan al host por el pipe, nunca por argumentos ni variables de entorno.
- **Rendimiento:** las queries corren en el host con el mismo código y la
  misma optimización que antes. Lo único que se agrega es el pasaje de las
  filas por el pipe, que es despreciable al lado de la red y la base.

## Desarrollo

`cargo tauri dev` y `cargo test --workspace` compilan todos los drivers
dentro de la app, como siempre. No hace falta descargar nada.

Para probar la app con drivers descargables en local:

```bash
scripts/build-driver-hosts.py aarch64-apple-darwin /tmp/hosts http://127.0.0.1:8000
(cd /tmp/hosts && python3 -m http.server 8000) &
DBINE_PLUGIN_CATALOG=/tmp/hosts/plugins.json cargo tauri dev --features plugins
```

Para Windows desde macOS: `CARGO_BUILD="cargo xwin build"` delante del
script.

## Versiones de los drivers

La app sale seguido; un driver puede no cambiar en años. Por eso cada uno
tiene su versión y su propio lugar de publicación:

- **La versión** es la `version` de `crates/drivers/<crate>/Cargo.toml`. Se
  sube a mano cuando el driver cambia.
- **El id publicado** es `<versión>+p<protocolo>.e<epoch>`, por ejemplo
  `1.2.0+p1.e1`:
  - `protocolo` es `PROTOCOL` en `crates/dbine-plugin/src/proto.rs`. Sube
    solo si cambia un mensaje existente, y entonces se republican todos los
    drivers. Agregar una operación nueva no lo sube: un driver que no la
    conoce contesta «no soportado».
  - `epoch` es `drivers-epoch` en `crates/dbine-plugin-host/Cargo.toml`. Se
    sube a mano para republicar todos los drivers con las mismas versiones,
    por ejemplo por un arreglo de seguridad en una dependencia que comparten
    (rustls, tokio) o un cambio en `dbine-driver` que todos deben tener.
- **Dónde:** en el release permanente `drivers` del repo. Cada plataforma
  tiene su índice (`index-<target>.json`) con todas las versiones publicadas,
  su tamaño, su SHA-256 y lo que cada driver dice de sí mismo. Nada se borra,
  así una app vieja siempre encuentra los suyos.
- **Lo que muestra la app** (campos de conexión, capacidades) sale del
  índice, es decir, de la versión publicada del driver y no del código actual.
  Por eso los campos nuevos en `DriverMeta`, `DriverInfo` o `Capabilities`
  llevan `#[serde(default)]`: la app tiene que poder leer lo que dijeron los
  drivers publicados antes.

**La verificación del release:**

- Si un driver cambió (su crate, los crates de driver que usa o las versiones
  de sus dependencias directas) y su versión no, el release falla y dice cuál
  subir.
- Si cambió código que comparte con otros (`dbine-driver`, `dbine-plugin`,
  dependencias indirectas), el resumen del job lo avisa pero no falla: el
  cambio llega a ese driver cuando se sube su versión o el epoch.
- Un cambio solo en `tests/` o en documentación no cuenta.

## Release

`.github/workflows/release.yml` corre, en cada plataforma:

1. Baja el índice publicado (`index-<target>.json` del release `drivers`).
2. `scripts/build-driver-hosts.py <target> driver-hosts <url de drivers> <índice>`
   compila solo los drivers cuyo id no está publicado, verifica los que se
   reutilizan, escribe el índice nuevo y `plugins.json`, y comprueba que la
   app puede leerlo (`dbine-plugin-host --check-catalog`).
3. En los tags, sube los drivers nuevos y el índice al release `drivers`,
   antes de armar la app: una app publicada nunca apunta a un driver que no
   está.
4. Arma la app con `--features plugins` y
   `DBINE_PLUGIN_CATALOG=driver-hosts/plugins.json`. Sin la variable, la app
   compila igual pero no ofrece drivers descargables (y cargo avisa).

En las corridas manuales, los drivers nuevos quedan como artefacto y no se
publican.

## Agregar o cambiar un driver

Nada especial: un crate nuevo en `crates/drivers/` y su feature en
`crates/dbine-drivers` y en `crates/dbine-plugin-host/Cargo.toml` (una línea
`<crate> = ["dbine-drivers/<crate>"]`, que además tiene que estar en
`default`). El script de release lo toma de ahí.

Para que un driver venga incluido en el instalador, se agrega a `BUILT_IN` en
`crates/dbine-drivers/src/lib.rs`.

Un método nuevo en el contrato (`Driver` o `Session`) necesita su variante en
`Call` y `Reply` (`proto.rs`), su atención en `host.rs` y su reenvío en
`remote.rs`. Los drivers publicados antes contestan «no soportado» hasta
que se les sube la versión. Al cambiar un mensaje que ya existe, se sube
`PROTOCOL`.
