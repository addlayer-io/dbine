# DBine: guía para agentes

DBine es un gestor de bases de datos multimotor de escritorio: Tauri 2 con un
core en Rust y una UI en Vue 3 + Element Plus.

## Estructura

- `crates/dbine-driver`: el contrato de los drivers (traits `Driver` y
  `Session`, `DriverInfo`, modelo de resultados, helpers SQL, `ReadOnlySession`).
  Es liviano y no depende de ningún cliente de base.
- `crates/drivers/<motor>`: un crate por motor o por familia de protocolo.
  Cómo se escribe uno: `docs/drivers.md`.
- `crates/dbine-drivers`: el registro de drivers, con una feature de cargo por
  crate.
- `crates/dbine-core`: el estado local en SQLite (conexiones y queries
  guardadas) y los secretos en el llavero del sistema.
- `src-tauri`: los comandos Tauri. Cada uno recibe un solo `args` y usa
  `rename_all = "camelCase"`. Los errores se devuelven como `CommandError`
  `{kind, message}`.
- `web/`: Vue 3 + Pinia + Element Plus, con SCSS (sin Tailwind). El workbench
  imita a VS Code y los tokens de estilo están en `web/src/styles/global.scss`.

## Regla principal: toda función es para todos los motores

Una función nueva tiene que funcionar en **todos** los drivers: planes de
ejecución, edición de datos, exportación, autocompletado, cancelación, solo
lectura, etc. Hay dos únicas excepciones:

- **El motor no tiene la capacidad.** Por ejemplo, Redis no tiene planes de
  ejecución.
- **No hay forma razonable de ofrecerla** con el protocolo o cliente
  disponible.

Cada motor puede tener sus particularidades en cómo la implementa, pero no se
entrega una función "solo para SQL Server" o "solo para los SQL".

Para cumplirlo:

1. La función entra por el contrato (`crates/dbine-driver`) con un método por
   defecto que devuelve `Error::Unsupported` y, si la UI lo necesita, una
   capacidad en `Driver` (como `supports_explain`). La UI muestra la función
   solo donde está soportada.
2. Se implementa en **cada** crate de `crates/drivers/`, en la misma tanda de
   trabajo.
3. Los motores que quedan sin la función se listan en
   `docs/soporte-por-motor.md`, con el motivo. "No hubo tiempo" no es un motivo
   válido; queda como pendiente explícito.
4. Se prueba contra servidores reales (contenedores `dbine-test-*`) en los
   motores que tengan imagen de Docker o emulador.

## Convenciones

- Textos de UI y documentación en español; código, comentarios y commits en
  inglés.
- Commits con prefijo `feat:`, `fix:` o `perf:`.
- Marca: "AddLayer".
- Los tipos Rust se copian a mano en `web/src/api/types.ts`, con los campos en
  snake_case.
- Las contraseñas y los campos secretos nunca van al archivo de estado ni a
  los logs.

## Changelog

`CHANGELOG.md` cuenta, para quien usa DBine, qué trae cada versión: es lo que
muestra el aviso de actualización y las notas del release. Está en **inglés**
y es la fuente. Cada cambio que el usuario nota suma una línea en
`## [Unreleased]`, en el mismo commit o en uno propio, con el formato del
agente `changelog` (`.claude/agents/changelog.md`).

Las traducciones están en `changelog/CHANGELOG.{es,pt,fr,it}.md`: las mismas
versiones publicadas, sin sección `Unreleased`. Al preparar un release,
`python3 scripts/changelog.py release <versión>` convierte `Unreleased` en la
versión nueva en el archivo en inglés; después el agente `translator` agrega
esa versión a cada traducción y `python3 scripts/changelog.py check` tiene que
pasar (falla si a una traducción le falta una versión). El workflow de
release falla si el tag no tiene su sección. La app muestra las notas en el
idioma de la interfaz leyendo la traducción del tag, y en inglés si no puede.

## Archivos compartidos

Para modificar estos archivos hay que coordinar con las otras sesiones:
avisar y esperar confirmación.

- `Cargo.toml` (raíz), `Cargo.lock`, `crates/dbine-drivers/Cargo.toml` y `crates/dbine-drivers/src/lib.rs`
- `crates/dbine-driver/src/*` (el contrato)
- `src-tauri/src/lib.rs`, `src-tauri/capabilities/`, `src-tauri/tauri.conf.json`
- `web/package.json`, `web/src/main.ts`, `web/src/App.vue`, `web/src/api/*`
- `CHANGELOG.md` (cada sesión agrega sus líneas, en inglés, a `Unreleased`;
  releer el archivo justo antes de editarlo) y `changelog/*` (solo al
  publicar, lo escribe el agente `translator`)

Cada driver es dueño exclusivo de su carpeta `crates/drivers/<motor>/`.

## Git y Docker con sesiones en paralelo

- Nunca usar `git checkout -- <ruta>`, `git reset --hard`, `git clean` ni `git stash`.
- Contenedores de prueba: solo `dbine-test-*`. Los demás contenedores son de
  otros proyectos y no se tocan.

## Vista previa de componentes (solo desarrollo)

`web/dev-preview.html?view=plan|chart|results|export` monta componentes con datos de ejemplo
(`web/src/preview/samples.ts`) en un navegador común, sin Tauri ni base de
datos. Sirve para verlos y sacar capturas con Chromium headless mientras la
ventana de la app no está visible. No forma parte del build: `vite build`
solo empaqueta `index.html`.

## Comandos

- Dev: `npm install --prefix web && cargo tauri dev`
- Tests: `cargo test --workspace`
- Build: `cargo tauri build`
