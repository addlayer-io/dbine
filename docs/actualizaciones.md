# Actualizaciones

DBine se fija sola si hay una versión nueva y, donde se puede, se actualiza
sola: descarga la versión nueva, verifica su firma y se reinicia para
terminar. Donde no se puede, ofrece la página de la versión.

## Qué ve el usuario

- **Al abrir DBine**, unos segundos después del arranque, una sola ventana
  (la primera que arranca) pregunta si hay una versión nueva. Si falla, por
  ejemplo sin red, no muestra nada. Una versión marcada con «Omitir esta
  versión» no se vuelve a ofrecer sola.
- **A mano**, desde **Ayuda › Buscar actualizaciones…** o **Configuración ›
  General › Buscar actualizaciones**: siempre contesta. Si no hay nada nuevo,
  dice «Estás en la última versión».
- **El aviso** muestra la versión nueva, la instalada y las notas de la
  versión, con tres botones: «Omitir esta versión», «Más tarde» y
  «Actualizar ahora».
- **Actualizar ahora** descarga el paquete con una barra de avance (los MB
  descargados y el total), verifica la firma y queda en «La actualización
  está lista». La descarga se puede cancelar.
- **Reiniciar para terminar** pasa por el mismo control que Salir: si hay
  ejecuciones en segundo plano en cualquier ventana, pregunta antes y las
  cancela («Cancelar y reiniciar»). Después instala y abre la versión nueva.
  Con «Más tarde», la actualización descargada queda lista hasta que se
  cierre DBine: el próximo «Buscar actualizaciones» ofrece reiniciar sin
  volver a descargar.
- **Si algo falla** (la descarga, la firma o la instalación), el aviso dice
  qué pasó y ofrece «Abrir la página de descarga».
- **Varias ventanas:** solo una muestra la actualización, la que la encontró.
  Si se busca desde otra ventana mientras se descarga, avisa en qué ventana
  está. Si esa ventana se cierra, la descarga sigue y otra ventana puede
  terminarla.

## Qué pasa en cada sistema

| Sistema | Paquete de la actualización | Cómo se instala |
|---|---|---|
| macOS (Apple Silicon e Intel) | `DBine_X_aarch64.app.tar.gz`, `DBine_X_x64.app.tar.gz` | reemplaza `DBine.app` en su lugar. Si la carpeta no se puede escribir, macOS pide la contraseña de un administrador. |
| Windows | `DBine_X_x64-setup.exe` (NSIS) | DBine se cierra, el instalador corre en modo pasivo (solo una barra de avance, sin preguntas) y abre la versión nueva. |
| Linux (AppImage) | `DBine_X_amd64.AppImage` | reescribe el archivo AppImage en su lugar y reinicia. |

Casos en que no se actualiza sola y ofrece la página de la versión:

- **Linux con `.deb` o `.rpm`:** los archivos son del gestor de paquetes del
  sistema, no de DBine. El aviso lo explica: «Instalaste DBine con un paquete
  del sistema: descargá la nueva versión desde su página». Se detecta porque
  falta la variable `APPIMAGE` que define toda AppImage al correr.
- **macOS abierta desde el DMG o sin mover a Aplicaciones** (macOS la corre
  desde una copia temporal, «AppTranslocation»): el aviso sugiere moverla a
  Aplicaciones.
- **Windows instalado con el `.msi`:** el manifiesto solo trae el instalador
  NSIS.
- **Versiones sin manifiesto:** hasta la 0.1.3 no se publicó `latest.json`.
  Tampoco existe en el rato entre que se crea una versión y se sube su
  manifiesto, ni para una plataforma que todavía no se subió. En esos casos,
  y ante cualquier error al leerlo, DBine consulta la última versión en
  GitHub como antes y ofrece su página.

## Cómo funciona

- El plugin `tauri-plugin-updater` (fijado en 2.12: la 2.13 pide tauri 2.12)
  lee `https://github.com/addlayer-io/dbine/releases/latest/download/latest.json`.
- Todo lo maneja el backend (`src-tauri/src/commands/updates.rs`): la
  consulta, la descarga y la instalación. La webview no tiene permisos del
  plugin, así que no puede pedir que se instale nada por su cuenta.
- La firma es minisign (Ed25519). La clave pública está en
  `plugins.updater.pubkey` de `src-tauri/tauri.conf.json`. Con
  `requireSignedVersion`, cada firma lleva la versión para la que se firmó
  (`version:X` en el comentario de confianza): un manifiesto alterado no
  puede ofrecer un paquete viejo como si fuera nuevo.
- El paquete descargado queda en memoria hasta el reinicio (entre 20 y 90 MB
  según el sistema).
