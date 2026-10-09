# AI assistant

A chat in the right sidebar. It opens with the sparkles icon in the activity
bar or with ⌘I. It is useful for:

- writing queries;
- explaining or fixing the query in the editor;
- understanding the database structure.

## Main rule: the assistant never writes

The assistant **never changes the database**: whatever modifies data or
structure it writes as code, and running it is left in the user's hands.

- There is no technical path for it to write. With a local model it can
  **read** the tab's connection (see "Query the whole connection"), and only
  through the read tools of the MCP server, which open read-only sessions and
  reject any write. Claude Code and Codex run without tools.
- If the user asks it "drop table X", the assistant writes the `DROP TABLE`
  and nothing else.
- Each code block has three actions:
  - **Add to query**: the main action. It appends the code to the end of the
    open query and leaves it selected. If there is no open query, it opens a
    new one on the active tab's database.
  - **Replace**: swaps the editor content for the code, for example when it
    was asked to fix the query. ⌘Z undoes it.
  - **Copy**.
- Any block that modifies data or structure (`DROP`, `DELETE`, `UPDATE`,
  `ALTER`, `TRUNCATE`, `INSERT`… and their equivalents in Mongo or Redis)
  shows a fixed UI warning: "Changes data or structure. Nothing was run:
  review it before you run it yourself." The warning does not depend on what
  the model says.
- The prompt also tells it that it does not run anything nor claim to have
  run anything, with an example at the end, which is what small models
  respect best.

## Query the whole connection (local models)

