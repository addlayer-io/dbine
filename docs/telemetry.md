# Telemetry

DBine sends **anonymous** usage data. It's used to know how much the tool is
used, which database engines are worth prioritizing and which operating
systems it runs on.

## How to turn it off

- It is **on by default**. On first launch, a notice explains what is sent and
  what isn't, and how to turn it off.
- It is turned off in **Settings › General › Share anonymous usage data**.
  From that moment nothing more is sent.
- It is just another preference: it travels with sync to the same user's
  other machines.
- To turn it off on a whole machine (managed installations), the environment
  variable `DO_NOT_TRACK=1` or `DBINE_TELEMETRY=0` is enough. With either one
  no event goes out, whatever the settings say.
- Whoever had declined it in an earlier version (when the app asked) still has
  it off.

## What is sent

Four events, nothing else:

| Event | When | Own data |
|---|---|---|
| `app_started` | once per run of the app | — |
| `connection_opened` | the first time each engine is connected to in a run | `engine`: the driver id (`postgres`, `sqlserver`, `redis`…) |
| `module_opened` | the first time each module is shown in a run | `module`: the tab type (`query`, `object`, `designer`, `diagram`, `monitor`, `profiler`, `migration`, `connection`, `compare`, `dataCompare`, `security`, `backups`, `indexes`, `dependencies`, `file`, `fileDiff`) or the side panel (`ai`, `projects`, `library`, `history`) |
| `feature_used` | the first time each feature is used in a run | `feature`: `ai_message` (a question to the assistant), `mcp_tool` (an MCP client used a tool), `schema_sync` (a schema sync was applied), `data_sync` (data), `migration_run` (a migration was run), `multi_db_run` (a query on several databases). With `ai_message`, also `provider`: the provider type (`embedded`, `ollama`, `lm_studio`, `claude_code`, `codex`), never the model or the text |

With `connection_opened` and `module_opened` you see which engines and
modules are really used, and which never appear; with `feature_used`, whether
features are used beyond opening their screen. In Aptabase, the properties
(`engine`, `module`, `feature`, `provider`) are seen by opening each event.

Each event also carries:

- the DBine version;
- the operating system and its version (`macOS 26.0`, `Windows 10.0.26100`,
  `ubuntu 24.04`);
- the app's language;
- whether it is a development build (those events are seen separately and
  don't mix with the released app's);
- a random session id, which changes on each run and after 4 hours without
  use.

The country is deduced by the service from the connection; **the IP is not
stored**.

## What is never sent

- Names of connections, servers, ports, users, databases, tables or schemas.
- Queries, results or any data from the databases.
- Passwords or secrets.
- Any identifier of the user, the machine or the installation.

The list is fixed in the app's code, not in the interface:
`src-tauri/src/commands/telemetry.rs` discards any event that isn't one of
the four above, any engine that isn't a known driver and any module, feature
or provider that isn't in its lists.

## Where it is stored

In [Aptabase](https://aptabase.com), an open-source analytics service built
with privacy in mind, in its United States region. If there is no internet
connection, the event is discarded: it isn't kept for later.