- No se agrega telemetría: la adopción de cada versión ya se ve en
  `app_started`, que lleva la versión ([`telemetria.md`](telemetria.md)).

### Comandos

| Comando | Args | Respuesta |
|---|---|---|
| `check_for_update` | `{ manual }` | `UpdateInfo`: `current`, `latest`, `available`, `url` (la página de la versión), `notes`, `published_at`, `installable`, `reason` (`unsigned`, `location`, `package`, `no_manifest`), `phase` (`idle`, `downloading`, `ready`) y `owner` (la ventana que lleva la actualización) |
| `update_download` | `{}` | descarga y verifica; el avance llega a la ventana dueña como evento `update-progress` (`phase`, `downloaded`, `total`). Cancelada: error `cancelled` |
| `update_cancel` | — | corta la descarga |
| `update_install_and_restart` | — | instala y reinicia; la UI lo llama solo desde el control de salida (`requestUpdateRestart` en `quitGuard.ts`) |
| `open_release_page` | `{ url }` | abre la página en el navegador; solo páginas de versiones de DBine |

Si otra ventana pasa a ser la dueña, la anterior recibe
`update-owner-changed` y cierra su aviso. Al terminar la descarga, la dueña
recibe `update-finished` (`ok`, `cancelled`, `error`): así también termina
el aviso de una ventana que tomó una descarga que empezó otra, ya cerrada.

## La clave de firma

- **Se genera una sola vez**, en la máquina del dueño:
  `cargo tauri signer generate -w ~/.tauri/dbine-updater.key`. Deja la
  privada (`dbine-updater.key`, con contraseña) y la pública
  (`dbine-updater.key.pub`).
- **La pública** va, tal cual, en `plugins.updater.pubkey` de
  `src-tauri/tauri.conf.json`. Un test (`cargo test -p dbine updates`) falla si
  quedó el placeholder `DBINE_UPDATER_PUBKEY_PLACEHOLDER`.
- **La privada nunca entra al repo, a los logs ni a un mensaje.** Hay que
  **respaldarla** en un lugar seguro junto con su contraseña: si se pierde,
  las instalaciones existentes dejan de poder actualizarse solas (verifican
  contra la pública que traen) y habría que pedir a cada usuario que instale
  a mano una versión con una clave nueva.
- Una build con el placeholder nunca intenta instalar (ofrece la página) y
  `scripts/make-latest-json.py` se niega a publicar con él. Una versión que
  salga con el placeholder no se puede actualizar sola nunca.

## Builds de release

- **`createUpdaterArtifacts` queda en `false` en `tauri.conf.json`.** Con
  `true`, cualquier `cargo tauri build` sin la clave privada falla, incluidas
  las de desarrollo y las de pruebas. Las builds de release suman
  `--config src-tauri/tauri.updater.conf.json`, que lo activa.
- **Variables**, en el `.env` de la raíz del repo (fuera de git) o en el
  entorno. Nunca se imprimen:
  - `TAURI_SIGNING_PRIVATE_KEY`: la **ruta absoluta** a la clave (el CLI no
    expande `~` y, si no encuentra el archivo, toma el texto como si fuera la
    clave y falla con «failed to decode secret key»), o su contenido.
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: siempre definida, aunque sea vacía.
    Si falta, el CLI la pide por teclado y la build se queda esperando.
- `scripts/check-updater-signing.sh` revisa las dos y la clave pública sin
  mostrar ningún valor. Conviene correrlo antes de cada build.
- **Qué deja cada build** (además de los instaladores de siempre):

| Plataforma | Comando | Paquete y firma |
|---|---|---|
| macOS | `cargo tauri build --target aarch64-apple-darwin --features plugins --config src-tauri/tauri.updater.conf.json` (igual con `x86_64-apple-darwin`) | `target/<triple>/release/bundle/macos/DBine.app.tar.gz` y `.sig` |
| Windows (desde macOS) | `cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --features plugins --bundles nsis --config src-tauri/tauri.updater.conf.json`, con el entorno de LLVM de `scripts/build-windows-from-mac.sh` | `target/x86_64-pc-windows-msvc/release/bundle/nsis/DBine_X_x64-setup.exe` y `.sig` |
| Linux (Docker) | `scripts/linux-release-build.sh` dentro del contenedor, con la clave montada en `/run/secrets/dbine-updater.key` (el encabezado del script tiene el `docker run`) | `linux-out/DBine_X_amd64.AppImage` y `.sig` |

- En Windows, revisar que el log no diga «Failed to add bundler type»: sin
  esa marca la app no sabe que la instaló NSIS y ofrece la página en lugar
  de actualizarse (falla del lado seguro, pero hay que corregirlo).
