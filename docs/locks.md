# Locks

A connection's Monitor has a **Locks** panel: who is waiting for whom right
now, and the option to end a session.

## What it shows

- Blocking chains as a tree: first each session that holds what others are
  waiting for (the **head**, with how many sessions are waiting for it) and
  below it those that wait for it, directly or indirectly.
- For each session: its id, user and client, database, what it waits for (the
  lock type or state, as the engine reports it), for how long, the blocked
  object and the current or last statement.
- A head that is **idle with an open transaction** is the typical case:
  someone started a transaction and didn't finish it. The panel says so.
- With no locks, the panel shows "No blocking". It refreshes along with the
  rest of the Monitor.

## Ending a session

**Kill**, on each row, asks for confirmation (with the session's statement)
and ends the session on the server: its transaction in progress is rolled
back.

- Each engine uses its native way: `KILL` in SQL Server and MySQL,
  `pg_terminate_backend` in PostgreSQL, `ALTER SYSTEM KILL SESSION` in
  Oracle, `killOp` / `killSessions` in MongoDB, `TERMINATE TRANSACTION` in
  Neo4j, etc.
- The id is validated before building the command.
- The session DBine uses to look can't be ended.
- **Read-only** connections show the locks but don't allow ending sessions.
- The corresponding permission on the server is required (for example,
  `VIEW SERVER STATE` and `ALTER ANY CONNECTION` in SQL Server). If it is
  missing, the panel shows the server's message.

## Contract

- `Session::blocking()` returns the sessions of the chains
  (`BlockedSession`: id, `blocked_by`, user, client, database, wait, time,
  object and statement).
- `Session::kill_session(id)` ends one.
- Each driver declares `Capabilities::blocking` and `kill_session` according
  to what it implements, per variant. The interface shows the panel and the
  button only where applicable.

What each engine supports is in
[`engine-support.md`](engine-support.md#locks).
