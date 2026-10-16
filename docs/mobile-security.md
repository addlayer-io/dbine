# Phone app: security model

This page says what the DBine phone app can and can't do, what travels between
the phone and the computer, and who can see what. It is the reference for the
code, the docs, dbine.com and any promotional material: if one of them says
something this page doesn't, this page wins and the other gets fixed.

Design and implementation: [mobile-companion.md](mobile-companion.md).

## In one paragraph

The phone never connects to a database. Your connections, hosts, users and
passwords stay on your computer, as always. The phone asks DBine on your
computer for a short list of things (who is blocking whom, what is running,
the server's figures) and DBine answers. Phone and computer recognize each
other by keys exchanged through a QR code you scan, and everything between
them is encrypted end to end with those keys. On your local network nothing
is in the middle. Outside it, only if you turn that on, the connection may go
through a relay server that forwards encrypted bytes it can't read.

## What the phone can do

A closed list, the same for every phone:

| What | Needs |
|---|---|
| See the computer's connections (name, engine, color) | Being paired |
| See a connection's Monitor, blocking chains, running statements and health check | Being paired |
| See the editor's running and recent long queries, and the scheduled tasks with their runs | Being paired |
| Get notified when a long query ends or a task fails or alerts | Being paired |
| **End a session** or **cancel a query** | "Puede terminar sesiones y cancelar consultas" on that phone (off by default), and the phone's biometrics or PIN each time |
| **Run a scheduled task** | "Puede ejecutar y cancelar tareas" on that phone (off by default), the phone's biometrics or PIN each time, and the task's writing steps approved on the computer |
| **Cancel a running scheduled task** | "Puede ejecutar y cancelar tareas" on that phone, and the phone's biometrics or PIN each time. The step in progress is cut and no further step runs, also for a run the OS scheduler started with DBine closed |

What the phone **can't** do, by design:

- run any SQL or command of its own: there is no request for it;
- read table data: no request returns rows of user tables;
- see or receive hosts, ports, users, passwords, connection strings, SSH
  keys or any secret;
- change DBine's settings, connections, tasks or permissions;
- pair another phone.

## What travels, and what doesn't

**Travels** (encrypted, between your phone and your computer only): the
connections' names and engines, and their databases' names; Monitor figures and the facts the engine
reports about itself (version, uptime and, for some engines, the server's own
name); session ids, users, client hosts, wait types, locked objects and the
statements involved in locks and processes (trimmed to 500 characters);
health-check findings (with the objects they name); the first lines of the
editor's queries with their duration and row counts or error; task names, run
statuses and step summaries; notices.

**Never travels:** passwords, the hosts and ports in your connection
settings, connection strings, SSH or TLS keys, the vault or the keychain,
table rows, DBine's settings.

## Pairing

1. You turn on **Permitir celulares** in Configuración › Dispositivos (off by
   default).
2. **Vincular un celular** shows a QR code with DBine's public key, a
   one-time 32-byte secret and where to reach DBine. It expires in 2 minutes
   and pairs one phone. The code is a link, `https://dbine.com/pair#…`, with
   all of that in the fragment (after `#`), which browsers never send to a
   server. Scanned with the phone's camera, it opens the app (Android App
   Links, verified for dbine.com); without the app, dbine.com/pair (no
   analytics, it clears the fragment at once) offers the download. A link
   opened that way never replaces a pairing: a paired phone has to unpair
   first.
3. The phone proves it read the code (Noise `IKpsk2`, the secret as the
   pre-shared key). A wrong or expired code fails the handshake. DBine allows
   at most 5 pairing attempts a minute.
4. DBine asks you, on the computer, whether to pair the phone, showing a
   code that the phone shows too. Nothing is kept until you say yes.
5. From then on each side knows the other's public key and accepts no one
   else.

## Every connection

- Noise `IK` (X25519, ChaCha20-Poly1305, BLAKE2s): mutual authentication by
  the paired keys, forward secrecy. DBine drops any key it didn't pair; the
  phone drops any computer whose key isn't the one it pinned.
- Removing a phone in Configuración › Dispositivos cuts its open sessions at
  once, and it can't connect again without a new QR code.
- A kill is never retried after a dropped connection.

## Local network and internet

| | Local network | Internet, own relay | Internet, public relays |
|---|---|---|---|
| Default | Yes (when the phone feature is on) | No | No |
| In the middle | Nobody | Your `iroh-relay` | iroh's public relays, run by n0 |
| What the middle sees | — | IP addresses, timing, sizes | IP addresses, timing, sizes |
| What the middle can't see or do | — | Read, change or forge anything | Read, change or forge anything |
| How it's turned on | Permitir celulares | Fuera de la red local › Servidor propio | Fuera de la red local › Servidores públicos, after a confirmation that says all of the above |

Over the internet the connection is iroh's QUIC (TLS 1.3, keys bound to each
endpoint), and the same Noise channel runs on top. iroh connects directly
when the networks allow it and only uses the relay otherwise.

## On the computer and the phone

- DBine's keys (`link-keys`, `link-iroh-key`) live in its encrypted vault,
  never in the state file or the logs.
- The paired phones (name, public key, permissions) live in `local.link`,
  this machine's state; they don't sync to the cloud backup.
- The phone keeps its own key pair and the computer's public key sealed
  (ChaCha20-Poly1305) with a file key that is itself sealed by an AES-256
  key held in the **Android Keystore**, which never leaves it. A copy of the
  app's files (a rooted phone, a backup) can't be opened without that
  phone's Keystore; if the Keystore loses its key, the pairing becomes
  unreadable and the phone has to pair again. The app is excluded from
  Android's backups.
- Every kill, cancel, task run and task cancel from a phone is logged on the
  computer with the phone that did it.
- The phone app checks for a newer version on GitHub's latest DBine release
  (api.github.com: GitHub sees the phone's IP, nothing else is sent) and only
  opens its download page in the browser; it never installs anything by
  itself, and only opens DBine's own release pages.

## Known limits

- With public relays, a third party (n0) sees both ends' IP addresses and
  when and how much traffic flows.
- While "Permitir celulares" is on, DBine listens on the local network (and,
  with internet access on, can be dialed through iroh by anyone who knows its
  id). Only paired phones get past the handshake; anything else is dropped
  before any request is read.
- The code hasn't had an independent audit yet. It's open source.

## Invariants (for anyone changing the code)

These hold in every change; a change that breaks one is a security bug:

1. The phone protocol (`crates/dbine-link/src/proto.rs`) has no request that
   runs free-form SQL or commands, returns table rows, or returns or accepts
   hosts, credentials, connection strings or settings.
2. Every request that changes something (`kill`, `cancel_query`,
   `run_task`, `cancel_task_run`) is refused unless that phone has the
   permission, and the phone confirms it with biometrics or PIN first. New
   ones follow the same rule.
3. Pairing needs the QR code's secret **and** the user's yes on the
   computer. No other path adds a phone.
4. DBine accepts only keys it paired; the phone talks only to the key it
   pinned. No fallback to an unauthenticated channel, ever.
5. Internet access is off by default. Turning on public relays shows the
   confirmation with what they see. No default may send traffic through a
   third party.
6. The Noise channel runs end to end on every path (local, own relay,
   public relay). A relay or transport is never trusted with plaintext.
7. Keys and secrets stay in the vault (computer) or sealed under the
   Keystore (phone): never in the clear on disk, nor in logs, state files,
   notices or error messages.
8. Any change to these files goes through the security scan before a
   release: `crates/dbine-link/`, `src-tauri/src/link.rs`,
   `mobile/src-tauri/`, and anything they call on the phone's behalf.
