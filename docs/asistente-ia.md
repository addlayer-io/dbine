# Asistente de IA

Un chat en la barra lateral derecha. Se abre con el ícono de la varita en la
barra de actividad o con ⌘I. Sirve para:

- escribir queries;
- explicar o corregir la query del editor;
- entender la estructura de la base.

## Regla principal: el asistente nunca escribe

El asistente **nunca cambia la base**: lo que modifica datos o estructura lo
escribe como código y la ejecución queda en manos del usuario.

- No existe ningún camino técnico para que escriba. Con un modelo local puede
  **leer** la conexión de la pestaña (ver «Consultar toda la conexión»), y solo
  por las herramientas de lectura del servidor MCP, que abren sesiones de solo
  lectura y rechazan cualquier escritura. Claude Code y Codex corren sin
  herramientas.
- Si el usuario le pide "borrá la tabla X", el asistente escribe el
  `DROP TABLE` y nada más.
- Cada bloque de código tiene tres acciones:
  - **Agregar a la query**: la acción principal. Agrega el código al final de
    la query abierta y lo deja seleccionado. Si no hay una query abierta, abre
    una nueva en la base de la pestaña activa.
  - **Reemplazar**: cambia el contenido del editor por el código, por ejemplo
    cuando se le pidió corregir la query. ⌘Z lo deshace.
  - **Copiar**.
- Cualquier bloque que modifica datos o estructura (`DROP`, `DELETE`,
  `UPDATE`, `ALTER`, `TRUNCATE`, `INSERT`… y sus equivalentes en Mongo o
  Redis) muestra una advertencia fija de la UI: "No se ejecutó nada: revisalo
  antes de ejecutarlo vos". La advertencia no depende de lo que diga el modelo.
- El prompt también le indica que no ejecuta ni dice haber ejecutado nada, con
  un ejemplo al final, que es lo que mejor respetan los modelos chicos.

## Consultar toda la conexión (modelos locales)

