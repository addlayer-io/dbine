# On-demand drivers

The DBine installer ships a single driver, SQLite. The others are downloaded
the first time someone connects to that engine. Whoever uses only SQL Server
downloads the SQL Server driver and nothing else.

A downloaded driver behaves the same as a bundled one: the same features, the
same errors, the same cancellation and the same row streaming. The parity
tests check it (`crates/dbine-plugin-host/tests/parity.rs`).

## What the user sees

- **First connection to an engine:** the connection node shows "Downloading
  the SQL Server driver, just this once… 45 %" instead of "Connecting…". It is
  the same notice as the one for DuckDB.
- **The following ones:** the driver is already on disk and starts right away.
- **Settings › Drivers:** the list of downloadable drivers. Each one can be
  downloaded before using it (for example, to work offline) or deleted.
  "Download all" downloads them one by one. The section appears only in
  builds with downloadable drivers.
- **Updates:** installed drivers update themselves in the background. Each
  row shows the version in use and, when it applies, a status ("Downloading
  version X…", "Version X is used from the next connection", "A newer version
  needs DBine X or later", "Version X failed and it went back to Y"). "Check
  for updates" queries the index right away and "Roll back" drops the
  current version and goes back to the previous one.
- **Offline:** the `DBINE_DRIVERS_DIR` variable points to a folder that
  already has the drivers (`dbine-driver-<crate>[.exe]`), and then nothing is
  downloaded or updated. More in [Environment variables](#environment-variables).

## How it works

Each crate in `crates/drivers/` is compiled as a separate program, the
*host* (`crates/dbine-plugin-host`, with that crate's feature). The app
starts it as a child process and talks to it over stdin/stdout.

| Piece | Where | What it does |
|---|---|---|
| Protocol | `crates/dbine-plugin/src/proto.rs` | Length-prefixed (u32) frames + MessagePack. Handshake with protocol and app version. Calls multiplexed by id. |
| Host | `crates/dbine-plugin/src/host.rs` | Serves the calls with the real drivers. Sessions live in the host. Sends rows to the sink one at a time, with backpressure. |
| Proxy | `crates/dbine-plugin/src/remote.rs` | `RemoteDriver` and `RemoteSession` implement `Driver` and `Session`, forwarding each method to the host. |
| Download | `crates/dbine-plugin/src/install.rs` | Downloads the host from the release, resumes it if interrupted, verifies the SHA-256 and installs it with an atomic rename. |
| Catalog | `crates/dbine-drivers` (feature `plugins`) | The list of downloadable drivers the app carries inside, with each one's `DriverInfo`, the file, the size and the SHA-256 of its floor version. |
| Index | `crates/dbine-plugin/src/index.rs`, `state.rs`, `updater.rs` | Verifies the signed index, picks each driver's version, stores the local state and downloads the updates. |

Details:

- **The app knows every driver without downloading them.** The catalog
  carries what each driver says about itself (connection fields,
  capabilities, templates, etc.), so the connection form, the explorer and
  the menus work before the download.
- **One host per crate, not per engine.** The `sqlserver` crate serves SQL
  Server, Azure SQL and Fabric with a single process and a single download.
- **The host starts with the first connection** and is reused for the
  following ones. It dies when the app closes, because its standard input is
  closed.
- **If the host crashes**, open sessions return a connection error. The next
  connection starts a new host. If a profiler was active at that moment, the
  setting the profiler changed on the server stays as it was, just as if the
  network were cut.
- **Versions:** each driver has its own version, separate from the app's (see
  [Driver versions](#driver-versions)). Hosts are stored in
  `<app data>/components/drivers/`, with the version in the name. An app
  update that does not change a driver does not download it again. On startup,
  the versions that are not the floor, the active one or the previous one are
  deleted; the previous one is kept on purpose, so you can go back.
- **When the app closes**, each host exits by itself when its standard input
  is closed, even if the app crashed.
- **Security:** the size and the SHA-256 of the floor are fixed inside the
  app; those of the newer versions come from the index, which is signed with
  the updater key. A file that does not match is deleted and not run.
  Passwords travel to the host over the pipe, never through arguments or
  environment variables.
- **Performance:** queries run in the host with the same code and the same
  optimization as before. The only thing added is passing the rows over the
  pipe, which is negligible next to the network and the database.

## Development

`cargo tauri dev` and `cargo test --workspace` compile all the drivers inside
the app, as always. Nothing needs to be downloaded.

To try the app with downloadable drivers locally:

```bash
scripts/build-driver-hosts.py aarch64-apple-darwin /tmp/hosts http://127.0.0.1:8000
(cd /tmp/hosts && python3 -m http.server 8000) &
DBINE_PLUGIN_CATALOG=/tmp/hosts/plugins.json cargo tauri dev --features plugins
```

For Windows from macOS: `CARGO_BUILD="cargo xwin build"` in front of the
script.

## Driver versions

The app ships often; a driver may not change in years, or be fixed several
times between two app versions. That is why each driver has its own version
and is published on its own, without an app release.

- **The version** is the `version` in `crates/drivers/<crate>/Cargo.toml`. It
  is bumped by hand when the driver changes.
- **The published id** is `<version>+p<protocol>.e<epoch>`, for example
  `1.2.0+p1.e1`:
  - `protocol` is `PROTOCOL` in `crates/dbine-plugin/src/proto.rs`. It goes up
    only if an existing message changes, and then all the drivers are
    republished. Adding a new operation does not bump it: a driver that does
    not know it answers "not supported".
  - `epoch` is `drivers-epoch` in `crates/dbine-plugin-host/Cargo.toml`. It
    is bumped by hand to republish all the drivers with the same versions,
    for example for a security fix in a dependency they share (rustls, tokio)
    or a change in `dbine-driver` that all of them must have.
- **Where:** in the permanent `drivers` release of the repo. Each platform
  has its signed index (`index-<target>.json` and `.sig`) with all the
  published versions.
- **The floor:** the catalog the app carries inside (`plugins.json`) fixes,
  per driver, the version the app was built with. It is the floor: that
  version can always be downloaded and the app never uses an older one. The
  catalog no longer decides the version that runs, only the minimum.
- **What the app shows** (connection fields, capabilities) comes from the
  version of the driver in use, taken from the index cached at startup. That
  is why new fields in `DriverMeta`, `DriverInfo` or `Capabilities` carry
  `#[serde(default)]`: the app has to be able to read what previously
  published drivers said, and the drivers have to accept the configuration
  from an older manifest.

### How the app chooses

For each driver, the app takes, among the versions in the index, the highest
one (semver) that meets all of this (`index::resolve`):

1. It has the same protocol and the same epoch as the floor. A driver with
   another protocol or epoch cannot be used with this app.
2. It is not older than the floor.
3. It is not withdrawn (`yanked`).
4. It has not failed before on this machine (see [If a version fails](#if-a-version-fails)).
5. Its `min_app` is less than or equal to the app version.

If nothing qualifies, or there is no index, it uses the floor. If there is a
newer version that the app cannot run because of its `min_app`, the driver
stays on the current version and Settings › Drivers says "A newer version
needs DBine X or later": the app has to be updated.

### How it updates

1. **Signed index.** The app downloads `index-<target>.json` and its `.sig`
   about 10 seconds after starting, every 6 hours and with "Check for
   updates" in Settings › Drivers. It verifies the signature (minisign) with
   the app updater's public key (`src-tauri/tauri.conf.json`) and stores it
   in `components/drivers/`. A connection never waits for this check.
2. **No replaying the old.** If the index `seq` is lower than the last
   accepted one, it is rejected (a replayed old index does not make you go
   back). Without internet, the cached index is used if its signature
   verifies; otherwise, the floor.
3. **Background download.** Only for drivers that are already installed: it
   downloads the file (with resume, SHA-256 and atomic rename), and when done
   it leaves it as the active version and keeps the previous one. A driver
   that was never used is not downloaded ahead of time: the first connection
   directly downloads the chosen version, and if that fails, the floor.
4. **New host for new connections.** The file carries the version in its name
   and does not overwrite the one that is running. The next connection starts
   a new host; open sessions (and an active profiler) stay on the old one
   until they close. Settings › Drivers notifies "Version X is used from the
   next connection". Native copy between two sessions of different hosts is
   not supported; the transfer falls back to the generic copy.
5. **New options.** Connection fields are loaded once, at startup. If the
   active version brings options that the session did not load, the screen
   asks to restart DBine to see them.

With `DBINE_DRIVERS_DIR` there is no check and no updates: only the floor,
from that folder.

### If a version fails

A version is marked as bad on this machine, and is not chosen again, if the
host does not start, exits before the handshake, takes more than 10 seconds
in the handshake or says it is a version other than the expected one. The app
deletes that file, goes back to the previous version (or the floor) and
retries the connection once. The screen shows "Version X failed and it went
back to Y". The floor is never marked or deleted: it is the last resort.

**Roll back** (Settings › Drivers) does the same by hand, with the reason
`user`. After confirming, the current version is not used again and open
connections stay on it until they close. It is not offered if the driver is
already on the floor version.

The local state (active version, previous, bad versions and the last `seq`)
is in `components/drivers/state.json`. On startup, the hosts that are not the
floor, the active one or the previous one are deleted.

### Index fields

`index-<target>.json`:

```json
{ "target": "aarch64-apple-darwin", "schema": 2, "seq": 1760000000,
  "drivers": { "postgres": { "0.1.10+p1.e3": {
      "file": "…gz", "size": 0, "sha256": "…", "own_hash": "…",
      "shared_hash": "…", "manifest": [],
      "min_app": "0.1.9", "yanked": null, "published_at": "…Z" } } } }
```

| Field | What it is |
|---|---|
| `schema` | Version of the index format. Today `2`. |
| `seq` | A number that grows with each publication (Unix time). The app rejects an index with a `seq` lower than the last accepted one. |
| `min_app` | The oldest app that runs that host. Without the field, any app. |
| `yanked` | The reason that version was withdrawn; `null` if not. A withdrawn version is not chosen. Without the field, it is not withdrawn. |
| `published_at` | When it was published (UTC). |

`manifest` is what the drivers say about themselves (`DriverMeta`). Apps
older than this schema do not read the index: they use only their catalog, so
the new fields do not affect them.

## Publishing a driver

To publish a driver without an app release:

1. Bump the `version` in `crates/drivers/<crate>/Cargo.toml` and merge.
2. Create and push the tag `driver-<pkg>-v<version>`, for example
   `driver-postgres-v0.1.10`. The tag must state the same version as the
   `Cargo.toml`, or the workflow fails.

`.github/workflows/drivers.yml` builds that driver on the four platforms (the
same runners and the same setup as the app release, in
`.github/actions/driver-hosts`). Only when all four built, it uploads the
files to the `drivers` release and then merges its entries into the published
index, signs it and uploads `index-<target>.json` and its `.sig`. At the end
it prunes (see [Pruning](#pruning)). App releases and driver releases do not
overwrite each other: each one re-reads the published index before merging,
and each platform's publishing job runs one at a time.

**Trial without publishing:** Actions › drivers › Run workflow, with the
driver and `dry_run` on (it is the default). It builds the driver from the
chosen branch, runs the checks and leaves the hosts as workflow artifacts.
With `dry_run` off, it publishes.

**Checks** (`scripts/build-driver-hosts.py <target> <out> <url> <index> --only <pkg> --no-catalog`):

- If the driver's code changed and its version did not, it fails and says
  which one to bump. If code shared with other drivers changed, it only
  warns.
- The new version must be higher than the highest published one with the same
  protocol and epoch.
- The manifest's engine ids must include all those of the previous version:
  installed apps switch to the new one on their own and an engine cannot
  disappear.
- A published app that can run it must exist (see `min_app`). If not, it
  fails: publish the app first.
- A problem in another driver only warns; in the app release, it fails.

The index signature uses the secrets `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, which are only passed through the
step's `env:`. If the first one is missing, publishing fails.

### How `min_app` is computed

A new host only runs with apps that speak the same way over the pipe. That is
summarized in the `wire_hash`: a hash of the code in `crates/dbine-plugin/src`
and `crates/dbine-driver/src` (without `tests` or `testdata`).
`scripts/driver_index.py` computes it for the current code and for each tag
of the last five app versions (`vX.Y.Z`).

- `min_app` is the oldest app among those five whose `wire_hash` equals that
  of the current code.
- In an app release, the version being published counts as a floor, so there
  is always an answer.
- In a driver-only release, if none of the five matches, it fails with
  "publish the app first".
- A driver can raise that minimum by hand, never lower it, in its
  `Cargo.toml`:

```toml
[package.metadata.dbine]
min-app = "0.2.0"
```

To see it locally: `scripts/driver_index.py wire-hash` and
`scripts/driver_index.py min-app`.

## Release

`.github/workflows/release.yml` runs, on each platform:

1. Downloads the published index (`index-<target>.json` from the `drivers`
   release).
2. `scripts/build-driver-hosts.py <target> driver-hosts <drivers url> <index>`
   builds only the drivers whose id is not published, verifies the ones that
   are reused, writes the new index and `plugins.json` (the floor), and
   checks that the app can read it (`dbine-plugin-host --check-catalog`). It
   gives each new host its `min_app`.
3. On tags, it uploads the new drivers to the `drivers` release, before
   building the app, and then merges the index (`scripts/driver_index.py
   publish`: reads the one published at that moment, adds the new entries,
   signs it and uploads the index with its `.sig`). That way a published app
   never points to a driver that is not there.
4. Builds the app with `--features plugins` and
   `DBINE_PLUGIN_CATALOG=driver-hosts/plugins.json`. Without the variable,
   the app still compiles but does not offer downloadable drivers (and cargo
   warns).
5. When the four platforms finished, it prunes the `drivers` release.

In manual runs, the new drivers are left as an artifact and are not
published.

### Pruning

A GitHub release admits up to 1000 files, so `scripts/prune-driver-assets.py`
deletes those that are no longer needed. The union of these is kept:

- **(a)** the hosts named by the catalogs of the last five app versions. The
  names are built from each tag's sources, the same way as
  `build-driver-hosts.py`;
- **(b)** the newest non-withdrawn version of each driver (by protocol, epoch
  and platform);
- **(c)** what each of those five versions switches to on its own, applying
  the app's rule with that tag's version and catalog against the current
  index.

Everything else is deleted, including withdrawn versions that are not in (a),
and leaves its index, which is signed and uploaded again. A host that is not
in the index and was uploaded less than a day ago is not touched: it is a
driver release that has not published its index yet.

**Fail-safe:** if not all the files the newest app version expects are
there, or a platform's index is missing, it deletes nothing. With `--dry-run`
it shows what it would delete. An app older than the last five keeps the
drivers it has, but does not download new ones of those already pruned: it
has to be updated.

It runs after an app release and after a driver release, one at a time.

## Environment variables

| Variable | What it does |
|---|---|
| `DBINE_DRIVERS_DIR` | Folder with the already downloaded drivers (`dbine-driver-<crate>[.exe]`). Nothing is downloaded or checked: only the floor. |
| `DBINE_DRIVERS_INDEX_URL` | Another index (a mirror, local tests); the driver files are looked up next to it. It is verified with the signature just like the official one. |
| `DBINE_DRIVERS_PUBKEY` | Another public key to verify the index. Only in debug builds (with a test key); ignored in release. |

## Adding or changing a driver

Nothing special: a new crate in `crates/drivers/` and its feature in
`crates/dbine-drivers` and in `crates/dbine-plugin-host/Cargo.toml` (a line
`<crate> = ["dbine-drivers/<crate>"]`, which also has to be in `default`).
The release script picks it up from there.

For a driver to ship in the installer, add it to `BUILT_IN` in
`crates/dbine-drivers/src/lib.rs`.

A new method in the contract (`Driver` or `Session`) needs its variant in
`Call` and `Reply` (`proto.rs`), its handling in `host.rs` and its forwarding
in `remote.rs`. Drivers published earlier answer "not supported" until their
version is bumped. When changing a message that already exists, `PROTOCOL` is
bumped.
