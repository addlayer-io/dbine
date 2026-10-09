# Processes

A connection's Monitor has a **Dashboard / Processes** selector. **Processes**
is the list of what the server is doing right now: running sessions and
statements, with the option to cancel a query or end a session.

## What it shows

- One row per process. What a "process" is depends on the engine: in most it
  is a session (with its current statement, if it has one); in engines
  without sessions it is a running query or task (see
  [By engine](#by-engine)).
- Columns **appear only if the engine reports them**: session, status,
  user, host, program, database, command, duration, CPU, reads, writes,
  wait, blocked by and statement. What an engine doesn't know stays empty,
  and if no row has a given value, the column isn't shown.
- The session DBine uses to list the processes is marked **DBine**.
- The server's own processes (checkpointer, replication, internal tasks) are
  marked as system processes.
- Sort by any column by clicking its header.
- The counter shows "N of M": visible rows against the total.

## Refresh

- The list polls every **2, 5, 10 or 30 s** (the same interval selector as
  the Monitor) and can be **paused**.
- It polls over its own connection, so it doesn't wait behind a dashboard
  snapshot or one of your running queries. If the connection drops, the next
  read reconnects.
- While the list is shown, the **dashboard stops polling**; going back to
  **Dashboard** resumes it.

## Filters

- **Active only**: those running something right now (not idle or sleeping).
- **Hide system processes**: removes the server's own processes.
- **Database**, **user** and **host**: lists with the values present at that
  moment ("All databases", "All users", "All hosts").
- **Text search** over the rows.

Filters combine.

## Locks

A session that others are waiting on is highlighted with **blocks N** (how
many are waiting). It is the head of a chain: the detail shows the whole
chain (see below). The [Locks](locks.md) panel is still the view by chains;
here the information appears in the list.

## Row detail

Selecting a row opens its detail:

- The **whole statement** (not truncated as in the table). If the process
  isn't running anything, it says so.
- The **blocking chain**: whom it waits for and who waits for it.
- **Open in a query**: puts the statement in a new query tab, without
  running it.
- **Copy**: copies it to the clipboard.
- **Cancel query** and **End session** (below).

## Cancel query and end session

- **Cancel query** stops the statement the session is running and **leaves
  the session open**.
- **End session** closes the session on the server and **rolls back its
  transaction in progress**.

Both:

- ask for confirmation, with the session's id and user;
- don't appear on **read-only** connections;
- follow the `kill_session` permission check: if the user lacks the
  permission on the server, the button is disabled and says which one is
  missing;
- are rejected for DBine's own session (the one that lists the processes).

Each button appears only where the engine has the action (see
[`engine-support.md`](engine-support.md#processes)).

## By engine

- **SQL Server, Azure SQL, Fabric and Babelfish** only **end sessions**:
  `KILL` is all there is and it closes the session. They show the
  parameterized statement as the server sees it; Fabric doesn't return the
  text. An idle session with an open transaction shows its last batch.
- **Oracle** cancels with `ALTER SYSTEM CANCEL SQL` (18c or later). If the
  session belongs to **another DBine tab** it refuses to cancel it (the
  client library never returns from that interruption and the tab would hang)
  and points to **End session** or to cancelling from that tab.
- **ksqlDB**: cancelling a **persistent** query **pauses** it (`PAUSE`); it
  resumes with `RESUME`. Terminating it (`TERMINATE`) would stop it for good.
  A *push* query does end, for its client.
- **TDengine** only **cancels**: its connections are taosAdapter's shared
  pool, so closing one would cut off other clients. No row is marked as
  DBine's own.
- **Snowflake**: polling **keeps the warehouse on** (the history function
  needs it). DBine doesn't resume a suspended one: if it's off, the list
  doesn't wake it. **End session** closes the session running the chosen
  query.
- **Redis, Valkey and Dragonfly** only see as "running" the clients stopped
  on a blocking command (`BLPOP`, `XREAD BLOCK`…): Redis runs one command at
  a time. Cancelling is `CLIENT UNBLOCK` (the command fails, the connection
  stays); it doesn't return the command's arguments. Dragonfly has no
  `CLIENT UNBLOCK`: it doesn't cancel.
- **Neo4j and Memgraph** can't stop a query and keep its transaction:
  cancelling is `TERMINATE TRANSACTION`, which rolls it back (the client
  connection stays open).
- **MongoDB**: cancelling a running operation is `killOp`; an idle session
  with an open transaction is ended with `killSessions`.
- **Firebird** shows only the user's own connections if the user is not
  SYSDBA / `RDB$ADMIN` and doesn't have `MONITOR_ANY_ATTACHMENT`.
- **CouchDB** lists tasks (indexing, compaction, replication); only
  transient replications can be cancelled. One defined in a `_replicator`
  document is stopped by changing that document.
- **Query-centered engines** have no sessions: they list **running
  queries**, not sessions. They are ClickHouse, Trino, Presto, Starburst,
  BigQuery, Athena, Databricks, Dremio, Drill, Spanner, Couchbase and
  Elasticsearch / OpenSearch (tasks). For them, "End session" doesn't exist
  and "Cancel query" stops the chosen query. Drill truncates the text to 150
  characters.

## Contract

- `Session::processes()` returns `Vec<ServerProcess>`: `id`, `status`,
  `active`, `system`, `own`, `user`, `host`, `program`, `database`,
  `command`, `elapsed_ms`, `cpu_ms`, `reads`, `writes`, `wait`, `blocked_by`
  and `sql`. Each engine fills in what it reports. The `id` is the one
  `cancel_query` and `kill_session` take.
- `Session::cancel_query(id)` stops the statement and leaves the session.
- `Session::kill_session(id)` ends the session (the same method as
  [Locks](locks.md)).
- Capabilities: `Capabilities::processes`, `cancel_query` and `kill_session`,
  according to what each engine implements (per variant or version when it
  differs). The interface shows the list and each button only where
  applicable.
- Tauri commands: `monitor_processes`, `monitor_cancel_query` and
  `monitor_kill_session`, in
  [`api-commands.md`](api-commands.md#server-monitor).

What each engine supports and why it's missing in the others is in
[`engine-support.md`](engine-support.md#processes).
