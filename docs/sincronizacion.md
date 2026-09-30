# Sincronización en la nube

DBine guarda todo en una base local (SQLite, en la carpeta de configuración de
la app). La sincronización mantiene una **copia de respaldo cifrada** de esa
base en la cuenta del propio usuario. Así, en otra máquina se conecta la misma
cuenta y se recupera todo.

Se abre en **Configuración › Sincronización**, desde el engranaje de la barra
lateral o con ⌘,.

## Qué se guarda

- Conexiones: nombre, color, carpeta y configuración (host, usuario, opciones…).
- Contraseñas y campos secretos de las conexiones que las guardan.
- Carpetas del explorador.
- Queries guardadas.
- Preferencias: formato de copia, filas máximas y demás.

No se guardan el tamaño de los paneles, las pestañas abiertas ni el estado del
explorador: eso es de cada máquina.

## Seguridad

- **Cifrado en la máquina, antes de salir.** El backup completo se cifra con
  XChaCha20-Poly1305. La clave se deriva de una frase que elige el usuario, con
  Argon2id (64 MiB, 3 iteraciones y sal aleatoria). Se cifra todo, no solo las
  contraseñas: hosts, usuarios, queries y preferencias. Lo único legible es un
  encabezado con la fecha, el nombre del equipo, la versión y los parámetros del
  cifrado. Ese encabezado está autenticado, así que si se altera, el backup no
  abre.
- **En la cuenta del usuario.** El backup va a la carpeta privada de la app en
  Google Drive (`appDataFolder`), a la carpeta de la app en OneDrive
  (`Aplicaciones/DBine`) o a una carpeta del disco. AddLayer no tiene servidores
  para esto y nunca recibe los datos.
- **Permisos mínimos.** En Google se pide `drive.appdata` y en Microsoft
  `Files.ReadWrite.AppFolder`. Con eso DBine solo ve su propia carpeta y ningún
  otro archivo del usuario.
- **Credenciales cifradas en cada máquina.** La frase clave y los tokens de la
  cuenta quedan en el archivo cifrado de secretos, con la llave en el llavero
  del sistema de cada máquina, nunca en el archivo de estado ni en los logs. El login se hace en el navegador del sistema (OAuth
  con PKCE y redirección a `127.0.0.1` o `localhost`), así que DBine nunca ve la
  contraseña de la cuenta.
- **Sin la frase no hay acceso.** Nadie puede leer el backup sin ella: ni
  Google, ni Microsoft, ni AddLayer. Por lo mismo, si se pierde, no hay forma
  de recuperarlo.

El formato está en `crates/dbine-sync/src/crypto.rs`. Un backup se puede
abrir sin la app:

```sh
DBINE_BACKUP_PASSPHRASE='…' cargo run -p dbine-sync --example open_backup -- dbine-backup.json
```

## Cómo sincroniza

- **Al activarla:**
  - si no hay backup, se elige una frase (10 caracteres como mínimo) y se sube
    lo de esta máquina;
  - si ya hay uno, se escribe su frase y se restaura en esta máquina, o bien se
    lo reemplaza con lo de esta.
- **Automática** (se puede apagar):
  - sube unos 4 segundos después del último cambio;
  - trae los cambios de otras máquinas al abrir la app y cada 10 minutos.

En cada sincronización se comparan dos cosas: si esta máquina tiene cambios sin
subir y si el backup cambió desde la última vez que se vio.

| Local | Backup | Qué hace |
|---|---|---|
| sin cambios | igual | nada |
| con cambios | igual | sube |
| sin cambios | cambió | restaura |
| con cambios | cambió | gana el más reciente |

**Nada se pierde en silencio:**
- Antes de restaurar, lo que había en la máquina se guarda en una copia local
  cifrada: `backups/` en la carpeta de configuración, con las últimas 10. Se
  puede volver a cualquiera desde Configuración.
- Antes de reemplazar un backup que escribió otra máquina, ese backup se guarda
  en la nube como `dbine-backup.previous.json`.

**Cambios de frase o de acceso:**
- Si la frase se cambia en una máquina, las demás la piden la próxima vez que
  sincronizan.
- Si se revoca el acceso a la cuenta, se pide conectarla de nuevo.

## Registrar la app en Google y Microsoft

