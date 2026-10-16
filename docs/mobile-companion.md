# Mobile companion (design)

Status: **built**: on the local network, and from anywhere through iroh.
The security model, what travels and the invariants every change keeps:
[mobile-security.md](mobile-security.md). Agreed with the
owner on 2026-10-16. What's built is in [First version](#first-version); the
rest of this page is the design it grows into.

## First version

- **DBine:** Configuración › Dispositivos (`src-tauri/src/link.rs`,
  `web/src/components/DevicesSettings.vue`).
  - **Permitir celulares** (off by default) listens on port `27518` (it can be
    changed) on every interface.
  - **Vincular un celular** shows a QR code that lasts 2 minutes and pairs one
    phone. When the phone reads it, DBine asks: the name the phone gave and a
    code (its key's fingerprint) that the phone shows too. Nothing is kept
    until the user says yes. At most 5 pairing attempts a minute.
  - Each paired phone can be removed (its open sessions are cut at once) and
    allowed, one by one, to **end sessions** (off by default).
  - Kept in `local.link` (this machine only, not synced); DBine's key pair in
    the vault.
- **The channel** (`crates/dbine-link`): Noise `IKpsk2` to pair (the QR's
  secret is the PSK, DBine's public key is pinned), Noise `IK` afterwards
  (only keys DBine paired; the phone only talks to the key it pinned), frames
  of up to 64 KB, messages of up to 16 MB, JSON requests: `hello`,
  `connections`, `databases`, `monitor`, `locks`, `processes`, `kill`,
  `events`, `tasks`, `task_runs`, `run_task`, `health`, `queries`,
  `cancel_query`. The
  Monitor's labels come in Spanish, as the engines write them; the phone
  translates them with a slice of DBine's catalog that its build copies
  (`mobile/scripts/catalog.py`, the messages from the monitor files and the
  channel, about 100 KB a language).
- **The phone app** (`mobile/`, Tauri 2 + Vue, its own Cargo workspace, so
  desktop builds and CI never compile it): pairs by scanning the QR code, lists
  the computer's connections, shows a connection's Monitor (CPU, memory,
  connections, activity and the rest of the engine's figures, with bars
  against their ceiling and counters as a rate per second), its blocking
  chains and the statements running now (refreshed every 5 seconds), and
  ends a session
  after the phone's biometrics or PIN. A kill is never retried after a
  dropped connection. The texts are in DBine's five languages.
- **Actividad, notices and health (phone app):** two sections at the
  bottom, **Conexiones** and **Actividad**.
  - **Actividad › Consultas:** the queries running in DBine's editor, with
    how long they've been running (ticking), and **Cancelar** (the same
    per-phone permission as ending sessions, confirmed with biometrics or
    PIN); below, the ones that ran 10 seconds or more and ended, with their
    result (rows, or the error). Opening the app shows how a query ended
    even when its notification never arrived.
  - **Actividad › Tareas programadas:** each task with its last and next
    run, its last 10 runs with each step's summary and alerts, and
    **Ejecutar ahora**, allowed per phone (**Puede ejecutar tareas**, off by
    default) and confirmed with biometrics or PIN. A task with unapproved
    writing steps can't be run from the phone.
  - **Notices:** DBine keeps its last 100 in memory: a long query that ended
    ("Terminó en 3 min 20 s · 1240 filas", or the error) and each task run
    that ended, as the task's own **Avisar** says (on failure or alert by
    default; it reads the history every 15 seconds, so runs the OS scheduler
    made with DBine closed count too). The phone keeps its place in that list
    on disk (`news.json`), so a notice arrives once and none is lost when the
    app restarts; after DBine restarts, everything of its new run is new.
    - With the app open or in the background: every 20 seconds.
    - With the app closed: Android's periodic work (`NewsWorker.kt`,
      WorkManager), every 15 minutes, Android's minimum, when there's a
      network. It calls the same Rust code (`news_from`) through JNI. No
      push service is involved, so nothing goes through a third party.
  - **Salud:** a database's health check, the desktop's, on demand.
