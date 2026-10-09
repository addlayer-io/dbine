# Updates

DBine checks by itself whether there is a new version and, where it can,
updates itself: it downloads the new version, verifies its signature and
restarts to finish. Where it cannot, it offers the release page.

## What the user sees

- **When opening DBine**, a few seconds after startup, a single window (the
  first one that starts) asks whether there is a new version. If it fails, for
  example with no network, it shows nothing. A version marked with "Skip this
  version" is not offered again on its own.
- **By hand**, from **Help › Check for updates…** or **Settings › General ›
  Check for updates**: it always answers. If there is nothing new, it says
  "You're on the latest version".
- **The notice** shows the new version, the installed one and the version
  notes, with three buttons: "Skip this version", "Later" and "Update now".
- **Update now** downloads the package with a progress bar (the MB downloaded
  and the total), verifies the signature and ends in "DBine X is ready". The
  download can be cancelled.
- **Restart to finish** goes through the same control as Quit: if there are
  background runs in any window, it asks first and cancels them ("Cancel and
  restart"). Then it installs and opens the new version. With "Later", the
  downloaded update stays ready until DBine is closed: the next "Check for
  updates" offers to restart without downloading again.
- **If something fails** (the download, the signature or the installation),
  the notice says what happened and offers "Open the download page".
- **Several windows:** only one shows the update, the one that found it. If
  you search from another window while it downloads, it says which window it
  is in. If that window is closed, the download continues and another window
  can finish it.

## What happens on each system

| System | Update package | How it is installed |
|---|---|---|
| macOS (Apple Silicon and Intel) | `DBine_X_aarch64.app.tar.gz`, `DBine_X_x64.app.tar.gz` | replaces `DBine.app` in place. If the folder cannot be written, macOS asks for an administrator's password. |
| Windows | `DBine_X_x64-setup.exe` (NSIS) | DBine closes, the installer runs in passive mode (only a progress bar, no questions) and opens the new version. |
| Linux (AppImage) | `DBine_X_amd64.AppImage` | rewrites the AppImage file in place and restarts. |

Cases where it does not update itself and offers the release page:

- **Linux with `.deb` or `.rpm`:** the files belong to the system's package
  manager, not to DBine. The notice explains it: "You installed DBine with a
  system package: download the new version from its release page". It is
  detected because the `APPIMAGE` variable, which every AppImage defines when
  running, is missing.
- **macOS opened from the DMG or without moving to Applications** (macOS runs
  it from a temporary copy, "AppTranslocation"): the notice suggests moving it
  to Applications.
- **Windows installed with the `.msi`:** the manifest only carries the NSIS
  installer.
- **Versions without a manifest:** up to 0.1.3 `latest.json` was not
  published. It also does not exist in the time between a version being
  created and its manifest being uploaded, nor for a platform that has not
  been uploaded yet. In those cases, and on any error reading it, DBine
  queries the latest version on GitHub as before and offers its page.

## How it works

- The `tauri-plugin-updater` plugin (pinned to 2.12: 2.13 requires tauri
  2.12) reads
  `https://github.com/addlayer-io/dbine/releases/latest/download/latest.json`.
- The backend handles everything (`src-tauri/src/commands/updates.rs`): the
  check, the download and the installation. The webview has no plugin
  permissions, so it cannot ask for anything to be installed on its own.
- The signature is minisign (Ed25519). The public key is in
  `plugins.updater.pubkey` of `src-tauri/tauri.conf.json`. With
  `requireSignedVersion`, each signature carries the version it was signed for
  (`version:X` in the trusted comment): a tampered manifest cannot offer an old
  package as if it were new.
- The downloaded package stays in memory until the restart (between 20 and 90
  MB depending on the system).
- No telemetry is added: each version's adoption is already visible in
  `app_started`, which carries the version ([`telemetry.md`](telemetry.md)).

### Commands

| Command | Args | Response |
|---|---|---|
| `check_for_update` | `{ manual }` | `UpdateInfo`: `current`, `latest`, `available`, `url` (the release page), `notes`, `published_at`, `installable`, `reason` (`unsigned`, `location`, `package`, `no_manifest`), `phase` (`idle`, `downloading`, `ready`) and `owner` (the window that carries the update) |
| `update_download` | `{}` | downloads and verifies; progress reaches the owner window as the `update-progress` event (`phase`, `downloaded`, `total`). Cancelled: error `cancelled` |
| `update_cancel` | — | cuts the download |
| `update_install_and_restart` | — | installs and restarts; the UI calls it only from the quit control (`requestUpdateRestart` in `quitGuard.ts`) |
| `open_release_page` | `{ url }` | opens the page in the browser; only DBine release pages |

If another window becomes the owner, the previous one receives
`update-owner-changed` and closes its notice. When the download finishes, the
owner receives `update-finished` (`ok`, `cancelled`, `error`): this also ends
the notice of a window that took over a download started by another, already
closed.

## The signing key

- **It is generated only once**, on the owner's machine:
  `cargo tauri signer generate -w ~/.tauri/dbine-updater.key`. It leaves the
  private key (`dbine-updater.key`, with a password) and the public key
  (`dbine-updater.key.pub`).
- **The public key** goes, as is, in `plugins.updater.pubkey` of
  `src-tauri/tauri.conf.json`. A test (`cargo test -p dbine updates`) fails if
  the `DBINE_UPDATER_PUBKEY_PLACEHOLDER` placeholder was left.
- **The private key never enters the repo, the logs or a message.** It must be
  **backed up** in a safe place together with its password: if it is lost,
  existing installs can no longer update themselves (they verify against the
  public key they carry) and each user would have to be asked to install by
  hand a version with a new key.
- A build with the placeholder never tries to install (it offers the page) and
  `scripts/make-latest-json.py` refuses to publish with it. A version that
  ships with the placeholder can never update itself.

## Release builds

- **`createUpdaterArtifacts` stays `false` in `tauri.conf.json`.** With
  `true`, any `cargo tauri build` without the private key fails, including
  development and test ones. Release builds add
  `--config src-tauri/tauri.updater.conf.json`, which enables it.
- **Variables**, in the `.env` at the repo root (outside git) or in the
  environment. They are never printed:
  - `TAURI_SIGNING_PRIVATE_KEY`: the **absolute path** to the key (the CLI does
    not expand `~` and, if it does not find the file, takes the text as if it
    were the key and fails with "failed to decode secret key"), or its
    content.
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: always defined, even if empty. If it
    is missing, the CLI asks for it on the keyboard and the build is left
    waiting.
- `scripts/check-updater-signing.sh` checks both and the public key without
  showing any value. It is worth running before each build.
- **What each build leaves** (besides the usual installers):

| Platform | Command | Package and signature |
|---|---|---|
| macOS | `cargo tauri build --target aarch64-apple-darwin --features plugins --config src-tauri/tauri.updater.conf.json` (same with `x86_64-apple-darwin`) | `target/<triple>/release/bundle/macos/DBine.app.tar.gz` and `.sig` |
| Windows (from macOS) | `cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --features plugins --bundles nsis --config src-tauri/tauri.updater.conf.json`, with the LLVM environment from `scripts/build-windows-from-mac.sh` | `target/x86_64-pc-windows-msvc/release/bundle/nsis/DBine_X_x64-setup.exe` and `.sig` |
| Linux (Docker) | `scripts/linux-release-build.sh` inside the container, with the key mounted at `/run/secrets/dbine-updater.key` (the script's header has the `docker run`) | `linux-out/DBine_X_amd64.AppImage` and `.sig` |

- On Windows, check that the log does not say "Failed to add bundler type":
  without that mark the app does not know NSIS installed it and offers the
  page instead of updating (it fails on the safe side, but it must be fixed).
- The CLI also signs the `.deb` and the `.rpm`. Those `.sig` files are not
  uploaded.

## Publishing the manifest

`scripts/make-latest-json.py` builds `latest.json`:

```
python3 scripts/make-latest-json.py --version X --out dist/latest.json \
  --merge prev/latest.json \
  --artifact darwin-aarch64-app=target/aarch64-apple-darwin/release/bundle/macos/DBine.app.tar.gz \
  --check-uploaded
```

- It verifies each `.sig` against the conf's public key (key id, file
  signature, trusted comment signature and `version:X`) before writing
  anything.
- It copies each package and its `.sig` to `dist/` with the version's asset
  name (the two macOS `.app.tar.gz` have the same name coming out of the
  build) and lists what was copied in `dist/upload-files.txt`.
- The platform keys always carry the installer: `darwin-aarch64-app`,
  `darwin-x86_64-app`, `windows-x86_64-nsis` and `linux-x86_64-appimage`. That
  way a `.deb`, an `.rpm` or an `.msi` never takes a package of another type.
- `--merge` keeps the platforms that were already published for the same
  version, and their date. `--check-uploaded` requires each package to already
  be in the GitHub release, with the same size.
- The notes come from `--notes-file` or from the version's text on GitHub.
- Tests: `python3 scripts/test_make_latest_json.py`.

**Upload order, per platform** (each upload, with the owner's OK):

1. `gh release upload vX <usual installer> dist/<package> dist/<package>.sig`
2. `gh release download vX -p latest.json -D prev` (the first time it does not
   exist).
3. `make-latest-json.py --merge prev/latest.json --artifact … --check-uploaded`
4. `gh release upload vX dist/latest.json --clobber`

`latest.json` always goes last: while a platform is missing, that platform
keeps offering the page. The new version must end up as "latest" on GitHub
(the `drivers` one never is).

**Through CI** (`.github/workflows/release.yml`), instead of the local path: it
uses the secrets `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` and `tauri-action` builds its own
`latest.json` (`includeUpdaterJson`). Its platform keys are the generic ones
(`darwin-aarch64`, `windows-x86_64`, `linux-x86_64`…) besides those that carry
the installer; the app still checks how it was installed before installing (a
`.deb`, `.rpm` or `.msi` offers the page), so both manifests are valid.

A version is published through one path or the other, never both: if they
were mixed, CI would replace the hand-uploaded packages and the local
`latest.json` would be left with signatures of other files. The workflow avoids
it by itself: the `route` job checks whether the version already exists on
GitHub when it starts. If it exists, the local path created it (`gh release
create vX` creates the tag and triggers the workflow), and CI builds without
publishing anything (neither the app, nor the drivers, nor `latest.json`); it
leaves the installers as workflow artifacts. If it does not exist, CI
publishes it end to end.

## Tests

In development builds (`debug_assertions`), and only in them:

- `DBINE_UPDATE_ENDPOINT` replaces the manifest address. It accepts `http://`
  (the plugin warns on the console), so a local `python3 -m http.server`
  works.
- `DBINE_UPDATE_PUBKEY` replaces the public key, to sign with a test key
  (`cargo tauri signer generate --ci -p "" -w <file>`).

An end-to-end test on macOS:

1. Two `cargo tauri build --debug --bundles app` builds with another
   `identifier` (for example `com.addlayer.dbine.updtest`): one with
   `"version": "0.1.3"` and another with the new version and
   `--config src-tauri/tauri.updater.conf.json`, signed with the test key.
2. Copy the old one to a temporary folder (not to `/Applications`) and serve
   `latest.json` and the new `.app.tar.gz` with `python3 -m http.server`.
3. Open it with a temporary `HOME` and `DBINE_UPDATE_ENDPOINT` pointing to the
   server: notice, progress bar, "ready", the quit control with a task
   running, restart and the new version in Settings.
4. Without the server: the manifest is not there and DBine offers the page.

Windows (passive install and restart) is tested on a Windows machine or VM.
