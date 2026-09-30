# DBine

**DBine** es un gestor de bases de datos multimotor de escritorio, de
AddLayer. Con una sola app se trabaja con bases relacionales, analíticas, de
documentos, clave-valor, grafos, series de tiempo y búsqueda. Corre en
Windows, macOS y Linux.

Está hecho con **Tauri 2**, un core en **Rust** y una UI en **Vue 3 + Element
Plus**. La interfaz es un workbench al estilo VS Code. Cada motor es un crate propio y las queries se organizan dentro
de cada base.

## Qué hace

### Explorador y conexiones

- **Árbol por conexión y por base.** El primer nodo de cada base es
  **Queries**, con las queries guardadas de esa base. Así no se acumulan
  pestañas sueltas. Debajo vienen los objetos propios del motor: tablas,
  vistas, rutinas, colecciones, índices, keys, nodos…
- **Caché del explorador.** Al abrir una conexión, las bases, los objetos y las
  columnas aparecen al instante con lo de la última vez, y el servidor los
  actualiza (gana siempre el servidor). Solo guarda nombres y estructura.
  Detalle: [`docs/cache-del-explorador.md`](docs/cache-del-explorador.md).
- **Carpetas y grupos**, por ejemplo por cliente o por ambiente. Se pueden
  anidar y tener color, y los servidores se mueven entre ellas arrastrándolos.
- **Importar conexiones** de DBeaver, DbGate, DataGrip / JetBrains, Azure Data
  Studio y SSMS, o pegando URLs de conexión. DBine lee los archivos de esas
  herramientas, muestra lo que encontró y guarda las que elijas. Las
  contraseñas pasan directo al llavero del sistema, sin pasar por la interfaz.
  Desde DBeaver y DbGate el túnel SSH viene con ellas.
- **Túneles SSH:** cualquier conexión a un motor de red puede pasar por un
  servidor SSH, e incluso por una cadena de bastiones, con contraseña, clave
  privada o el agente SSH. La huella de cada servidor se verifica contra
  `known_hosts` o se confirma la primera vez. La contraseña y la frase de la
  clave nunca van al archivo de estado ni a los logs. Todo lo que se abre
  sobre una conexión comparte un solo túnel, que se vuelve a abrir solo si se
  cae. Detalle: [`docs/tuneles-ssh.md`](docs/tuneles-ssh.md).
- **Menú contextual de la base:**
  - nueva query, nueva tabla con diseñador, nuevos objetos desde plantillas;
  - diagrama ER, generar script, exportar o importar la base, ejecutar un
    archivo de script;
  - comparar esquemas, migrar, clonar o sincronizar, y Profiler;
  - crear o eliminar la base, copiar su nombre o el del servidor.
- **Conexiones de solo lectura**: DBine bloquea todo lo que no sea lectura.

### Monitor y Profiler

- **Monitor del servidor**, con clic derecho sobre la conexión: CPU, memoria,
  sesiones y actividad, en los motores que lo exponen.