- **The computer stays awake** while an editor query has been running 30
  seconds or more (`src-tauri/src/awake.rs`): `caffeinate -i` on macOS,
  `systemd-inhibit` on Linux, `SetThreadExecutionState` on Windows. Only idle
  sleep is held off: the display still turns off, and closing the lid or
  choosing Sleep still sleeps. Released when no long query runs.
- **Outside the local network (iroh):** Configuración › Dispositivos ›
  **Fuera de la red local** chooses how phones reach DBine when they aren't
  on its network:
  - **Solo red local** (default): no iroh endpoint at all.
  - **iroh's public servers**, after a confirmation that says what they see:
    free, run by n0, best effort, no availability guarantee. iroh tries a direct connection first (hole
    punching, the router's port mapping, IPv6) and goes through a relay when
    the networks don't allow it; its address lookup finds DBine by its id.
  - **Own server:** the URL of an `iroh-relay` of one's own (https), one or
    more. Only that is used: no public relay, no lookup service. The phone
    dials DBine through it.

  DBine's iroh id (its own key, in the vault, `link-iroh-key`) and the own
  relays go in the QR code (`Invite.remote`); `hello` returns them again, so
  a phone follows a change of relays without pairing again. The phone tries
  the local addresses first (2 seconds each) and then iroh, and shows how it
  got there ("Red local" / "Por internet"). **What a relay sees:** encrypted
  QUIC packets; the channel's Noise handshake and frames run on top, with
  the QR code's keys, exactly as on the local network, so a relay (public or
  own) can't read or forge anything. `crates/dbine-link/src/net.rs`; the
  end-to-end check over the public relays is
  `cargo run -p dbine-link --features iroh --example remote` (a plain
  program: under the test harness iroh's network report doesn't complete on
  some machines).
- **Not yet:** alerts as native pushes, the phone's key in the
  Android Keystore / iOS Keychain (today it's in the app's private folder),
  finding DBine again when the computer's address changes (pair again), and
  iOS builds (only Android is built). The Drive-based design below stays
  as an alternative for a setup with no relay at all.

Build the APK (JDK 17, Android SDK platform and build-tools 36, NDK
27.2.12479018, the Rust target `aarch64-linux-android` and `tauri-cli`
2.12, since Gradle calls back `cargo tauri`). `mobile/` is its own cargo
workspace; its frontend build reads `web/src/locales`, so it needs the whole
repo:

```sh
cd mobile && npm ci
JAVA_HOME=… ANDROID_HOME=… NDK_HOME=… cargo tauri android build --debug --apk --target aarch64
```

**Releases.** Every release publishes the phone app with the desktop
installers (`.github/workflows/release.yml`, jobs `android` and
`android-upload`):

- `cargo tauri android build --apk --target aarch64 --config '{"version":"X.Y.Z"}'`
  from `mobile/` (release, arm64 only), with the tag's version: the APK's
  `versionName` is the tag's version, which the app's update check compares.
  Gradle leaves it unsigned at
  `mobile/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk`.
- It is signed outside Gradle: `zipalign -p 4`, then `apksigner sign` (v2 and
  v3) with the keystore from the secrets `ANDROID_KEYSTORE_BASE64`,
  `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS` (`dbine`) and
  `ANDROID_KEY_PASSWORD`, decoded into the runner's temp folder only for that
  step.
- `apksigner verify --print-certs` must show a single signer whose
  certificate SHA-256 is
  `12:C1:C2:51:1A:1F:B9:57:A6:39:4D:E7:2E:99:86:55:8C:04:0D:F1:1E:34:6F:6A:07:7B:87:02:9E:BC:83:42`
  (the one in dbine.com's `/.well-known/assetlinks.json`, which App Links
  check), and `aapt2 dump badging` must show `io.addlayer.dbine.mobile` with
  the tag's `versionName`; otherwise the job fails and the release stays a
  draft.
- The asset is `DBine-<version>-android.apk`, uploaded to the release's
  draft once the desktop builds created it. The app looks for an asset
  ending in `-android.apk` in GitHub's latest release, and
  `dbine.com/download/android` redirects to it. A manual run keeps it as the
  workflow artifact `dbine-android`.