- El CLI también firma el `.deb` y el `.rpm`. Esos `.sig` no se suben.

## Publicar el manifiesto

`scripts/make-latest-json.py` arma `latest.json`:

```
python3 scripts/make-latest-json.py --version X --out dist/latest.json \
  --merge prev/latest.json \
  --artifact darwin-aarch64-app=target/aarch64-apple-darwin/release/bundle/macos/DBine.app.tar.gz \
  --check-uploaded
```

- Verifica cada `.sig` contra la clave pública del conf (id de clave, firma
  del archivo, firma del comentario de confianza y `version:X`) antes de
  escribir nada.
- Copia cada paquete y su `.sig` a `dist/` con el nombre del asset de la
  versión (los dos `.app.tar.gz` de macOS se llaman igual al salir de la
  build) y lista lo copiado en `dist/upload-files.txt`.
- Las claves de plataforma siempre llevan el instalador:
  `darwin-aarch64-app`, `darwin-x86_64-app`, `windows-x86_64-nsis` y
  `linux-x86_64-appimage`. Así un `.deb`, un `.rpm` o un `.msi` nunca toma un
  paquete de otro tipo.
- `--merge` conserva las plataformas que ya estaban publicadas para la misma
  versión, y su fecha. `--check-uploaded` exige que cada paquete ya esté en
  la versión de GitHub, con el mismo tamaño.
- Las notas salen de `--notes-file` o del texto de la versión en GitHub.
- Tests: `python3 scripts/test_make_latest_json.py`.

**Orden de subida, por plataforma** (cada subida, con el OK del dueño):

1. `gh release upload vX <instalador de siempre> dist/<paquete> dist/<paquete>.sig`
2. `gh release download vX -p latest.json -D prev` (la primera vez no existe).
3. `make-latest-json.py --merge prev/latest.json --artifact … --check-uploaded`
4. `gh release upload vX dist/latest.json --clobber`

`latest.json` siempre va último: mientras falta una plataforma, esa
plataforma sigue ofreciendo la página. La versión nueva tiene que quedar
como «latest» en GitHub (la de `drivers` nunca lo es).

**Por CI** (`.github/workflows/release.yml`), en lugar de la vía local: usa
los secrets `TAURI_SIGNING_PRIVATE_KEY` y `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
y `tauri-action` arma su propio `latest.json` (`includeUpdaterJson`). Sus
claves de plataforma son las genéricas (`darwin-aarch64`, `windows-x86_64`,
`linux-x86_64`…) además de las que llevan el instalador; la app igual se fija
en cómo se instaló antes de instalar (un `.deb`, `.rpm` o `.msi` ofrece la
página), así que los dos manifiestos son válidos.

Una versión se publica por una vía o por la otra, nunca por las dos: si se
mezclaran, CI reemplazaría los paquetes subidos a mano y el `latest.json`
local quedaría con firmas de otros archivos. El workflow lo evita solo: el
job `route` se fija si la versión ya existe en GitHub cuando arranca. Si
existe, la creó la vía local (`gh release create vX` crea el tag y dispara el
workflow), y CI compila sin publicar nada (ni la app, ni los drivers, ni
`latest.json`); deja los instaladores como artefactos del workflow. Si no
existe, la publica CI de punta a punta.

## Pruebas

En builds de desarrollo (`debug_assertions`), y solo en ellas:

- `DBINE_UPDATE_ENDPOINT` reemplaza la dirección del manifiesto. Acepta
  `http://` (el plugin avisa por consola), así sirve un
  `python3 -m http.server` local.
- `DBINE_UPDATE_PUBKEY` reemplaza la clave pública, para firmar con una clave
  de prueba (`cargo tauri signer generate --ci -p "" -w <archivo>`).

Una prueba de punta a punta en macOS:

1. Dos builds `cargo tauri build --debug --bundles app` con otro
   `identifier` (por ejemplo `com.addlayer.dbine.updtest`): una con
   `"version": "0.1.3"` y otra con la versión nueva y
   `--config src-tauri/tauri.updater.conf.json`, firmada con la clave de
   prueba.
2. Copiar la vieja a una carpeta temporal (no a `/Applications`) y servir
   `latest.json` y el `.app.tar.gz` nuevo con `python3 -m http.server`.
3. Abrirla con un `HOME` temporal y `DBINE_UPDATE_ENDPOINT` apuntando al
   servidor: aviso, barra de avance, «lista», el control de salida con una
   tarea corriendo, reinicio y la versión nueva en Configuración.
4. Sin el servidor: el manifiesto no está y DBine ofrece la página.

Windows (instalación pasiva y reinicio) se prueba en una máquina o VM con
Windows.
