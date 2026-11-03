# Updates

DBine checks by itself whether there is a new version and, where it can,
updates itself the way VS Code does: it downloads the new version in the
background, verifies its signature, and installs it when DBine restarts or
quits. Where it cannot, it shows a notice with the release page.

## What the user sees

- **Checks:** a few seconds after DBine opens (only the first window that
  starts) and then every 6 hours while it runs. Every window runs the 6-hour
  timer, and the backend lets only one check per period reach the network.
  A check that fails (no network, rate-limited) shows nothing.
- **Where DBine updates itself** (`installable`, see below): a new version
  downloads and verifies in the background, with no dialog. If an even newer
  version comes out while one is downloaded and waiting, the newer one
  replaces it. There is no "Skip this version" here.
- **Ready:** once the package is downloaded and its signature verified, the
  status bar of every window shows **Restart to update**, with the version in
  its tooltip. Clicking it goes through the same control as Quit: if there
  are background runs in any window, it asks first and cancels them ("Cancel
  and restart"), and unsaved project files are offered for saving. Then it
  installs and opens the new version. There is no modal.
- **Quitting** normally with an update ready installs it as part of the quit,
  after the quit control, without opening DBine again: the next launch is the
  new version. If the install fails it is logged (`app.log`), DBine quits
  anyway, and the next run checks and downloads again.
- **After an update**, the first launch of the new version shows what's new:
  the changelog sections between the version that ran before and the new one
  (at most five), in the interface language when the translation has them
  and in English otherwise. If they can't be read (offline), the next launch
  tries again. A first install shows nothing.
- **By hand**, from **Help › Check for updates…** or **Settings › General ›
  Check for updates**: it always answers. If there is nothing new, it says
  "You're on the latest version". If an update is already downloaded, the
  dialog says it is ready and offers **Restart to finish**. If one is found,
  the dialog shows its download with a progress bar (cancellable) and ends in
  "DBine X is ready". Only the manual check says it couldn't reach the update
  server.
- **Where DBine doesn't update itself** (a `.deb`/`.rpm` or `.msi` install,
  macOS outside Applications, a build without the updater key, a version
  without a manifest), the checks are the same, and a new version shows the
  notice: the new version, the installed one, the version notes, the reason,
  and "Skip this version", "Later" and "Download" (the release page). It is
  shown once per version per run; a skipped version is not offered again on
  its own.
- **If something fails** in a download the user is watching, or in the
  install from "Restart to update", the dialog says what happened and offers
  "Open the download page".
- **Several windows:** only one drives the download, the one whose check
  found it. If you search from another window while it downloads, it says
  which window it is in. If that window is closed, the download continues and
  another window can finish it. The status bar button shows in every window.

## Managed machines: `DBINE_AUTO_UPDATE`