- Changing the signing key breaks updates over installed apps (Android
  refuses an APK signed by another key) and the App Links: the fingerprint
  changes here, in `release.yml` and in dbine.com's Worker together.

A phone app, paired with DBine by scanning a QR code, that lets you look at
a server and act on it while you're away from the computer: the typical case
is a blocking alert that arrives while you're out and has to be answered in a
minute, without remote-desktop software.

## Principles

- **The connections never leave the computer.** The phone holds no hosts,
  users, passwords or connection strings. It asks DBine to do something;
  DBine does it with its own sessions and returns the result.
- **No AddLayer servers.** On the local network nothing is in the middle.
  Outside it, only if the user turns it on, a phone that can't reach DBine
  directly goes through a relay: their own `iroh-relay`, or **iroh's public
  relays, run by n0** (a third party, after a confirmation): they forward
  encrypted packets and can't read or forge them, but they do see the IP
  addresses of both ends and when and how much traffic flows. The default is
  **Solo red local**. (The original design used the user's Drive
  as the only intermediary; it stays below as an alternative.)
- **End-to-end encrypted.** Keys are exchanged through the QR code, so
  neither a relay, nor Google or Microsoft, nor anyone on the network can
  read or forge traffic.
- **Same rules as MCP.** Each paired phone is a client with its own access
  level per connection, the `prod` and read-only caps, and every action in
  the activity log ([mcp.md](mcp.md)).
- **Approved on the phone.** **Kill** and every other action that changes
  something are confirmed on the phone itself, with the device's biometrics
  or PIN, not in DBine: the point is acting when nobody is at the computer.
  The activity log records the phone that approved it.
- **It only works while DBine is open.** Closing DBine closes the door.

## What the phone does

First version, in this order:

1. **Locks:** the blocking tree of a connection and **Kill**, with
   confirmation (the [Locks](locks.md) panel, which already works on every
   engine that has locks).
2. **Processes and long-running statements** ([processes.md](processes.md)).
3. **Health check** results ([health-check.md](health-check.md)).
4. **Saved queries:** run one and see the result.
5. **Scheduled tasks:** last runs and their result.

Free-form query editing comes later, if ever: it isn't what a phone is for.

## Pairing

1. DBine: **Settings › Phone › Pair a phone** shows a QR code.
2. The phone scans it. The code carries:
   - DBine's device public key;
   - a one-time pairing secret that expires in 2 minutes;
   - DBine's local addresses and port;
   - the sync provider (Google or Microsoft).
3. If the phone isn't signed in yet, **Continue with Google / Microsoft**,
   with the same account as the sync. This is a single tap, done once.
4. Both sides prove possession of the pairing secret and store each other's
   public key. From then on, the secret is useless.

A paired phone appears in DBine with a name and can be revoked; revoking it
cuts any open session and stops it from connecting.

The phone app opens with the device's biometrics or PIN.

## Connecting

The phone and DBine don't need to know where the other one is. Each time the
phone opens a session, both sides try every path at once and keep the best
one that answers:

| Order | Path | When it works |
|---|---|---|
| 1 | **Same network**: the last known local address, or discovery on the LAN (mDNS) | Same Wi-Fi |
| 2 | **Direct over IPv6** | Both sides have IPv6 |
| 3 | **Direct through a port the home router opened** (UPnP / NAT-PMP / PCP) | The router allows it, which most home routers do |
| 4 | **Direct with simultaneous open** (hole punching) over IPv4 | Most home and office networks; often not on mobile carriers' shared IPv4 |
| 5 | **Drive mailbox**: requests and responses as encrypted files in the app's folder | Always, with a few seconds per round trip |

### Signaling through Drive

When the phone isn't on the same network, the two sides introduce themselves
through the app's private folder in the user's account: Google's
`appDataFolder` or OneDrive's `Apps/DBine`, the same scopes sync already asks
for (`drive.appdata`, `Files.ReadWrite.AppFolder`).

- The phone writes an **offer**: its candidate addresses, a session id and an
  epoch number, encrypted and signed with the paired keys.
- DBine reads it, answers with its own candidates, and both start trying
  paths 2 to 4 at the same moment.