Con un modelo local (integrado, Ollama o LM Studio), el asistente no se limita
a la base de la pestaña: puede leer **todas las bases de esa conexión** para
armar una query que las cruce o para analizarlas ("¿qué índices no se usan en
ninguna base?"). No recibe todo de entrada (no entraría en el contexto): lo
pide a medida que lo necesita.

- **Cómo pide:** el prompt le explica un formato propio de DBine. El modelo
  responde solo con `<herramienta>{"name": …, "arguments": {…}}</herramienta>`,
  DBine lo atiende y le devuelve `<resultado>…</resultado>` (o `<error>`), y
  el modelo sigue. Se usa ese formato y no las herramientas nativas de cada
  modelo porque en la prueba Qwen2.5-Coder con llama.cpp no generó llamadas
  nativas; el formato propio funciona igual con los tres proveedores locales.
- **Qué puede leer sin preguntar:** las bases de la conexión, los objetos de
  una base, la estructura de una tabla (`describe_object`) y el uso de sus
  índices (`index_usage`). Son lecturas del catálogo que hace DBine, no SQL
  del modelo.
- **Qué lee solo con tu aprobación:** filas de muestra y consultas de **solo
  lectura** (`sample_rows`, `run_query`, `explain`). Antes de correr cada una,
  el chat muestra una tarjeta con el modelo (por ejemplo "Qwen2.5-Coder 32B,
  local"), la conexión › base y la consulta exacta, con **Aprobar**,
  **Rechazar** y **Aprobar lecturas en esta conversación**. Si la rechazás, el
  modelo recibe un `<error>` y sigue sin esos datos. "Aprobar lecturas en esta
  conversación" deja de valer al empezar otra conversación o al cambiar de
  conexión o de base. Aprobar no habilita escrituras: la consulta corre en una
  sesión de solo lectura que rechaza cualquier cambio.
- **Con qué código:** las mismas herramientas del [servidor MCP](mcp.md)
  (`assistant_call` en `src-tauri/src/mcp/tools.rs`): sesiones de solo lectura
  propias, rechazo de escrituras, topes de filas y de tiempo. No depende de la
  configuración de MCP, y vale solo sobre la conexión de la pestaña (no ve
  otras conexiones). Cada lectura queda en la actividad de MCP
  como "Asistente de DBine".
- **Límites:** hasta 12 lecturas por respuesta; una consulta repetida no se
  vuelve a ejecutar. Cada resultado se recorta a 12 000 caracteres.
- **En el chat:** mientras lee se ve "Consultando la conexión: estructura de
  people.customer en tenant-compras…", y debajo de la respuesta, plegada, la
  lista de lo que consultó. Las líneas `<herramienta>` no se muestran.
- **Modelos:** está disponible con cualquiera. Con el pedido "analizá los
  índices de las bases y decime cuál sobra", el 32B y el 7B listaron las bases,
  leyeron el uso de índices de cada una y respondieron bien; el 3B usó las
  herramientas pero se fue por las ramas (planes de consultas inventadas).
- **Cómo reconoce el pedido:** además de `<herramienta>`, acepta `<tool_call>`,
  un bloque ```` ```json ```` o un objeto `{"name", "arguments"}` suelto después
  de texto, que es lo que escriben a veces los modelos chicos; ese texto no se
  muestra en el chat. `index_usage` sin `object` resume toda la base (hasta
  400 tablas o 2 minutos).
- **Claude Code y Codex:** no tienen estas herramientas: la estructura de las
  otras bases saldría de la máquina. Si se habilitan más adelante, será solo
  con metadatos, y con datos únicamente si el usuario lo aprueba.

## Proveedores

DBine no tiene un servicio de IA propio. Usa lo que hay en la máquina, en este
orden:

| Proveedor | Qué es | Privacidad |
|---|---|---|
| **Integrado en DBine** | llama.cpp dentro de la app (Metal en Mac) con Qwen2.5-Coder 3B, 7B o 32B, que se descargan una vez | Local: nada sale de la máquina |
| **Ollama** | servidor local (`localhost:11434` u `OLLAMA_HOST`) | Local |
| **Claude Code** | el CLI `claude` con la cuenta del usuario | La pregunta, la estructura y el editor van a Anthropic |
| **Codex** | el CLI `codex` con la cuenta del usuario | Ídem, a OpenAI |
| **LM Studio** | servidor local compatible con OpenAI (`localhost:1234`) | Local |

**Detección:**
- Una app abierta desde el Finder no hereda el PATH de la terminal. Por eso se
  lee una vez el PATH del shell de login y se le suman las carpetas habituales
  (Homebrew, npm global, mise, asdf, nvm…).
- Si Ollama está instalado pero cerrado, se ofrece abrirlo.
- Si Ollama está abierto pero sin modelos, se ofrece bajar `qwen2.5-coder:7b`,
  o `qwen2.5-coder:32b` desde 48 GB de RAM.

**Sin nada instalado:** se recomienda descargar el modelo integrado. Se marca
como recomendado el más grande que la RAM de la máquina aguanta cómodo: el 7B
desde 16 GB y el 32B desde 48 GB. Quien ya usa uno más chico que el
recomendado ve un aviso en el chat para descargarlo ("Ahora no" lo oculta
para ese modelo).

**Por qué esos modelos.** En una prueba del 2026-10-03 (MacBook M5 Max),
consultas reales de SQL Server entre bases (`[base].esquema.tabla`, con
corchetes por los guiones) solo salieron bien con el 32B: el 3B, el 7B y el
14B nombraban mal la base o el esquema, y Qwen3-30B-A3B gastaba la respuesta
pensando. El 14B no rindió mejor que el 7B, así que no está en el catálogo.
En la misma prueba, 24 pedidos legítimos que suenan sensibles (contraseñas,
permisos, datos personales, borrados) no tuvieron rechazos con ningún modelo.

### Claude Code y Codex, sin herramientas

Cada respuesta es un proceso nuevo que corre en una carpeta temporal vacía.

- **Claude Code:** `claude -p --output-format stream-json --include-partial-messages --tools "" --strict-mcp-config --setting-sources "" --no-session-persistence --system-prompt-file …`.
  No tiene herramientas ni servidores MCP, y no carga la configuración del
  usuario, así que no corren sus hooks.
- **Codex:** `codex exec --json --skip-git-repo-check --sandbox read-only -C <carpeta vacía> -`.

### Modelo integrado

- **Nada dentro de la app:** ni el modelo ni llama.cpp vienen en el
  instalador. Quien usa Claude Code, Codex, Ollama o LM Studio no descarga
  nada.
- **Motor:** el build oficial de llama.cpp para la plataforma (release
  `b11213` de github.com/ggml-org/llama.cpp, tamaño y SHA-256 fijos en
  `crates/dbine-ai/src/embedded.rs`): Metal en macOS, CPU en Windows y Linux.
  Pesa entre 11 y 19 MB y se guarda en `models/llama.cpp-<release>/`.
- **Cuándo se descarga:** junto con el primer modelo, en la misma barra de
  progreso. Si el modelo ya estaba en disco (instalaciones anteriores), en el
  primer chat: el mensaje muestra «Descargando el motor de IA, solo esta
  vez… 45 %».
- **Cómo corre:** su `llama-server` como proceso hijo, escuchando solo en
  `127.0.0.1` en un puerto libre y con una clave de API aleatoria por
  arranque. El chat va por su API compatible con OpenAI (el mismo cliente que
  LM Studio), con temperatura 0,2 y hasta 3072 tokens de respuesta. El modelo
  queda cargado entre respuestas; al elegir otro, el servidor se reinicia con
  ese.
- **Modelos:** los GGUF oficiales de Qwen (Q4_K_M) se guardan en `models/`
  dentro de la carpeta de datos de la app. La descarga se puede pausar y
  retomar, y se verifica el SHA-256 antes de usar el archivo.
- **Plantilla de chat:** la aplica `llama-server` a partir del propio archivo
  GGUF; no está escrita a mano.
- **Cierre:** la app detiene el servidor al salir. Si la app se cerró de
  golpe, el siguiente arranque del motor termina el servidor que quedó vivo
  (su PID queda en `server.pid`).
- **Windows:** `llama-server` necesita el runtime de Visual C++ de
  Microsoft. Si falta, el chat lo explica y pide instalar «Microsoft Visual
  C++ Redistributable».
- **Prueba de punta a punta:**
  `DBINE_TEST_MODELS=<carpeta con un modelo> cargo test -p dbine-ai -- --ignored`
  descarga el motor en esa carpeta y chatea con el modelo.
- **Al actualizar llama.cpp:** cambiar `ENGINE_TAG` y los nombres, tamaños y
  SHA-256 de `ENGINE` en `embedded.rs`.

## Contexto que acompaña cada pregunta

Se arma en `src-tauri/src/commands/ai.rs`:

- **Motor y lenguaje:** SQL con su dialecto, CQL, JSON/Mongo, Redis, Flux o
  Cypher. También la base actual y si la conexión es de solo lectura.
- **Con un modelo local:** solo los nombres de las tablas, vistas y rutinas de
  la base de la pestaña (hasta 400, con la cantidad de las que quedan afuera),
  de la lista que ya tiene el explorador. Las columnas, claves e índices los
  pide el modelo con las herramientas del catálogo (ver «Consultar toda la
  conexión»), sin aprobación. No hay casilla para apagarlo: es liviano, sale
  de la caché y llama.cpp reutiliza lo que ya procesó del mismo texto.
- **Con Claude Code o Codex** (sin herramientas), la estructura compacta:
  - Viene de `database_schema`, así que funciona con todos los drivers que la
    implementan. Se guarda en caché 10 minutos.
  - Formato compacto: `schema.tabla(col tipo PK, col tipo NOT NULL, …)` y sus
    FK.
  - Si no entra entera, van primero las tablas nombradas en la pregunta, en el
    editor o en la pestaña, y del resto solo el nombre.
  - Tope: unos 150 000 caracteres.
- **Editor:** el texto de la query abierta, la selección y el error de la
  última ejecución.
- **Pistas del dialecto** (SQL Server, PostgreSQL, MySQL, Oracle, SQLite): cómo
  se citan los nombres, cómo se nombra una tabla de otra base y cómo se limitan
  las filas, que es en lo que más se equivocan los modelos chicos.
- **Nunca se envían filas de datos.**

## Si el modelo se niega

Un modelo chico a veces responde "Lo siento, no puedo ayudarte con eso" a un
pedido legítimo sobre la base del propio usuario. Si la respuesta es corta, no
trae código y se disculpa o dice que no puede (en español, inglés, portugués,
francés o italiano), DBine vuelve a preguntar una sola vez agregando al prompt
que el pedido es legítimo y que el usuario decide qué ejecutar. El chat
muestra "Volviendo a preguntar…" y reemplaza la negativa por la nueva
respuesta.

Debajo de cada pregunta se muestra qué contexto viajó; por ejemplo,
"SQLite · 2 tablas · editor" (con un modelo local, "· 48 objetos").

## Preferencias

Viajan con la sincronización:

- `ai.provider`
- `ai.model.<proveedor>`

La conversación queda en la máquina (las últimas 60 entradas). "Nueva
conversación" no la borra: la pasa al **Historial** (el ícono del reloj en el
encabezado del panel), que guarda las últimas 50 conversaciones con su primera
pregunta como título. Desde ahí se retoma una (la actual pasa al historial) o
se borra una o todas. Todo queda en el almacenamiento local de la app; si se
llena, se descartan primero las más viejas.

## Comandos

| Comando | args | Qué hace |
|---|---|---|
| `ai_detect` | — | Proveedores, catálogo del modelo integrado y recomendado |
| `ai_chat` | `{ chat_id, provider, model, messages, context }` | Responde en streaming (eventos `ai-delta` y `ai-status`) |
| `ai_cancel` | `{ id }` | Detiene un chat o pausa una descarga |
| `ai_download_model` | `{ id }` | Descarga un modelo integrado (evento `ai-download`) |
| `ai_delete_model` | `{ id }` | Borra un modelo integrado |
| `ai_start_ollama` | — | Abre Ollama y espera que responda |
| `ai_pull_ollama` | `{ id }` | `ollama pull` con progreso |

Para probar un proveedor desde la terminal:

```sh
cargo run -p dbine-ai --features embedded --example ask -- embedded qwen2.5-coder-3b "pregunta"
cargo run -p dbine-ai --example ask -- detect
```