For installations managed by IT, the environment variable
`DBINE_AUTO_UPDATE=0` (or `false`, `off`, `no`, in any case) turns off the
background download and the install on quit. DBine still checks at launch
and every 6 hours, and a new version shows the notice instead ("Skip this
version", "Later", "Update now"): nothing is downloaded unless the user
chooses to. Any other value, or the variable missing, leaves automatic
updates on. There is no switch in the interface.

## What happens on each system

| System | Update package | How it is installed |
|---|---|---|
| macOS (Apple Silicon and Intel) | `DBine_X_aarch64.app.tar.gz`, `DBine_X_x64.app.tar.gz` | replaces `DBine.app` in place (one atomic swap on APFS, so the app is never missing). **Restart to update:** if the folder cannot be written, macOS asks for an administrator's password, then the new version opens. **On quit:** the windows hide, the bundle is swapped and DBine exits. When the swap would need an administrator's password (the folder isn't writable, or the bundle belongs to another user) the quit doesn't wait on a prompt: it skips the install, and the restart button or the next run updates. |
| Windows | `DBine_X_x64-setup.exe` (NSIS) | DBine runs its exit cleanup, starts the installer in passive mode (only a progress bar, no questions) and ends; the installer closes what is left of DBine. **Restart to update:** the installer opens the new version (`/R`). **On quit:** it doesn't (no `/R`). |
| Linux (AppImage) | `DBine_X_amd64.AppImage` | rewrites the AppImage file in place. **Restart to update:** then restarts. **On quit:** just exits. |

The Flathub build (feature `flatpak`, [`flathub.md`](flathub.md)) doesn't
check at all, neither at launch nor every 6 hours, and never downloads or
installs on quit: Flatpak updates it. A manual check answers `reason: "flatpak"`
without touching the network and the UI says where updates come from.

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

## When the app identifier changes

The identifier changed from `com.addlayer.dbine` to `io.addlayer.dbine`
(what is copied, and how: [identifier-migration.md](identifier-migration.md)).
The old data stays untouched, so going back to an older version loses
nothing. The update itself is not affected:

- **macOS:** the updater replaces `DBine.app` in place after checking the
  package's minisign signature; it never compares bundle identifiers. The
  relaunched app has the new bundle identifier and copies the folders (APFS
  clones, no extra space) before its window opens. What macOS keys on the bundle identifier or the code
  signature may ask once more:
  - **Keychain:** the item `com.addlayer.dbine` was created by the old app, so
    reading it asks for access once, as after every update while DBine is
    signed ad hoc (its code requirement changes with each build). DBine then
    writes a copy under `io.addlayer.dbine`, created by the new app, and
    leaves the old one for older versions. A denied prompt writes nothing:
    the next start asks again.
  - **Privacy permissions (TCC)** are kept per bundle identifier: local
    network access, and the Documents, Desktop or Downloads folders, may be
    asked again. Full Disk Access, if the user granted it by hand, has to be
    granted again for the new entry. Notifications don't change: DBine shows
    them through `osascript`.
  - Re-registering the scheduled tasks under the new label may show the
    system's "Background item added" notice.
- **Windows:** the NSIS installer keys on the product name
  (`Software\Microsoft\Windows\CurrentVersion\Uninstall\DBine`, installed in
  `…\DBine`), and the MSI upgrade code comes from the product name too, so
  the update installs in place. The AppUserModelID changes, which only
  re-groups the taskbar button (an older pin may need pinning again). The
  single-instance lock is named after the identifier, so the old and new
  versions don't see each other; the installer closes the old one before it
  runs, so they never overlap.
- **Linux:** the AppImage is rewritten in place; the `.deb`/`.rpm` package
  name and the `.desktop` file come from the product name. The
  single-instance D-Bus name follows the identifier, as on Windows.

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
- The downloaded package stays in memory until the restart or the quit
  (between 20 and 90 MB depending on the system). It is only offered or
  installed after the plugin verified its signature: a package that fails
  verification is dropped and the next check downloads again.
- The install on quit (`install_on_quit`) runs from `quit_app`, which the UI
  calls only after the quit control, and from `RunEvent::Exit` (an OS quit,
  such as a logout, that needed no prompt). The first call takes the
  package, so it installs once. The installable builds read only
  `latest.json`: the GitHub API is asked only by the builds that can't update
  themselves, at most once per period.
- No telemetry is added: each version's adoption is already visible in
  `app_started`, which carries the version ([`telemetry.md`](telemetry.md)).

### Commands

| Command | Args | Response |
|---|---|---|
| `check_for_update` | `{ manual, lang, periodic }` | `UpdateInfo`: `current`, `latest`, `available`, `url` (the release page), `notes`, `published_at`, `installable`, `auto` (installable and `DBINE_AUTO_UPDATE` not off), `reason` (`unsigned`, `location`, `package`, `no_manifest`, `flatpak`), `phase` (`idle`, `downloading`, `ready`) and `owner` (the window that carries the update). `periodic`: answered from memory when another background check went out less than 5 h 50 min ago |
| `update_state` | — | the downloaded update waiting for the restart (`UpdateInfo`), or `null` |
| `whats_new` | `{ since, lang }` | `{ previous, current, notes }`: the changelog sections after `since` up to the running version, read at its tag (`CHANGELOG.md`, and `changelog/CHANGELOG.<lang>.md` with English for the versions it lacks); `null` when there's nothing to show |
| `update_download` | `{}` | downloads and verifies; progress reaches the owner window as the `update-progress` event (`phase`, `downloaded`, `total`). Cancelled: error `cancelled` |
| `update_cancel` | — | cuts the download |
| `update_install_and_restart` | — | installs and restarts; the UI calls it only from the quit control (`requestUpdateRestart` in `quitGuard.ts`) |
| `open_release_page` | `{ url }` | opens the page in the browser; only DBine release pages |

If another window becomes the owner, the previous one receives
`update-owner-changed` and closes its notice. When the download finishes, the
owner receives `update-finished` (`ok`, `cancelled`, `error`): this also ends
the notice of a window that took over a download started by another, already
closed. Every window receives `update-state-changed` when a downloaded
update becomes ready or is replaced by a newer one; the status bar then asks
`update_state`.

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
| macOS | `cargo tauri build --target aarch64-apple-darwin --features plugins --config src-tauri/tauri.updater.conf.json -- --no-default-features` (same with `x86_64-apple-darwin`) | `target/<triple>/release/bundle/macos/DBine.app.tar.gz` and `.sig` |
| Windows (from macOS) | `cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --features plugins --bundles nsis --config src-tauri/tauri.updater.conf.json -- --no-default-features`, with the LLVM environment from `scripts/build-windows-from-mac.sh` | `target/x86_64-pc-windows-msvc/release/bundle/nsis/DBine_X_x64-setup.exe` and `.sig` |
| Linux (Docker) | `scripts/linux-release-build.sh` inside the container, with the key mounted at `/run/secrets/dbine-updater.key` (the script's header has the `docker run`) | `linux-out/DBine_X_amd64.AppImage` and `.sig` |

- On Windows, check that the log does not say "Failed to add bundler type":
  without that mark the app does not know NSIS installed it and offers the
  page instead of updating (it fails on the safe side, but it must be fixed).
- The CLI also signs the `.deb` and the `.rpm`. Those `.sig` files are not
  uploaded.

## Publishing the manifest

In CI (`.github/workflows/release.yml`) the release is a **draft** until
the four platforms are built: each job adds its installers and its part of
`latest.json` to the draft, and the final `release-page` job writes the notes
and publishes it as the latest release. Installed apps read
`releases/latest/download/latest.json`, so they never see a release with
only some platforms in it. If a build fails, the release stays a draft; a
new run of the same tag fills that draft again.

The Android phone app is part of the same release: the `android` job builds
and signs `DBine-<version>-android.apk` alongside the desktop jobs, and
`android-upload` adds it to the draft once `build` is done. `release-page`
waits for it too, so a failed APK build or signature check leaves the release
a draft like a failed desktop build. It is not in `latest.json`: the phone
app checks GitHub's latest release itself ([mobile-companion.md](mobile-companion.md)).

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
   `identifier` (for example `io.addlayer.dbine.updtest`): one with
   `"version": "0.1.3"` and another with the new version and
   `--config src-tauri/tauri.updater.conf.json`, signed with the test key.
2. Copy the old one to a temporary folder (not to `/Applications`) and serve
   `latest.json` and the new `.app.tar.gz` with `python3 -m http.server`.
3. Open it with a temporary `HOME` and `DBINE_UPDATE_ENDPOINT` pointing to the
   server: notice, progress bar, "ready", the quit control with a task
   running, restart and the new version in Settings.
4. Without the server: the manifest is not there and DBine offers the page.

Windows (passive install and restart) is tested on a Windows machine or VM.