Para que el login funcione, la app tiene que estar registrada con cada
proveedor. Es un trámite que AddLayer hace **una sola vez**; los usuarios no
registran nada.

Mientras falte el registro, ese proveedor aparece como "No disponible en esta
versión". La opción **Carpeta** funciona siempre.

### Google Drive

1. En <https://console.cloud.google.com>, crear un proyecto (por ejemplo,
   "DBine").
2. **APIs y servicios › Biblioteca**: habilitar **Google Drive API**.
3. **Pantalla de consentimiento de OAuth** (Google Auth Platform):
   - tipo **Externo**;
   - nombre "DBine", correo de soporte y logo;
   - en **Acceso a datos**, agregar el alcance
     `https://www.googleapis.com/auth/drive.appdata`.
4. **Clientes › Crear cliente**: tipo **App de escritorio**. Esto da un
   *client ID* y un *client secret*. En apps de escritorio el secret no es
   secreto: va dentro de la app, y lo que protege el intercambio es PKCE.
5. Mientras la app esté en modo **Prueba**, solo pueden entrar los usuarios
   agregados en **Público › Usuarios de prueba**. Para publicarla hay que pasar
   la verificación de Google. `drive.appdata` no es un alcance restringido, así
   que no requiere la auditoría de seguridad.

### OneDrive (Microsoft)

1. En <https://entra.microsoft.com>, ir a **Registros de aplicaciones › Nuevo
   registro**:
   - nombre "DBine";
   - cuentas: **cualquier directorio organizativo y cuentas personales de
     Microsoft**.
2. **Autenticación › Agregar plataforma › Aplicaciones móviles y de escritorio**:
   - URI de redirección `http://localhost`;
   - activar **Permitir flujos de clientes públicos**.
3. **Permisos de API › Microsoft Graph › Delegados**: `Files.ReadWrite.AppFolder`,
   `User.Read` y `offline_access`.
4. No hace falta secreto: es un cliente público con PKCE. El *Application
   (client) ID* es lo único que se necesita.

### Dónde van los IDs

- **En el build** (lo normal para distribuir), como variables de entorno al
  compilar:

  ```sh
  DBINE_GOOGLE_CLIENT_ID=… DBINE_GOOGLE_CLIENT_SECRET=… DBINE_MICROSOFT_CLIENT_ID=… cargo tauri build
  ```

  Para compilar en local sin tipearlas, pueden ir en un archivo `.env` en la
  raíz del repo (git lo ignora):

  ```sh
  DBINE_GOOGLE_CLIENT_ID=….apps.googleusercontent.com
  DBINE_GOOGLE_CLIENT_SECRET=…
  DBINE_MICROSOFT_CLIENT_ID=…
  ```

  Si una variable también está en el entorno, gana el entorno (así funciona
  en CI, con los secretos del repositorio). Después de crear el `.env` por
  primera vez, correr `cargo clean -p dbine-sync` una vez; de ahí en más,
  cada cambio en el archivo recompila solo lo necesario.

- **En tiempo de ejecución** (para probar sin recompilar), en el archivo
  `cloud-clients.json` dentro de la carpeta de configuración de la app. En
  macOS es `~/Library/Application Support/com.addlayer.dbine/`. Lo que diga el
  archivo pisa lo del build:

  ```json
  {
    "google_client_id": "….apps.googleusercontent.com",
    "google_client_secret": "…",
    "microsoft_client_id": "…"
  }
  ```

## Código

- `crates/dbine-sync`: formato y cifrado (`crypto.rs`), motor de decisiones
  (`engine.rs`), OAuth (`oauth.rs`) y los proveedores (`gdrive.rs`,
  `onedrive.rs`, `folder.rs`).
  - Tests: cifrado, OAuth con loopback, el motor con dos máquinas y conflictos,
    y los dos proveedores contra imitaciones locales de sus APIs, incluida la
    renovación de tokens tras un 401.
- `src-tauri/src/sync.rs`: el manejador y la tarea automática.
- `src-tauri/src/commands/sync.rs`: los comandos (docs/api-comandos.md).
- `web/src/components/SettingsDialog.vue`, `web/src/stores/sync.ts` y
  `web/src/stores/settings.ts`: la UI y las preferencias.
