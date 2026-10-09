# MCP server

DBine can act as a local MCP (Model Context Protocol) server. With it,
assistants such as Claude Code or Codex can see your connections, browse the
structure of the databases and, where you enable it, run read-only queries
or, approving each one, changes. It only works while DBine is open.

## How to turn it on

1. Open **Settings › MCP** and turn on **MCP server**. It comes off.
2. Check the **port** (`27517` by default). If another program is using it,
   DBine warns you right there: pick another one and save it. DBine doesn't
   change the port on its own.
3. Under **Clients**, create one per assistant ("Claude Code", "Codex"…).
   DBine shows its token **only once**, together with the configuration ready
   to paste.

## Access levels

Each connection has a level, which decides what a client can do with it:

| Level | What it allows |
|---|---|
| Disabled | Nothing: clients don't see the connection. |
| Schema | List databases and objects, and see columns, keys, indexes and foreign keys. No data. |
| Read | Also: sample rows (up to 100), read-only queries (up to 500 rows, 30 s by default) and estimated execution plans. |
| Write | Also: changes to data and structure with `execute`. Each one is approved in DBine before it runs (see [Approvals](#approvals)). |

The **default level** is chosen in Settings › MCP and applies to connections
without their own level. Out of the box it's **Schema**. In each connection's
form, **MCP access** lets you use the default or set another one.

Two caps always apply, whatever level is chosen:

- a connection tagged `prod` (upper or lower case) never goes beyond **Read**;
- a connection configured as read-only never goes beyond **Read**.

## Tools

| Tool | Level | What it does |
|---|---|---|
| `list_connections` | Schema | Name, engine and level of each visible connection. Never shows hosts, users or passwords. |
| `list_databases` | Schema | A connection's databases. |
| `list_objects` | Schema | Tables, views, collections and other objects of a database. |
| `describe_object` | Schema | Columns, primary key, foreign keys and indexes. |
| `index_usage` | Schema | A table's indexes and how much they're used: reads, writes, percentage of reads, unused and disabled. |
| `sample_rows` | Read | The first rows of a table or collection. |
| `run_query` | Read | A read-only query in the engine's language. |
| `explain` | Read | A query's estimated plan, in the engines that have plans. |
| `execute` | Write | Code that changes data or structure, in the engine's language. Waits for the user's approval. |

Queries always run in the MCP server's own read-only session. In SQL engines,
a statement that modifies data or structure is rejected before reaching the
server. Every word of the statement is checked, not only the first one, so
these are rejected too:

- a write hidden after a read in a T-SQL batch;
- a data-modifying CTE;
- `SELECT … INTO`;
- `EXEC`;
- `SET`;
- functions with side effects (`set_config`, `dblink_exec`, `xp_cmdshell`…).

Where the engine has a server-side read-only mode (PostgreSQL, MySQL), the
session also runs in it, and the statements that would switch it off are
rejected. The other engines use their own read-only mode. That also applies to
connections with the **Write** level: to change anything, the assistant has to
use `execute`.

## Approvals

Every time an assistant asks for `execute`, DBine opens a window (even if
minimized or hidden) with the client, the connection, the database and the
exact code, and doesn't run anything until you answer:

- **Approve**: only that request runs, in the MCP server's own session and
  with the same time limit as queries (30 s by default). The assistant
  receives "approved and executed" with the affected rows or the ones it
  returned.
- **Reject**: it doesn't run; the assistant receives "rejected by the user".
- **Approve all**: this request and the following ones from that client run
  without asking, until you close DBine, remove it or revoke the client. The
  risk is yours. While active, Settings › MCP shows it next to the client
  ("Approving everything until DBine is closed") with the **Remove** button.

If you don't answer within **2 minutes**, the request is rejected and the
assistant receives "no response: rejected". If several arrive at once, they're
shown one at a time, with the number still waiting. Each request and its
result (approved, rejected or no response) are recorded in the activity.

## Connect Claude Code

With `--scope user` it's available in all projects, not just the folder where
the command is run.

```sh
claude mcp add --scope user --transport http dbine http://127.0.0.1:27517/mcp --header "Authorization: Bearer <token>"
```

## Connect Codex

In `~/.codex/config.toml`:

```toml
[mcp_servers.dbine]
url = "http://127.0.0.1:27517/mcp"
http_headers = { "Authorization" = "Bearer <token>" }
```

## Cursor

In `~/.cursor/mcp.json` (or `.cursor/mcp.json` inside a project):

```json
{
  "mcpServers": {
    "dbine": {
      "url": "http://127.0.0.1:27517/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

## Claude Desktop

Claude Desktop only starts local servers by command, so it uses `mcp-remote`
as a bridge (it needs Node). In
`~/Library/Application Support/Claude/claude_desktop_config.json` (on Windows,
`%APPDATA%\Claude\claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "dbine": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:27517/mcp", "--header", "Authorization:${DBINE_AUTH}"],
      "env": { "DBINE_AUTH": "Bearer <token>" }
    }
  }
}
```

Then restart Claude Desktop.

## VS Code (Copilot)

```sh
code --add-mcp '{"name":"dbine","type":"http","url":"http://127.0.0.1:27517/mcp","headers":{"Authorization":"Bearer <token>"}}'
```

## Windsurf

In `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "dbine": {
      "serverUrl": "http://127.0.0.1:27517/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

## ChatGPT

It can't be connected. ChatGPT calls MCP servers from OpenAI's servers, so it
needs a public HTTPS URL, and DBine's listens only on this machine
(`127.0.0.1`) on purpose. Exposing it to the internet through a tunnel would
leave the databases within reach of anyone who has the URL and the token: we
don't recommend it. To use an OpenAI model with DBine, use Codex (above).

## Other clients

Those that accept an `mcpServers` block with HTTP servers:

```json
{
  "mcpServers": {
    "dbine": {
      "type": "http",
      "url": "http://127.0.0.1:27517/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

In every case, `<token>` is the one DBine shows once when the client is
created, and the port is the one listed in Settings → MCP server.

## Activity

Every call is recorded on this machine with the time, the client, the
connection, the tool, a summary (the query, trimmed) and the result with the
number of rows. The last 10,000 are kept and shown in Settings › MCP,
filterable by client or by connection.

## Security

- **Local only.** The server listens on `127.0.0.1`, never on the network. It
  rejects requests coming from a browser page (`Origin` header) and those
  arriving with a different host name.
- **Token per client.** Every request carries `Authorization: Bearer <token>`.
  Each client is revoked separately and stops working immediately.
- **Fingerprint only.** DBine stores the token's SHA-256, never the token. If
  you lose it, revoke the client and create another.
- **Caps.** `prod` connections and read-only ones never go beyond Read, and
  every write needs your approval in DBine.
- **No credentials.** No response or the activity log includes passwords,
  tokens or other secrets, and `list_connections` doesn't show hosts or users.
- **Query results reach the model.** What `sample_rows` and `run_query`
  return is sent to the assistant and, depending on how it works, to the
  model's provider. Enable **Read** only where that's acceptable.
- All of this is local to this machine: the server configuration, the clients
  and the activity don't travel in the cloud backup. Each connection's level
  does, with the rest of the connection.
