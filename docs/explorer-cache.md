# Explorer cache

When a connection is opened, the tree appears immediately with what it showed
last time: the databases, each database's objects and each object's columns.
Meanwhile DBine asks the server, as usual.

## How it works

It is *stale-while-revalidate*:

1. On connecting, or on expanding a database or an object, the UI asks for
   what is stored (`get_cached`) and shows it right away.
2. In parallel, the server answers the usual way. While it waits, the node
   shows the **refreshing…** notice.
3. When the answer arrives, **the server wins**: it replaces what was on
   screen and what was stored. An object that no longer exists disappears.

If nothing is stored (first time, or it was cleared), the tree behaves as
before: it waits for the server. If the server fails, the error is shown the
same as without a cache.

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
- `web/src/stores/connections.ts`: the `fromCache` (databases) and `stale`
  (objects and columns) states that the UI shows as "refreshing…".

## Contract

It doesn't touch the drivers: the cache stores what `list_databases`,
`list_objects` and `get_columns` already return. It is the same on all
engines.
