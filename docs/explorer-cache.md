# Explorer cache

Once a connection is confirmed, the tree shows right away what it showed last
time: each database's objects and each object's columns. Meanwhile DBine asks
the server, as usual.

## How it works

It is *stale-while-revalidate*:

1. Expanding a connection connects first (the node shows **Connecting…**);
   nothing below it shows until the server has answered. Then, on expanding
   a database or an object, the UI asks for what is stored (`get_cached`)
   and shows it right away.
2. In parallel, the server answers the usual way. While it waits, the node
   shows the **refreshing…** notice.
3. When the answer arrives, **the server wins**: it replaces what was on
   screen and what was stored. An object that no longer exists disappears.

If nothing is stored (first time, or it was cleared), the tree behaves as
before: it waits for the server.

A background re-read (after five minutes, when you come back to DBine, after
a `CREATE`, `ALTER` or `DROP` in the editor) keeps what's on screen while it
asks: the tree doesn't close or show **Loading…**, and only the objects that
appeared or disappeared change.

## The filter

The tree reads folder by folder, so the filter at the top doesn't stop at
what's open. As you type, it filters what's loaded at once and, after a short
pause, looks for the text in the databases in sight (the open ones, or each
connected connection's default database when none is open):

1. **In this cache**, by name, in every folder it kept for the database: the
   matches show at once, in their folders (Tables, Views…), also while
   connecting.
2. **On the server**, by name, with the engine's narrow query (a page of up
   to 200 matches). Its answer replaces the cache's. If the server can't
   answer, the cache's stay.
3. With **</>** (search in code) on, and from three letters, **in the objects'
   definitions**: views, procedures, functions, triggers, and Power BI's
   measures and tables (their M and DAX). This uses the code search of
   [Search in database](search.md), on a session of its own. An object found
   this way shows a **code** tag, and its tooltip has the first line where the
   text is. If the login can't read some objects' code, the line under the
   filter says how many.

The filter opens the way to each match without opening the folders by hand,
and clearing it leaves the tree as it was. BigQuery bills each metadata query,
so for it the server and code searches wait for **Enter** (the line under the
filter says so); the cache still answers as you type.

## When the connection fails

Connect first, then show: what the tree shows always belongs to a connection
that answered.

- **Failed connect.** The connection shows only a banner: what happened (for
  example "The server's firewall blocks your IP"), **Retry** and **What's
  wrong…**; the whole message is its tooltip. No cached tree is shown.
- **No silent retries.** Expanding a connection that failed shows its banner
  again without connecting; **Retry**, the context menu's **Connect** /
  **Refresh**, and explicit actions in a tab (running a query, opening a
  table) try again. One connect runs per connection at a time: every expand,
  completion or tab that needs it waits for that one (the backend shares it
  the same way per session). Repeated failures with the same message leave
  one entry in Output.
- **Dropped later.** When a load fails with a connection error, DBine checks
  the connection again (a connect, at most every 10 seconds). If it fails,
  the connection flips to its banner and its children hide. A structure read
  that only timed out on a busy or blocked server (kind `busy`) doesn't count.
- After a successful retry, the open nodes that failed load again.
- **Tabs.** A tab never stays blank: while its connection connects it shows
  **Connecting…**, and if it can't, the error with **What's wrong…** and
  **Retry** (in a query tab, in the results pane and the status bar; Retry
  runs the same script again). Code: `composables/connectionGate.ts`,
  `components/ConnectionState.vue`.

### What's wrong…

The dialog explains the failure by its kind, which the driver reports
(`Error::ConnectFailed`, see `docs/drivers.md`) or the app guesses from the
message, so every engine gets it:

| Kind | What it says to check |
|---|---|
| Firewall | The server refuses this client's IP. On Azure SQL (error 40615) it shows the IP Azure saw and offers **Add my IP to the Azure firewall**. |
| Timeout / refused | VPN, a firewall on the way (Azure NSG or WAF, the VM's or the network's), the server or port down. A `*.cloudapp.azure.com` host is SQL Server on a VM: its rule goes in the NSG/WAF, not in the Azure SQL firewall. |
| DNS | The host name. |
| TLS | The certificate and the connection's encryption options (**Edit connection**). |
| Auth | User and password, or the Entra method (**Edit connection**). |
| Unknown | The server's message. |

**Show my public IP** asks `https://api.ipify.org` only when clicked.
**Copy details** copies the connection name, server, kind, code, client IP and
message: never a password.

**Add my IP to the Azure firewall** creates a server-level rule
`DBine-<user>-<yyyymmdd>` for one IPv4 with the user's Azure CLI (`az`)
login, after a confirmation that shows the server, rule name and IP; on a
connection tagged as production it also asks to type the server's name. It
finds the server by its host name (`az sql server list`), in the current
subscription and then in the others the login sees, and runs
`az sql server firewall-rule create`. `az` is looked up on the PATH and in
its usual folders, run without a shell and with a time limit; the IP must be
a public IPv4 and every value passed to it is validated. Without `az`, or
logged out, the dialog shows the command, the Azure portal and the T-SQL
alternative (`sp_set_firewall_rule` in `master`, from a connection that is
allowed), ready to copy. Code: `src-tauri/src/commands/azure_firewall.rs`,
`web/src/components/ConnectionProblemDialog.vue`.

## What is stored

- The connection's databases, each database's objects and each object's
  columns.
- Names and structure only. Table rows or data are **not** stored, nor
  passwords or any secret.

## Where it lives

In `cache.db`, a SQLite file next to the app's state. It isn't part of the
state:

- it isn't synced with the cloud backup;
- if it is lost or deleted, nothing is lost: it fills up again as you
  navigate.

If the file can't be opened, DBine continues without a cache and logs it.

## When it is deleted

- On **deleting the connection**, everything of its own is deleted.
- On **saving the connection with another** driver, host, port, database,
  user or options: it points to another server or another login, so its
  stored tree no longer applies. Changing the name, folder or color doesn't
  delete it.

## What isn't cached

- Key searches in Redis and etcd.
- Table data (rows).

## Code

- `crates/dbine-core/src/cache.rs`: `ExplorerCache` (`get`, `put`, `remove`,
  `forget_connection`). Each entry is identified by connection, database,
  type (`databases`, `objects`, `columns`) and object.
- `src-tauri/src/commands/explorer.rs`: `get_cached` and writing the server's
  answer in `list_databases`, `list_objects` and `get_columns` (databases are
  also stored on connecting, in `connections.rs`).
- `src-tauri/src/commands/connections.rs`: deletion on removing or changing
  the connection.
- `web/src/stores/connections.ts`: the `stale` state (objects and columns)
  that the UI shows as "refreshing…", the connect single-flight and
  `recheckAfter` (a load's connection error rechecks the connection).

## Contract

It doesn't touch the drivers: the cache stores what `list_databases`,
`list_objects` and `get_columns` already return. It is the same on all
engines.