- **Profiler**, con clic derecho sobre una base: una pestaña con todas las
  consultas que cualquier cliente ejecuta sobre esa base, en vivo, como el
  Profiler de SQL Server. De cada una muestra la hora, la duración, el texto,
  la base, el usuario, el cliente, las filas y el error, con filtros y la
  opción de abrirla en una query.
  - Si el motor registra cada consulta (Extended Events en SQL Server,
    `system.profile` en MongoDB, historiales o logs de consultas), DBine lee
    lo nuevo cada segundo. Si solo muestra lo que se está ejecutando, lo
    muestrea cada 100 ms, y la pestaña avisa que las consultas más cortas
    pueden no aparecer.
  - Si el motor necesita activar algo para capturar, DBine lo activa al
    iniciar, lo muestra en amarillo y lo restaura al detener o al cerrar. En
    conexiones de solo lectura no cambia nada en el servidor.
  - **CPU, lecturas y escrituras** de cada consulta, en los motores que
    informan esas cifras y en su propia unidad (páginas, bloques, filas,
    bytes, documentos o claves). Al agrupar las consultas iguales se ven el
    promedio, el mínimo, el p95 y el máximo, y también sirven para filtrar.
  - Detalle por motor: [`docs/soporte-por-motor.md`](docs/soporte-por-motor.md#profiler).

### Editor y resultados

- **Queries guardadas.** Se guardan solas mientras escribís. Cada pestaña
  tiene su propia sesión, así que los `SET`, las tablas temporales y las
  transacciones se mantienen entre ejecuciones.
- **Pestañas de vista previa**, como en VS Code. Doble clic en una pestaña
  muestra su query u objeto en el árbol, y "Ir a la base" abre el menú de esa
  base.
- **Editor CodeMirror 6**, con el dialecto de cada motor y autocompletado de
  tablas y columnas. Ejecuta la selección o la sentencia bajo el cursor, y se
  puede cancelar con el mecanismo nativo de cada motor.
- **Edición de celdas:** al modificar una celda se genera el código de
  actualización en el lenguaje del motor. DBine no lo ejecuta: lo agrega a la
  query y vos decidís.
- **Grilla virtualizada** con varios resultados por ejecución, mensajes y visor
  de celdas (con JSON formateado).
- **Filtros por columna** en los datos de una tabla (valores, rangos, nulos,
  texto). Se aplican en el servidor, con el filtro propio de cada motor.
- **Copiar resultados** en 10 formatos: con o sin encabezados, CSV, JSON, JSON
  Lines, YAML, INSERT, UPDATE, inserts de Mongo… El formato por defecto de ⌘C
  es configurable.
- **Exportar resultados** a CSV (con `,`, con `;` o para Excel), TSV, JSON,
  JSONL, SQL, XLSX y XML. La exportación vuelve a correr la query completa en
  streaming, así que no se limita a las filas en pantalla.
- **Gráficos** de resultados con Apache ECharts.
- **Planes de ejecución gráficos**, al estilo de Management Studio: estimado,
  real o los dos, con zoom y desplazamiento.

### Diseño y estructura

- **Diseñador de tablas** adaptado a cada motor. En Mongo diseña colecciones,
  con validación; en cada motor usa sus tipos, identidades, índices y claves.
- **Diagrama ER** de la base:
  - con relaciones, búsqueda y filtro por uno o varios esquemas;
  - con modo "solo claves", minimapa y exportación a SVG o PNG;
  - con distribución automática que aguanta cientos de tablas.
- **Generador de script** de la base, eligiendo qué incluir: DROP, CREATE,
  índices, claves foráneas, vistas y rutinas, datos y triggers. Sale en el
  orden correcto para restaurar, con los ajustes de cada motor (por ejemplo,
  `IDENTITY_INSERT` en SQL Server o volver a sincronizar las secuencias en
  PostgreSQL).
- **Migrar, clonar y sincronizar bases:** "Migrar…" en el menú de una base
  pasa sus tablas y sus datos a otra base, aunque sea de otro motor. Solo se
  habilitan los modos que sirven para ese par de motores:
  - **Migrar (convertir),** entre cualquier par de motores: convierte tipos,
    valores por defecto, claves, índices y nombres, muestra un reporte de cada
    cambio y copia los datos.
  - **Clonar,** entre bases del mismo motor: deja el destino idéntico al
    origen, con esquemas, tipos, particiones, índices, restricciones,
    secuencias, vistas, rutinas y triggers. En SQL Server y Azure SQL, y en
    PostgreSQL, TimescaleDB, KingbaseES, AlloyDB, Cloud SQL, Aurora, EDB y
    Fujitsu.
  - **Sincronizar,** sobre tablas que ya existen en el destino: compara por
    clave e inserta, actualiza y borra solo las filas distintas, en una
    transacción por tabla. En SQL Server y Azure SQL, y en PostgreSQL,
    TimescaleDB, YugabyteDB, KingbaseES, AlloyDB, Cloud SQL, Aurora, EDB y
    Fujitsu, con PostgreSQL 11 o posterior.
  - La copia usa la carga masiva nativa de cada motor (INSERT BULK, COPY
    binario, LOAD DATA, el Appender de DuckDB…) o INSERT por lotes donde no
    la hay. Mueve varias tablas a la vez con memoria acotada y muestra el
    progreso y las filas por segundo de cada una. El origen se abre en solo
    lectura y los datos llegan sin pérdida.
  - Si la app se cierra a mitad de camino, la corrida se retoma después sin
    volver a copiar las tablas ya terminadas.
  - **Migraciones guardadas:** cada base tiene un nodo "Migraciones" con las
    que se iniciaron desde ella. La configuración se guarda sola y viaja con
    la sincronización en la nube, y cada ejecución queda en su historial para
    reabrirla, retomarla o repetirla.

  Detalle: [`docs/migracion.md`](docs/migracion.md).
- **Comparar esquemas:** "Comparar esquemas…" en el menú de una base muestra
  dos bases lado a lado, al estilo WinMerge. Pueden ser de distintas
  conexiones, e incluso de distintos motores.
  - Compara tablas, vistas, procedimientos, funciones y triggers. Resalta lo
    que difiere en columnas, índices, claves foráneas y clave primaria, y las
    líneas distintas del código.
  - Con las flechas `→` y `←` se pasan los cambios de un lado al otro, por
    objeto o por columna, con deshacer.
  - "Sincronizar" genera el script del motor de ese lado (`CREATE`, `ALTER`,
    `DROP`) en el orden correcto, y avisa si algo puede perder datos o fallar.
    Se abre como query o se ejecuta con confirmación.

  Detalle: [`docs/comparacion-de-esquemas.md`](docs/comparacion-de-esquemas.md).
- **Comparar datos:** compara las filas de dos tablas o colecciones, de la
  misma conexión, de conexiones distintas o de motores distintos. Las empareja
  por la clave primaria, o por las columnas que elijas, y compara los valores
  por lo que valen y no por cómo los devuelve cada motor.
  - Muestra las filas distintas, con cada valor marcado, y las que están solo
    de un lado. En cada fila elegís hacia qué lado va: actualizar, insertar o
    borrar. Los borrados nunca se eligen solos.
  - Genera un script por cada lado que cambia, en el lenguaje de su motor,
    que se copia, se abre en una query o se ejecuta. Después vuelve a comparar.
  - Avisa si las dos tablas no son la misma, si hay claves repetidas o si una
    pasa de 200.000 filas. Las conexiones de solo lectura rechazan el script.
  - Compara en todos los motores. Aplicar los cambios depende de lo que el
    destino permite escribir: Drill, por ejemplo, no tiene INSERT, UPDATE ni
    DELETE.

  Detalle: [`docs/comparacion-de-datos.md`](docs/comparacion-de-datos.md).
- **Clonar tabla:** desde el explorador, una tabla, colección o índice se
  clona al lado de la original, con un nombre con fecha y hora que se puede
  cambiar.
  - Copia columnas, clave primaria, restricciones, índices y claves foráneas,
    y los datos con el motor de transferencia, conservando las identidades.
    También se puede clonar solo la estructura.
  - El clon queda exacto o no se crea: si algo no se puede copiar igual, o si
    lo detenés, se borra lo creado. La tabla original nunca se toca.
  - Está en todos los motores con tablas o colecciones con filas propias. No
    en grafos, clave-valor, ksqlDB, CouchDB ni InfluxDB 2 y 3. Detalle:
    [`docs/soporte-por-motor.md`](docs/soporte-por-motor.md#clonar-tabla).
- **Importar** CSV, TSV, JSON, JSONL, XLSX y XML a una tabla nueva o existente,
  con mapeo de columnas.
- **Ejecutar un archivo de script** grande en partes, por ejemplo para
  restaurar un volcado.

### Administración

- **Usuarios y permisos:** una pestaña con los usuarios y roles del servidor
  (o de la base, donde son por base). De cada uno muestra sus roles y sus
  permisos, directos o heredados de un rol, incluidos los denegados.
  - Se crean usuarios y roles, se cambian contraseñas, se habilita o
    deshabilita el ingreso y se otorgan o revocan permisos sobre la base, un
    esquema o un objeto.
  - Cada cambio se convierte en un script del motor que se revisa antes de
    ejecutarlo. La contraseña no aparece en la vista previa y estos scripts no
    quedan en el historial.
  - Está en la gran mayoría de los motores. No la tienen los que no manejan
    usuarios propios, como SQLite, DuckDB o DynamoDB. Detalle:
    [`docs/usuarios-y-permisos.md`](docs/usuarios-y-permisos.md).
- **Acciones según los permisos del usuario:** antes de ofrecer backups,
  restauraciones, el Profiler, terminar sesiones, crear o borrar bases o
  administrar usuarios, DBine le pregunta al servidor si el usuario conectado
  puede hacerlo. Si no puede, la acción aparece deshabilitada y dice qué
  permiso falta. Si el motor no permite saberlo con certeza, queda habilitada.
- **Backups**, en una pestaña por base:
  - copias de DBine en todos los motores: un script local con la estructura
    y, si querés, los datos, con su historial en esta máquina, que se
    restaura en la misma base o en otra;
  - backups del servidor donde el motor los tiene (SQL Server, Oracle, SAP
    HANA, ClickHouse, Snowflake, BigQuery, Elasticsearch, Redis, entre otros),
    con su historial y el script para hacer, restaurar o borrar un backup, que
    se revisa antes de ejecutarlo.

  Detalle: [`docs/backups.md`](docs/backups.md).

### Biblioteca de scripts

Los scripts reutilizables de un DBA ("Reindexar una tabla", "Sesiones
bloqueantes"…) se guardan en su propia vista, con la ⭐:

- **Por motor, no por base:** cada script es de uno o más motores, así un
  script de MongoDB no se mezcla con uno de SQL Server.
- **Al abrirlo,** se copia a una query de la base activa y pide sus
  `{{parámetros}}`, con sugerencias de las tablas de esa base.
- **Organización:** carpetas y búsqueda.
- **Importar y exportar:** trae de una vez la carpeta de `.sql` que ya tenés, y
  la exporta igual.

Detalle: [`docs/biblioteca.md`](docs/biblioteca.md).

### Sincronización en la nube

Un backup de las conexiones, carpetas, queries, preferencias y contraseñas.
Se guarda **cifrado de punta a punta** con una frase clave del usuario
(XChaCha20-Poly1305 + Argon2id) y queda en **su propia cuenta**:

- Google Drive, en la carpeta privada de la app;
- OneDrive, en la carpeta de la app;
- o una carpeta cualquiera, como iCloud o Dropbox.

Sincroniza sola, resuelve conflictos sin perder datos y permite recuperar
todo en otra máquina. AddLayer no tiene servidores para esto y nunca ve los
datos. Detalle: [`docs/sincronizacion.md`](docs/sincronizacion.md).

### Asistente de IA

Un chat en la barra lateral derecha (⌘I). Escribe queries, explica o corrige
la del editor y responde sobre la estructura de la base.

- **Nunca ejecuta nada.** Su código se agrega a la query abierta, o la
  reemplaza cuando la corrige, y el usuario decide si lo ejecuta.
- **Usa lo que hay en la máquina:**
  - el **modelo integrado** (llama.cpp con Qwen2.5-Coder 3B o 7B), que corre
    local y se descarga una sola vez, la primera vez que se usa;
  - Ollama o LM Studio;
  - Claude Code o Codex con la cuenta del usuario, sin herramientas.
- **Nunca manda filas de datos.** Como contexto recibe el motor, la estructura
  y el editor.

Detalle: [`docs/asistente-ia.md`](docs/asistente-ia.md).

### Servidor MCP

DBine puede funcionar como servidor MCP local, así asistentes como Claude
Code, Codex, Cursor, Claude Desktop, VS Code o Windsurf trabajan con tus
conexiones mientras DBine está abierto. Viene apagado.

- **Cuatro niveles por conexión:** Deshabilitado (el asistente no la ve),
  Esquema (solo la estructura, sin datos), Lectura (además filas de muestra,
  consultas de solo lectura y planes estimados) y Escritura. El nivel
  predeterminado es Esquema, y las conexiones con la etiqueta `prod` o de solo
  lectura nunca pasan de Lectura.
- **Cada escritura se aprueba:** DBine pasa al frente y muestra el cliente, la
  conexión, la base y el código exacto para aprobarlo o rechazarlo. Si no
  respondés en 2 minutos, se rechaza y no se ejecuta nada.
- **Un token por cliente,** revocable por separado. DBine escucha solo en esta
  máquina y rechaza los pedidos que vienen de páginas del navegador.
- **Registro de actividad** local con las últimas 10.000 llamadas, filtrable
  por cliente o por conexión. No guarda contraseñas ni secretos.

Detalle: [`docs/mcp.md`](docs/mcp.md).

### Seguridad

- Las contraseñas y demás secretos se guardan **cifrados** (XChaCha20-Poly1305)
  en un archivo aparte, con una llave aleatoria que vive en el **llavero del
  sistema** (Keychain, Credential Manager, Secret Service). Es un solo ítem
  del llavero para toda la app: el sistema pide permiso una vez, no una por
  conexión. Nunca van al archivo de estado ni a los logs.
- El estado local es un SQLite en la carpeta de configuración de la app.

## Motores

Cada motor es un crate en `crates/drivers/`. Toda función nueva tiene que
funcionar en todos. Las excepciones, con su motivo, están en
[`docs/soporte-por-motor.md`](docs/soporte-por-motor.md).

| Familia | Motores |
|---|---|
| Relacionales | SQL Server · PostgreSQL · CockroachDB · YugabyteDB · TimescaleDB · Greenplum · KingbaseES · Denodo · MySQL · MariaDB · TiDB · OceanBase · SingleStore · SQLite · libSQL / Turso · Oracle · Oracle Autonomous · Firebird · SAP HANA · Aurora DSQL · Cloud Spanner · ODBC (Db2, Sybase ASE, SQL Anywhere, Informix y más) |
| Analíticas | DuckDB · archivos CSV / Parquet / JSON · ClickHouse · Trino · Presto · Starburst · BigQuery · Athena · Snowflake · Databricks · Redshift · StarRocks · Doris · Databend · Hive · Impala · Drill · Dremio · Calcite Avatica · Flight SQL · Teradata · Vertica · Exasol · Netezza · Ocient |
| Documentos | MongoDB · CouchDB · Couchbase · Azure Cosmos DB |
| Clave-valor | Redis · Valkey · Dragonfly · DynamoDB · etcd |
| Grafos | Neo4j · Memgraph · Amazon Neptune · OrientDB |
| Columnares anchas | Cassandra · ScyllaDB · Amazon Keyspaces · Phoenix |
| Series de tiempo | InfluxDB 1/2/3 · IoTDB · GreptimeDB · TDengine |
| Búsqueda | Elasticsearch · OpenSearch · Solr · Manticore |
| Streaming | ksqlDB · Timeplus |

Ningún driver necesita librerías del fabricante para que la app arranque:

- Oracle usa un cliente en Rust puro.
- ODBC carga el driver manager recién al usarlo.
- Los presets ODBC sí necesitan el driver del fabricante instalado para
  conectarse.
- DuckDB no viene dentro de la app. La primera vez que te conectás, DBine
  descarga la librería oficial, con la versión fijada y verificada por
  sha256, y el explorador muestra el avance ("Descargando DuckDB…"). Lo mismo
  pasa con el motor del asistente de IA. Así el ejecutable pesa bastante
  menos.

## Apoyar el proyecto

DBine es gratis y lo va a seguir siendo. Si te sirve, podés apoyarlo con lo
que quieras, una vez o todos los meses, desde
[GitHub Sponsors](https://github.com/sponsors/addlayer-io). Es un apoyo, no
una licencia: la app funciona igual.

## Descargar

Los instaladores están en
[Releases](../../releases):

- **Windows:** `.msi` o `-setup.exe`.
- **macOS:** `.dmg`, en versión Apple Silicon o Intel.
- **Linux:** `.AppImage`, `.deb` o `.rpm`.

Los binarios todavía no están firmados por Apple ni por Microsoft:

- **macOS:** la primera vez avisa que no puede verificar al desarrollador.
  Abrí Configuración del Sistema › Privacidad y seguridad y tocá «Abrir
  igualmente». También se puede desde la terminal:
  `xattr -dr com.apple.quarantine /Applications/DBine.app`.
- **Windows:** SmartScreen puede pedir "Más información" › "Ejecutar de todas
  formas".

## Compilar

### Requisitos

- **Rust** estable y **Node.js** 20 o superior.
- Un compilador de C (Xcode Command Line Tools en macOS, Visual Studio Build
  Tools en Windows, `build-essential` en Linux). llama.cpp y DuckDB ya no se
  compilan: se descargan la primera vez que se usan.
- **Solo en Linux:**
  `sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libssl-dev libxdo-dev`.

```bash
npm install --prefix web
cargo install tauri-cli --version "^2"
```

### Desarrollo

```bash
cargo tauri dev          # la app con recarga en caliente
cargo test --workspace   # tests unitarios
```

**macOS: que el llavero no pida permiso en cada recompilación.** Las
compilaciones de desarrollo se firman "ad hoc" y cambian de identidad en cada
build, así que macOS vuelve a preguntar por cada contraseña guardada. Con un
certificado de firma de código llamado `DBine Dev` en el llavero de inicio de
sesión (autofirmado alcanza), `cargo run` / `cargo tauri dev` firman la app
con una identidad fija (`scripts/dev-sign-run.sh`, configurado en
`.cargo/config.toml`). Después del primer "Permitir siempre", no vuelve a
preguntar. Para crear el certificado, una sola vez por máquina:

```bash
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 3650 -nodes -subj "/CN=DBine Dev" \
  -addext "keyUsage=critical,digitalSignature" -addext "extendedKeyUsage=critical,codeSigning" -addext "basicConstraints=critical,CA:false"
openssl pkcs12 -export -inkey key.pem -in cert.pem -name "DBine Dev" -out dev.p12 -passout pass:dbine-dev
security import dev.p12 -k ~/Library/Keychains/login.keychain-db -P dbine-dev -T /usr/bin/codesign
rm key.pem cert.pem dev.p12
```

Sin el certificado, la app corre igual, solo que sin firma fija.

### Instaladores

Cada sistema compila sus propios instaladores:

| Sistema | Comando | Resultado en `target/<target>/release/bundle/` |
|---|---|---|
| macOS Apple Silicon | `cargo tauri build --target aarch64-apple-darwin` | `.app`, `.dmg` |
| macOS Intel | `cargo tauri build --target x86_64-apple-darwin` | `.app`, `.dmg` |
| Windows | `cargo tauri build --target x86_64-pc-windows-msvc` | `.msi`, `-setup.exe` |
| Linux | `cargo tauri build --target x86_64-unknown-linux-gnu` | `.AppImage`, `.deb`, `.rpm` |

Sin `--target`, compila para la máquina actual y deja los archivos en
`target/release/bundle/`.

### Windows desde macOS (el `.exe`, sin instalador)

```bash
brew install llvm
cargo install cargo-xwin
rustup target add x86_64-pc-windows-msvc

./scripts/build-windows-from-mac.sh
# → target/x86_64-pc-windows-msvc/release/dbine.exe
```

El script corre
`cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle`
con dos ajustes para compilar el C de las dependencias desde la Mac:

- usa el `clang-cl` de LLVM;
- apunta el `lld-link` de cargo-xwin al `rust-lld` de rustup.

Acepta la licencia del SDK de Windows (`XWIN_ACCEPT_LICENSE=1`). Los
instaladores `.msi` y `.exe` se arman en Windows o en CI.


### Cuentas de nube

Para que el login de Google Drive y OneDrive funcione, se compilan los IDs de
la app registrada:

```bash
DBINE_GOOGLE_CLIENT_ID=… DBINE_GOOGLE_CLIENT_SECRET=… DBINE_MICROSOFT_CLIENT_ID=… cargo tauri build
```

En local también pueden ir en un `.env` en la raíz del repo, que git ignora.

Cómo registrarla: [`docs/sincronizacion.md`](docs/sincronizacion.md).

## Publicar una versión

`.github/workflows/release.yml` compila las cuatro variantes en GitHub
Actions: Windows x64, macOS Apple Silicon, macOS Intel y Linux x64. Después
publica los instaladores en un release.

```bash
git tag v0.2.0
git push origin v0.2.0
```

- La versión de la app se toma del tag.
- Si existen los secretos `DBINE_GOOGLE_CLIENT_ID`, `DBINE_GOOGLE_CLIENT_SECRET`
  y `DBINE_MICROSOFT_CLIENT_ID`, el build los incluye.
- **Prueba sin publicar:** Actions › release › Run workflow compila todo y
  deja los instaladores como artefactos.
- El instalador trae solo el driver de SQLite. Los demás se compilan aparte,
  se suben al mismo release y la app los descarga la primera vez que se usan:
  [`docs/drivers-bajo-demanda.md`](docs/drivers-bajo-demanda.md).

## Estructura del repositorio

| Ruta | Qué hay |
|---|---|
| `crates/dbine-driver` | El contrato de los drivers (traits `Driver` y `Session`) y los helpers |
| `crates/drivers/<motor>` | Un crate por motor o familia de protocolo |
| `crates/dbine-drivers` | El registro de drivers, con una feature por crate |
| `crates/dbine-plugin`, `crates/dbine-plugin-host` | Los drivers como procesos aparte, descargados al usarlos |
| `crates/dbine-core` | El estado local (SQLite), el llavero, la exportación y la importación |
| `crates/dbine-schema` | La conversión de esquemas entre motores y la comparación de esquemas |
| `crates/dbine-sync` | La sincronización cifrada (Google Drive, OneDrive, carpeta) |
| `crates/dbine-ai` | El asistente de IA (modelo integrado, Ollama, Claude Code, Codex, LM Studio) |
| `src-tauri` | Los comandos Tauri |
| `web` | La UI en Vue 3 |

Documentación:

- [`AGENTS.md`](AGENTS.md): convenciones y reglas del proyecto.
- [`docs/drivers.md`](docs/drivers.md): cómo agregar un motor.
- [`docs/drivers-bajo-demanda.md`](docs/drivers-bajo-demanda.md): cómo se descargan los drivers.
- [`docs/api-comandos.md`](docs/api-comandos.md): los comandos del backend.
- [`docs/soporte-por-motor.md`](docs/soporte-por-motor.md): qué soporta cada
  motor.
- [`docs/cache-del-explorador.md`](docs/cache-del-explorador.md): la caché del
  árbol del explorador.
- [`docs/comparacion-de-esquemas.md`](docs/comparacion-de-esquemas.md): cómo
  se comparan y sincronizan dos bases.
- [`docs/migracion.md`](docs/migracion.md) y
  [`docs/conversion-de-esquemas.md`](docs/conversion-de-esquemas.md): la
  migración a otro motor y cómo convierte los tipos.
- [`docs/asistente-ia.md`](docs/asistente-ia.md),
  [`docs/biblioteca.md`](docs/biblioteca.md) y
  [`docs/sincronizacion.md`](docs/sincronizacion.md): el asistente de IA, la
  biblioteca de scripts y la sincronización en la nube.

Los tests de integración están en `crates/drivers/*/tests/`, marcados
`#[ignore]`. Leen `DBINE_TEST_<MOTOR>_URL` y se corren contra contenedores de
Docker.