- The records are short-lived and deleted after use.

How fast DBine notices an offer depends on how often it reads the folder:
about every 20 seconds while idle and every 2 seconds while a phone session
is open or reconnecting. Neither provider can notify a desktop app without a
public web endpoint, so DBine reads; the quotas leave plenty of room for this.

The provider's sync client (the Drive or OneDrive folder on disk) isn't
used: it can lag for minutes. Both sides use the provider's API.

### The channel

One protocol on every path: **QUIC** (the `quinn` crate), with each side's
certificate pinned to the public key from the pairing, not to any
certificate authority. The Drive mailbox carries the same messages, sealed
with XChaCha20-Poly1305 under the session keys, as cloud sync already does.

The phone app is built with Tauri 2, so the networking code, the crypto and
the message types are the same Rust code on both ends.

## Changing networks

Pair at home on Wi-Fi, walk out, the phone jumps to mobile data: the session
must survive it without the user doing anything.

- **The session isn't tied to a path.** Its identity is the paired keys and
  the session id, not an address. Changing paths doesn't mean logging in
  again or losing what was on screen.
- **QUIC migrates on its own** when only the phone's address changed and the
  new address can still reach DBine (for example, IPv6 on both networks): the
  connection carries on with the same keys, no new handshake.
- **When the old path dies:**
  - The phone hears about the network change from the system (iOS
    `NWPathMonitor`, Android `ConnectivityManager`) and doesn't wait for a
    timeout. Without that signal, three missed keepalives (one every 5
    seconds) mark the path as dead.
  - It immediately starts using the **Drive mailbox**, so requests keep
    flowing, only slower.
  - At the same time, it writes a new offer with a higher epoch. DBine
    answers with fresh candidates and both try paths 2 to 4 again.
  - When a direct path answers, traffic moves back to it. The mailbox stays
    as the fallback.
- **DBine can move too** (a laptop that changes networks): the same
  mechanism works the other way round, and DBine publishes its new
  candidates.
- **Nothing runs twice and nothing is lost:**
  - Every request has an id. DBine runs each id at most once and keeps its
    result for 10 minutes.
  - After reconnecting, the phone asks for the results of the ids it didn't
    receive, instead of sending them again.
  - This matters most for **Kill**: a reconnect never kills a second
    session, and the phone shows whether the kill it asked for happened.
- **The user sees the path:** "Same network", "Direct", "Through Drive
  (slower)" or "Reconnecting…", so a slower answer has an explanation.

## Alerts

Native push needs a server that holds AddLayer's Apple and Google signing
keys, which can't ship inside open-source code. Without AddLayer servers,
alerts go through channels the user already has:

- a **Watch** step for scheduled tasks (they already run with DBine closed)
  that checks rules per connection and raises an alert:
  - a session blocking others for more than N seconds;
  - a statement running for more than N minutes;
  - the connection not answering;
  - free space under a threshold;
  - a failed server job or backup;
  - replication lag over N seconds;
- delivered by **email** (the existing SMTP step), **Teams**, **Slack** or
  **Telegram**;
- once when the problem starts and once when it clears, never once a minute
  while it lasts;
- with a summary only: connection, rule, duration, sessions affected. The
  statement text is opt-in, because the message goes through that channel's
  servers.

Each rule exists only on the engines that have the capability, as with every
feature ([engine-support.md](engine-support.md)).

From the alert, the user opens the phone app, which connects as above.

## Order of work

1. The **Watch** step and its channels: useful on its own, with or without
   the phone.
2. The pairing and the channel in DBine, with the Drive mailbox first: it
   always works, and it's the fallback everything else relies on.
3. The phone app (iOS and Android) with locks and Kill.
4. The direct paths (LAN, IPv6, router port, hole punching) and network
   changes.
5. The rest of the views.

## To verify before building

- That an iOS/Android OAuth client in the same Google Cloud project shares
  `appDataFolder` with the desktop client, and the same for one Microsoft app
  registration across platforms.
- Drive and Graph API quotas for a 2-second read while a session is open.
- App Store and Play Store review for an app that controls a desktop app.