With a local model (built-in, Ollama or LM Studio), the assistant is not
limited to the tab's database: it can read **all the databases of that
connection** to build a query that crosses them or to analyze them ("which
indexes are unused in any database?"). It does not receive everything up
front (it would not fit in the context): it asks for it as it needs it.

- **How it asks:** the prompt explains a DBine-specific format. The model
  answers only with `<herramienta>{"name": …, "arguments": {…}}</herramienta>`,
  DBine serves it and returns `<resultado>…</resultado>` (or `<error>`), and
  the model goes on. This format is used instead of each model's native tools
  because in the test Qwen2.5-Coder with llama.cpp did not generate native
  calls; the own format works the same with the three local providers.
- **What it can read without asking:** the connection's databases, a
  database's objects, a table's structure (`describe_object`) and the usage of
  its indexes (`index_usage`). These are catalog reads that DBine does, not
  SQL from the model.
- **What it reads only with your approval:** sample rows and **read-only**
  queries (`sample_rows`, `run_query`, `explain`). Before running each one,
  the chat shows a card with the model (for example "Qwen2.5-Coder 32B,
  local"), the connection › database and the exact query, with **Approve**,
  **Reject** and **Approve reads in this conversation**. If you reject it, the
  model receives an `<error>` and continues without that data. "Approve reads
  in this conversation" stops applying when another conversation starts or
  when the connection or database changes. Approving does not enable writes:
  where the engine supports it, the query runs as one statement in a
  read-only transaction the server enforces (`run_query`); elsewhere your
  approval in the chat is what lets it run, in a read-only session that
  rejects any change. "Approve reads in this conversation" is off at the
  start of each conversation and only that button turns it on. Which engines
  enforce reads: [engine-support.md](engine-support.md#server-enforced-reads-mcp-and-the-ai-assistant).
- **With what code:** the same tools as the [MCP server](mcp.md)
  (`assistant_call` in `src-tauri/src/mcp/tools.rs`): its own read-only
  sessions, write rejection, row and time limits. It does not depend on the
  MCP configuration, and it applies only to the tab's connection (it does not
  see other connections). Each read is recorded in the MCP activity as
  "DBine assistant", saying whether the server enforced it or it was
  approved in the chat.
- **Limits:** up to 12 reads per answer; a repeated query is not run again.
  Each result is trimmed to 12,000 characters.
- **In the chat:** while it reads you see "Querying the connection: structure
  of ventas.clientes in tenant-compras…", and below the answer, folded, the
  list of what it queried. The `<herramienta>` lines are not shown.
- **Models:** it is available with any. With the request "analyze the indexes
  of the databases and tell me which one is redundant", the 32B and the 7B
  listed the databases, read each one's index usage and answered correctly;
  the 3B used the tools but went off on tangents (plans of invented queries).
- **How it recognizes the request:** besides `<herramienta>`, it accepts
  `<tool_call>`, a ```` ```json ```` block or a loose `{"name", "arguments"}`
  object after text, which is what small models sometimes write; that text is
  not shown in the chat. `index_usage` without `object` summarizes the whole
  database (up to 400 tables or 2 minutes).
- **Claude Code and Codex:** they do not have these tools: the structure of
  the other databases would leave the machine. If they are enabled later, it
  will be with metadata only, and with data only if the user approves.

## Providers

DBine has no AI service of its own. It uses what is on the machine, in this
order:

| Provider | What it is | Privacy |
|---|---|---|
| **Built into DBine** | llama.cpp inside the app (Metal on Mac) with Qwen2.5-Coder 3B, 7B or 32B, downloaded once | Local: nothing leaves the machine |
| **Ollama** | local server (`localhost:11434` or `OLLAMA_HOST`) | Local |
| **Claude Code** | the `claude` CLI with the user's account | The question, the structure and the editor go to Anthropic |
| **Codex** | the `codex` CLI with the user's account | Same, to OpenAI |
| **LM Studio** | local OpenAI-compatible server (`localhost:1234`) | Local |

**Detection:**
- An app opened from the Finder does not inherit the terminal's PATH. That is
  why the login shell's PATH is read once and the usual folders are added
  (Homebrew, global npm, mise, asdf, nvm…).
- If Ollama is installed but closed, it offers to open it.
- If Ollama is open but has no models, it offers to download
  `qwen2.5-coder:7b`, or `qwen2.5-coder:32b` from 48 GB of RAM.

**With nothing installed:** downloading the built-in model is recommended.
The largest one the machine's RAM can handle comfortably is marked as
recommended: the 7B from 16 GB and the 32B from 48 GB. Whoever already uses a
smaller one than the recommended sees a notice in the chat to download it
("Not now" hides it for that model).

**Why those models.** In a test on 2026-10-03 (MacBook M5 Max), real SQL
Server queries across databases (`[database].schema.table`, with brackets
because of the hyphens) only came out right with the 32B: the 3B, the 7B and
the 14B named the database or the schema wrong, and Qwen3-30B-A3B spent the
answer thinking. The 14B did not perform better than the 7B, so it is not in
the catalog. In the same test, 24 legitimate requests that sound sensitive
(passwords, permissions, personal data, deletions) had no refusals with any
model.

### Claude Code and Codex, without tools

Each answer is a new process that runs in an empty temporary folder.

- **Claude Code:** `claude -p --output-format stream-json --include-partial-messages --tools "" --strict-mcp-config --setting-sources "" --no-session-persistence --system-prompt-file …`.
  It has no tools or MCP servers, and it does not load the user's
  configuration, so its hooks do not run.
- **Codex:** `codex exec --json --skip-git-repo-check --sandbox read-only -C <empty folder> -`.

### Built-in model

- **Nothing inside the app:** neither the model nor llama.cpp come in the
  installer. Whoever uses Claude Code, Codex, Ollama or LM Studio downloads
  nothing.
- **Engine:** the official llama.cpp build for the platform (release
  `b11213` of github.com/ggml-org/llama.cpp, fixed size and SHA-256 in
  `crates/dbine-ai/src/embedded.rs`): Metal on macOS, CPU on Windows and
  Linux. It weighs between 11 and 19 MB and is stored in
  `models/llama.cpp-<release>/`.
- **When it is downloaded:** together with the first model, in the same
  progress bar. If the model was already on disk (earlier installs), in the
  first chat: the message shows "Downloading the AI engine, just this once…
  45 %".
- **How it runs:** its `llama-server` as a child process, listening only on
  `127.0.0.1` on a free port and with a random API key per start. The chat
  goes through its OpenAI-compatible API (the same client as LM Studio), with
  temperature 0.2 and up to 3072 tokens of answer. The model stays loaded
  between answers; when another is chosen, the server restarts with it.
- **Models:** the official Qwen GGUFs (Q4_K_M) are stored in `models/` inside
  the app's data folder. The download can be paused and resumed, and the
  SHA-256 is verified before using the file.
- **Chat template:** applied by `llama-server` from the GGUF file itself; it
  is not handwritten.
- **Shutdown:** the app stops the server on exit. If the app closed abruptly,
  the next engine start terminates the server that was left alive (its PID is
  kept in `server.pid`).
- **Windows:** `llama-server` needs Microsoft's Visual C++ runtime. If it is
  missing, the chat explains it and asks to install "Microsoft Visual C++
  Redistributable".
- **End-to-end test:**
  `DBINE_TEST_MODELS=<folder with a model> cargo test -p dbine-ai -- --ignored`
  downloads the engine into that folder and chats with the model.
- **When updating llama.cpp:** change `ENGINE_TAG` and the names, sizes and
  SHA-256 of `ENGINE` in `embedded.rs`.

## Context that goes with each question

It is built in `src-tauri/src/commands/ai.rs`:

- **Engine and language:** SQL with its dialect, CQL, JSON/Mongo, Redis, Flux
  or Cypher. Also the current database and whether the connection is
  read-only.
- **With a local model:** only the names of the tables, views and routines of
  the tab's database (up to 400, with the number of those left out), from the
  list the explorer already has. The columns, keys and indexes are requested
  by the model with the catalog tools (see "Query the whole connection"),
  without approval. There is no checkbox to turn it off: it is lightweight,
  it comes from the cache and llama.cpp reuses what it already processed of
  the same text.
- **With Claude Code or Codex** (without tools), the compact structure:
  - It comes from `database_schema`, so it works with all the drivers that
    implement it. It is cached for 10 minutes.
  - Compact format: `schema.table(col type PK, col type NOT NULL, …)` and
    their FKs.
  - If it does not fit whole, the tables named in the question, in the editor
    or in the tab go first, and of the rest only the name.
  - Cap: about 150,000 characters.
- **Editor:** the text of the open query, the selection and the error of the
  last run.
- **Dialect hints** (SQL Server, PostgreSQL, MySQL, Oracle, SQLite): how names
  are quoted, how a table from another database is named and how rows are
  limited, which is where small models get it wrong the most.
- **Data rows are never sent.**

## If the model refuses

A small model sometimes answers "Sorry, I can't help you with that" to a
legitimate request about the user's own database. If the answer is short,
brings no code and apologizes or says it can't (in Spanish, English,
Portuguese, French or Italian), DBine asks again just once, adding to the
prompt that the request is legitimate and that the user decides what to run.
The chat shows "Asking again…" and replaces the refusal with the new answer.

Below each question it shows what context traveled; for example,
"SQLite · 2 tables · editor" (with a local model, "· 48 objects").

## Preferences

They travel with sync:

- `ai.provider`
- `ai.model.<provider>`

The conversation stays on the machine (the last 60 entries). "New
conversation" does not delete it: it moves it to the **History** (the clock
icon in the panel header), which keeps the last 50 conversations with their
first question as the title. From there one is resumed (the current one goes
to the history) or one or all are deleted. Everything stays in the app's local
storage; if it fills up, the oldest are discarded first.

## Commands

| Command | args | What it does |
|---|---|---|
| `ai_detect` | — | Providers, built-in model catalog and recommended one |
| `ai_chat` | `{ chat_id, provider, model, messages, context }` | Answers streaming (events `ai-delta` and `ai-status`) |
| `ai_cancel` | `{ id }` | Stops a chat or pauses a download |
| `ai_download_model` | `{ id }` | Downloads a built-in model (event `ai-download`) |
| `ai_delete_model` | `{ id }` | Deletes a built-in model |
| `ai_start_ollama` | — | Opens Ollama and waits for it to respond |
| `ai_pull_ollama` | `{ id }` | `ollama pull` with progress |

To try a provider from the terminal:

```sh
cargo run -p dbine-ai --features embedded --example ask -- embedded qwen2.5-coder-3b "question"
cargo run -p dbine-ai --example ask -- detect
```
