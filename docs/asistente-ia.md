# Asistente de IA

Un chat en la barra lateral derecha. Se abre con el ícono de la varita en la
barra de actividad o con ⌘I. Sirve para:

- escribir queries;
- explicar o corregir la query del editor;
- entender la estructura de la base.

## Regla principal: el asistente nunca ejecuta

El asistente **solo escribe código**. La ejecución siempre queda en manos del
usuario:

- No existe ningún camino técnico para que ejecute. El backend de IA no tiene
  acceso a las sesiones de base ni al comando de ejecución, y Claude Code y
  Codex corren sin herramientas.
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

## Proveedores

DBine no tiene un servicio de IA propio. Usa lo que hay en la máquina, en este
orden:

| Proveedor | Qué es | Privacidad |
|---|---|---|
| **Integrado en DBine** | llama.cpp dentro de la app (Metal en Mac) con Qwen2.5-Coder 3B o 7B, que se descargan una vez | Local: nada sale de la máquina |
| **Ollama** | servidor local (`localhost:11434` u `OLLAMA_HOST`) | Local |
| **Claude Code** | el CLI `claude` con la cuenta del usuario | La pregunta, la estructura y el editor van a Anthropic |
| **Codex** | el CLI `codex` con la cuenta del usuario | Ídem, a OpenAI |
| **LM Studio** | servidor local compatible con OpenAI (`localhost:1234`) | Local |

**Detección:**
- Una app abierta desde el Finder no hereda el PATH de la terminal. Por eso se
  lee una vez el PATH del shell de login y se le suman las carpetas habituales
  (Homebrew, npm global, mise, asdf, nvm…).
- Si Ollama está instalado pero cerrado, se ofrece abrirlo.
- Si Ollama está abierto pero sin modelos, se ofrece bajar `qwen2.5-coder:7b`.

**Sin nada instalado:** se recomienda descargar el modelo integrado. Se marca
como recomendado el más grande que la RAM de la máquina aguanta cómodo: el 7B
desde 16 GB.

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
- **Estructura de la base** (se puede desactivar con la casilla "estructura"):
  - Viene de `database_schema`, así que funciona con todos los drivers que la
    implementan. Se guarda en caché 10 minutos.
  - Formato compacto: `schema.tabla(col tipo PK, col tipo NOT NULL, …)` y sus
    FK.
  - Si no entra entera, van primero las tablas nombradas en la pregunta, en el
    editor o en la pestaña, y del resto solo el nombre.
  - Tope: unos 24 000 caracteres para modelos locales y 150 000 para Claude
    Code y Codex.
- **Editor:** el texto de la query abierta, la selección y el error de la
  última ejecución.
- **Nunca se envían filas de datos.**

Debajo de cada pregunta se muestra qué contexto viajó; por ejemplo,
"SQLite · 2 tablas · editor".

## Preferencias

Viajan con la sincronización:

- `ai.provider`
- `ai.model.<proveedor>`
- `ai.includeSchema`

La conversación queda en la máquina (las últimas 60 entradas); el botón
"Nueva conversación" la borra.

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
