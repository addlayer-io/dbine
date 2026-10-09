# SSH tunnels

A connection can reach its server through an SSH server (a bastion), and
optionally pass through other bastions first. It's for databases that are
only reachable from inside a network.

## How to use it

In the connection form, the **SSH tunnel** panel:

- **SSH server**, **port** (22 if left empty) and **SSH user**.
- **Authentication**:
  - **Password**.
  - **Private key**: the file (OpenSSH or PEM; `~` works) and its passphrase,
    if it has one.
  - **SSH agent**: the keys loaded in the agent (`ssh-agent`, 1Password,
    Windows' OpenSSH agent).
- **Jump hosts** (optional): SSH servers to go through first, in order, like
  `user@bastion1:22, bastion2`. If they don't give a user or port, the SSH
  user and 22 are used. They use the same authentication.

The database server and its port are written as usual, as the last SSH server
sees them (for example, `db.internal:5432`).

The panel appears on engines that connect over the network. It doesn't appear
on those that use a local file (SQLite, DuckDB, etc.).

## Server verification

The first time DBine connects to an SSH server it shows the fingerprint of
its key (`SHA256:…`) and asks whether to trust it. It must be compared with
the one the server administrator gives. The accepted fingerprint is saved in
the connection together with the server it was accepted for
(`[host]:port SHA256:…`), and the form shows "N verified SSH servers", with
the option to forget them.

- `~/.ssh/known_hosts` is checked first. A server that is already there is
  accepted without asking.
- If a server's key doesn't match the one in `known_hosts`, the connection is
  rejected, even if that key was accepted in DBine: it may be another server
  impersonating it.
- With jump hosts, each server is verified separately: a key accepted for one
  server (a bastion) is never accepted for another one (the next hop).
- Accepting a new key for a server replaces the one accepted before for it.
- Fingerprints saved by older versions (0.1.10 and before) don't say which
  server they were for, so they are no longer accepted: DBine asks once more
  for each server, and the old ones are dropped when a key is accepted.

## What is stored and where

- The configuration goes in the connection options (`ssh.enabled`,
  `ssh.host`, `ssh.port`, `ssh.user`, `ssh.auth`, `ssh.key_path`, `ssh.jump`,
  `ssh.trusted`).
- The SSH password and the key passphrase go to the system keychain, never to
  the state file or the logs. They are saved even if the connection doesn't
  save the database password: the "Save password" option refers to the
  database.
- When importing connections from other tools, the SSH tunnel comes with
  them, with its password or passphrase if the tool stored it.

## How it works

- DBine opens a local port (on `127.0.0.1`, chosen by the system) and forwards
  it over SSH to the database server (`crates/dbine-tunnel`, with `russh`).
- Only processes of the user running DBine (DBine and its driver hosts) can
  use that port: every connection to it is checked against the system's
  socket tables (`/proc/net/tcp` on Linux, the processes' open sockets on
  macOS, the TCP table on Windows) and closed if it comes from another user
  or can't be attributed. Other accounts on the same machine can't reach the
  database with the user's SSH identity.
- The driver connects to that local port as if it were the server, so any
  engine that connects over the network works with a tunnel without knowing
  anything about SSH. There are no changes in the drivers.
- **One tunnel per connection:** all of the connection's sessions (explorer,
  tabs, exports, Profiler) use the same tunnel.
  - It opens the first time it's needed.
  - If the SSH session drops, it is reopened on the next connection.
  - It closes when disconnecting or editing the connection.
- Keepalives are sent every 30 s so a firewall doesn't cut an idle tunnel.

## Particularities

- **TLS:** with a tunnel, the driver connects to `127.0.0.1`. If the database
  requires TLS and verifies that the certificate matches the server name, that
  verification fails. In that case you must turn on "Trust the server
  certificate" or, if the driver allows it, give the expected name.
- **Addresses in URLs:** on engines that use a URL as the server (for
  example, `https://es.internal:9200`), the tunnel replaces only the URL's
  host and port.
- Engines that connect to cloud services over HTTPS with a fixed name
  (BigQuery, Snowflake, Cosmos DB, DynamoDB…) usually don't need a tunnel. If
  one is configured, the same TLS considerations apply.
