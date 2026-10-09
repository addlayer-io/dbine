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
- **Actualizaciones:** los drivers instalados se actualizan solos en segundo
  plano. Cada fila muestra la versión en uso y, si corresponde, un estado
  («Descargando la versión X…», «se usa desde la próxima conexión», «necesita
  DBine X o posterior», «falló y se volvió a la Y»). «Buscar actualizaciones»
  consulta el índice al instante y «Volver a la anterior» deja la versión
  actual y vuelve a la previa.
- **Sin internet:** la variable `DBINE_DRIVERS_DIR` apunta a una carpeta que
  ya tiene los drivers (`dbine-driver-<crate>[.exe]`), y entonces no se
  descarga ni se actualiza nada. Más en [Variables de entorno](#variables-de-entorno).

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
| Catálogo | `crates/dbine-drivers` (feature `plugins`) | La lista de drivers descargables que la app lleva adentro, con el `DriverInfo` de cada uno, el archivo, el tamaño y el SHA-256 de su versión de piso. |
| Índice | `crates/dbine-plugin/src/index.rs`, `state.rs`, `updater.rs` | Verifica el índice firmado, elige la versión de cada driver, guarda el estado local y descarga las actualizaciones. |

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
  descargar. Al arrancar se borran las versiones que no son el piso, la
  activa ni la anterior; la anterior queda a propósito, para poder volver.
- **Al cerrarse la app**, cada host termina solo cuando se le cierra la
  entrada estándar, aunque la app se haya caído.
- **Seguridad:** el tamaño y el SHA-256 del piso van fijos dentro de la app;
  los de las versiones más nuevas salen del índice, que está firmado con la
  clave del actualizador. Un archivo que no coincide se borra y no se
  ejecuta. Las contraseñas
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

La app sale seguido; un driver puede no cambiar en años, o arreglarse varias
veces entre dos versiones de la app. Por eso cada driver tiene su versión y
se publica solo, sin release de la app.

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
  tiene su índice firmado (`index-<target>.json` y `.sig`) con todas las
  versiones publicadas.
- **El piso:** el catálogo que la app lleva adentro (`plugins.json`) fija, por
  driver, la versión con la que se armó la app. Es el piso: esa versión
  siempre se puede descargar y la app nunca usa una anterior. El catálogo ya
  no decide la versión que corre, solo el mínimo.
- **Lo que muestra la app** (campos de conexión, capacidades) sale de la
  versión del driver que se usa, tomada del índice en caché al arrancar. Por
  eso los campos nuevos en `DriverMeta`, `DriverInfo` o `Capabilities`
  llevan `#[serde(default)]`: la app tiene que poder leer lo que dijeron los
  drivers publicados antes, y los drivers tienen que aceptar la
  configuración de un manifiesto más viejo.

### Cómo elige la app

Para cada driver, la app toma, entre las versiones del índice, la más alta
(semver) que cumpla todo esto (`index::resolve`):

1. Tiene el mismo protocolo y el mismo epoch que el piso. Un driver de otro
   protocolo o epoch no se puede usar con esta app.
2. No es más vieja que el piso.
3. No está retirada (`yanked`).
4. No falló antes en esta máquina (ver [Si una versión falla](#si-una-versión-falla)).
5. Su `min_app` es menor o igual a la versión de la app.

Si nada califica, o no hay índice, usa el piso. Si hay una versión más nueva
que la app no puede correr por su `min_app`, el driver sigue en la versión
actual y Configuración › Drivers dice «Hay una versión más nueva que necesita
DBine X o posterior»: hay que actualizar la app.

### Cómo se actualiza

1. **Índice firmado.** La app baja `index-<target>.json` y su `.sig` unos 10
   segundos después de arrancar, cada 6 horas y con «Buscar actualizaciones»
   en Configuración › Drivers. Verifica la firma (minisign) con la clave
   pública del actualizador de la app (`src-tauri/tauri.conf.json`) y lo guarda
   en `components/drivers/`. Una conexión nunca espera esta búsqueda.
2. **Sin repetir lo viejo.** Si el `seq` del índice es menor que el último
   aceptado, se rechaza (un índice viejo repetido no hace volver atrás). Sin
   internet se usa el índice en caché si su firma verifica; si no, el piso.
3. **Descarga en segundo plano.** Solo para los drivers que ya están
   instalados: baja el archivo (con reanudación, SHA-256 y rename atómico),
   y al terminar lo deja como versión activa y guarda la anterior. Un driver
   que nunca se usó no se baja antes de tiempo: la primera conexión descarga
   directamente la versión elegida, y si eso falla, el piso.
4. **Host nuevo para las conexiones nuevas.** El archivo lleva la versión en
   el nombre y no pisa al que está corriendo. La próxima conexión arranca un
   host nuevo; las sesiones abiertas (y un profiler activo) siguen en el viejo
   hasta que se cierran. Configuración › Drivers avisa «La versión X se usa
   desde la próxima conexión». La copia nativa entre dos sesiones de hosts
   distintos no está soportada; la transferencia vuelve a la copia genérica.
5. **Opciones nuevas.** Los campos de conexión se cargan una vez, al
   arrancar. Si la versión activa trae opciones que la sesión no cargó, la
   pantalla pide reiniciar DBine para verlas.

Con `DBINE_DRIVERS_DIR` no hay búsqueda ni actualizaciones: solo el piso, de
esa carpeta.

### Si una versión falla

Una versión se marca como mala en esta máquina, y no se vuelve a elegir, si
el host no arranca, termina antes de saludar, tarda más de 10 segundos en el
saludo o dice ser de una versión distinta de la esperada. La app borra ese
archivo, vuelve a la versión anterior (o al piso) y reintenta la conexión una
vez. La pantalla muestra «La versión X falló y se volvió a la Y». El piso
nunca se marca ni se borra: es el último recurso.

**Volver a la anterior** (Configuración › Drivers) hace lo mismo a mano, con
motivo `user`. Después de confirmar, la versión actual no se vuelve a usar y
las conexiones abiertas siguen con ella hasta que se cierran. No se ofrece si
el driver ya está en la versión del piso.

El estado local (versión activa, anterior, versiones malas y el último `seq`)
está en `components/drivers/state.json`. Al arrancar se borran los hosts que
no son el piso, el activo ni el anterior.

### Campos del índice

`index-<target>.json`:

```json
{ "target": "aarch64-apple-darwin", "schema": 2, "seq": 1760000000,
  "drivers": { "postgres": { "0.1.10+p1.e3": {
      "file": "…gz", "size": 0, "sha256": "…", "own_hash": "…",
      "shared_hash": "…", "manifest": [],
      "min_app": "0.1.9", "yanked": null, "published_at": "…Z" } } } }
```

| Campo | Qué es |
|---|---|
| `schema` | Versión del formato del índice. Hoy `2`. |
| `seq` | Número que crece con cada publicación (hora Unix). La app rechaza un índice con `seq` menor al último aceptado. |
| `min_app` | La app más vieja que corre ese host. Sin el campo, cualquier app. |
| `yanked` | El motivo por el que se retiró esa versión; `null` si no. Una versión retirada no se elige. Sin el campo, no está retirada. |
| `published_at` | Cuándo se publicó (UTC). |

`manifest` es lo que los drivers dicen de sí mismos (`DriverMeta`). Las apps
anteriores a este esquema no leen el índice: usan solo su catálogo, así que
los campos nuevos no las afectan.

## Publicar un driver

Para publicar un driver sin release de la app:

1. Subí la `version` de `crates/drivers/<crate>/Cargo.toml` y mergeá.
2. Creá y subí el tag `driver-<pkg>-v<versión>`, por ejemplo
   `driver-postgres-v0.1.10`. El tag tiene que decir la misma versión que el
   `Cargo.toml`, o el workflow falla.

`.github/workflows/drivers.yml` compila ese driver en las cuatro
plataformas (los mismos runners y la misma preparación que el release de la
app, en `.github/actions/driver-hosts`). Solo cuando las cuatro compilaron,
sube los archivos al release `drivers` y después fusiona sus entradas en el
índice publicado, lo firma y sube `index-<target>.json` y su `.sig`. Al final
poda (ver [Poda](#poda)). Los release de la app y los de drivers no se pisan:
cada uno vuelve a leer el índice publicado antes de fusionar, y el trabajo
de publicación de cada plataforma corre de a uno.

**Prueba sin publicar:** Actions › drivers › Run workflow, con el driver y
`dry_run` activado (es el valor por defecto). Compila el driver de la rama
elegida, corre las verificaciones y deja los hosts como artefactos del
workflow. Con `dry_run` desactivado, publica.

**Verificaciones** (`scripts/build-driver-hosts.py <target> <out> <url> <índice> --only <pkg> --no-catalog`):

- Si el código del driver cambió y su versión no, falla y dice cuál subir.
  Si cambió código compartido con otros drivers, solo avisa.
- La versión nueva tiene que ser mayor que la más alta publicada con el mismo
  protocolo y epoch.
- Los ids de motor del manifiesto tienen que incluir todos los de la versión
  anterior: las apps instaladas cambian solas a la nueva y un motor no puede
  desaparecer.
- Tiene que existir una app publicada que pueda correrla (ver `min_app`). Si
  no, falla: publicá la app primero.
- Un problema de otro driver solo avisa; en el release de la app, falla.

La firma del índice usa los secretos `TAURI_SIGNING_PRIVATE_KEY` y
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, que solo se pasan por `env:` del paso.
Si falta el primero, la publicación falla.

### Cómo se calcula `min_app`

Un host nuevo solo corre con las apps que hablan igual por el pipe. Eso se
resume en el `wire_hash`: un hash del código de `crates/dbine-plugin/src` y
`crates/dbine-driver/src` (sin `tests` ni `testdata`). `scripts/driver_index.py`
lo calcula para el código actual y para cada tag de las últimas cinco
versiones de la app (`vX.Y.Z`).

- `min_app` es la app más vieja entre esas cinco cuyo `wire_hash` es igual al
  del código actual.
- En un release de la app, la versión que se publica cuenta como piso, así que
  siempre hay respuesta.
- En un release solo de driver, si ninguna de las cinco coincide, falla con
  «publicá la app primero».
- Un driver puede subir ese mínimo a mano, nunca bajarlo, en su `Cargo.toml`:

```toml
[package.metadata.dbine]
min-app = "0.2.0"
```

Para verlo en local: `scripts/driver_index.py wire-hash` y
`scripts/driver_index.py min-app`.

## Release

`.github/workflows/release.yml` corre, en cada plataforma:

1. Baja el índice publicado (`index-<target>.json` del release `drivers`).
2. `scripts/build-driver-hosts.py <target> driver-hosts <url de drivers> <índice>`
   compila solo los drivers cuyo id no está publicado, verifica los que se
   reutilizan, escribe el índice nuevo y `plugins.json` (el piso), y comprueba
   que la app puede leerlo (`dbine-plugin-host --check-catalog`). A cada host
   nuevo le pone su `min_app`.
3. En los tags, sube los drivers nuevos al release `drivers`, antes de armar
   la app, y después fusiona el índice (`scripts/driver_index.py publish`: lee
   el publicado en ese momento, agrega lo nuevo, lo firma y sube el índice con
   su `.sig`). Así una app publicada nunca apunta a un driver que no está.
4. Arma la app con `--features plugins` y
   `DBINE_PLUGIN_CATALOG=driver-hosts/plugins.json`. Sin la variable, la app
   compila igual pero no ofrece drivers descargables (y cargo avisa).
5. Cuando las cuatro plataformas terminaron, poda el release `drivers`.

En las corridas manuales, los drivers nuevos quedan como artefacto y no se
publican.

### Poda

Un release de GitHub admite hasta 1000 archivos, así que
`scripts/prune-driver-assets.py` borra los que ya no hacen falta. Se conserva
la unión de:

- **(a)** los hosts que nombran los catálogos de las últimas cinco versiones
  de la app. Los nombres se arman desde las fuentes de cada tag, igual que
  `build-driver-hosts.py`;
- **(b)** la versión más nueva, no retirada, de cada driver (por protocolo,
  epoch y plataforma);
- **(c)** a lo que cambia sola cada una de esas cinco versiones, aplicando la
  regla de la app con la versión y el catálogo de ese tag contra el índice
  actual.

Lo demás se borra, incluidas las versiones retiradas que no estén en (a), y
sale de su índice, que se vuelve a firmar y subir. Un host que no está en el
índice y se subió hace menos de un día no se toca: es un release de driver
que todavía no publicó su índice.

**Fail-safe:** si no están todos los archivos que espera la versión más
nueva de la app, o falta el índice de una plataforma, no borra nada. Con
`--dry-run` muestra lo que borraría. Una app más vieja que las últimas cinco
sigue con los drivers que tiene, pero no baja nuevos de los que ya se podaron:
tiene que actualizarse.

Corre después de un release de la app y después de uno de driver, de a uno a
la vez.

## Variables de entorno

| Variable | Qué hace |
|---|---|
| `DBINE_DRIVERS_DIR` | Carpeta con los drivers ya descargados (`dbine-driver-<crate>[.exe]`). No se descarga ni se busca nada: solo el piso. |
| `DBINE_DRIVERS_INDEX_URL` | Otro índice (un espejo, pruebas locales); los archivos de los drivers se buscan junto a él. Se verifica con la firma igual que el oficial. |
| `DBINE_DRIVERS_PUBKEY` | Otra clave pública para verificar el índice. Solo en builds de debug (con una clave de prueba); en release se ignora. |

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
