# Identifier migration

DBine's app identifier changed from `com.addlayer.dbine` to
`io.addlayer.dbine`. Tauri, the webviews and the keychain name things after
it, so the first start of the new version **copies** everything that carried
the old name. The old data is never changed or deleted: an older version
installed next to the new one keeps working on it, and going back to it loses
nothing. Deleting the old data is left to a later release.

## Where the data lives now

| System | State, vault, logs, backups, components, models | Webview storage (open tabs, assistant chats, language, sidebar) | Cache |
|---|---|---|---|
| macOS | `~/Library/Application Support/io.addlayer.dbine` | `~/Library/WebKit/io.addlayer.dbine` (and `~/Library/HTTPStorages/io.addlayer.dbine` if present) | `~/Library/Caches/io.addlayer.dbine` |
| Windows | `%APPDATA%\io.addlayer.dbine` | `%LOCALAPPDATA%\io.addlayer.dbine\EBWebView` | `%LOCALAPPDATA%\io.addlayer.dbine` |
| Linux | `~/.config/io.addlayer.dbine` | `~/.local/share/io.addlayer.dbine` | `~/.cache/io.addlayer.dbine` |

`~/Library/Logs/<id>` is copied too on macOS, although DBine keeps its logs in
the state folder (`logs/`).

The keychain service is `io.addlayer.dbine`. On macOS the scheduled tasks'
LaunchAgents are `~/Library/LaunchAgents/io.addlayer.dbine.task.<id>.plist`.

The Flathub build doesn't migrate: it lives in its own sandbox folders.

## What is copied on the first run

**Folders** (`src-tauri/src/identifier.rs`). At startup, before Tauri or a
webview exists, and also when the OS scheduler starts a task
(`dbine --run-task`):

- each folder above with the old name is copied to the new one when the old
  one exists and the new one doesn't. Both existing without a finished copy:
  neither is touched, and the log says so;
- the copy goes into a temporary sibling, is checked (every file there with
  its size), gets a marker file (`.dbine-copied-from-com.addlayer.dbine`) and
  is renamed into place. A copy interrupted before its rename starts over on
  the next start. The marker stays, so the copy runs only once: what changes
  afterwards in either folder stays there;
- on macOS every file is an APFS clone: it takes no extra disk space until
  one side changes (the 25 GB of AI models on a test Mac cost nothing and
  took a fraction of a second). Where cloning fails (a volume that isn't
  APFS), it's a plain copy;
- on Windows and Linux the re-downloadable folders inside the state folder,
  `models` (AI models and llama.cpp) and `components` (downloaded drivers,
  DuckDB), are copied only up to 256 MB each. Bigger ones are left out and
  logged: **a model may be downloaded again** the first time it's used, and
  a driver or DuckDB the same way;
- one process migrates at a time (a lock file in the temp folder);
- if another DBine process is running (the old version still closing after an
  update, an older version open, a scheduled task), the app waits up to 20
  seconds for it. If it's still there, nothing is copied: its SQLite files
  may be mid-write. That start runs on the old folders, as before the change,
  and the new ones Tauri and the webview create are marked provisional; the
  next start sets them aside (renamed to `<id>.provisional-<date>`, never
  deleted) and copies. A copy that fails is handled the same way. A scheduled
  task started in that situation runs on the old folders and creates nothing
  new;
- each copy is logged in `logs/app.log` (`identifier migration: …`), with the
  folder path only.

Only the real app migrates: a test instance with its own identifier (e2e,
updater tests) leaves every folder alone.

**Keychain** (`crates/dbine-core/src/secrets.rs`). The vault key, and any
older per-connection item, is looked up under `io.addlayer.dbine` first and,
if it isn't there, under `com.addlayer.dbine`, and then written under the new
service. The old item is never changed or deleted. If reading it fails (for
example, the user denies the macOS prompt), nothing is written and the next
start tries again. Deleting a secret deletes it only under the new service.
With `DBINE_KEYCHAIN_SERVICE` set (a test instance) nothing migrates.

The cloud backup's passphrase and tokens are stored in the vault under names
that carried the identifier (`com.addlayer.dbine.sync.passphrase`,
`….sync.token.<provider>`). The first time the new app opens its vault, each
is copied to its `io.addlayer.dbine.…` name, once (the vault records it, so a
secret deleted later doesn't come back). The vault the old app uses is its
own file in the old folder, never written by the new app; even a vault the
new app opened in place (a start on the old folders) keeps the old names, so
0.1.11 reads it as before.

**Scheduled tasks** (`src-tauri/src/tasks/os.rs`). These are the exception:
two LaunchAgents for one task would run it twice, so on macOS they move. At
startup, with the other entries' re-sync, each one under the old label is
replaced: an enabled task is registered under the new label first, and its
old entry is removed only once that worked. The entry of a deleted or
disabled task is removed. Removing a task also removes an old-label entry if
one is left. It happens only once the state was copied (not on a start that
runs on the old folders). From then on **a scheduled task runs from the
newest app**, with its copy of the state. If the user goes back to 0.1.11,
its own re-sync registers its old label again (and the new app moves it back
the next time it starts).

Windows (`DBine\task-<id>`) and Linux (`dbine-task-<id>`, cron's
`# dbine-task:<id>`) never carried the identifier: nothing changes there.

## What the user may notice

- **macOS:** the first start asks once for access to the keychain item
  `com.addlayer.dbine` (as after every update while DBine is signed ad hoc).
  macOS may ask again for permissions it keeps per app: local network access,
  and access to the Documents, Desktop or Downloads folders. A Full Disk
  Access the user granted by hand has to be granted again. Re-registering the
  scheduled tasks may show the "Background item added" notice. The
  `com.addlayer.dbine` preferences domain (the open dialog's last folder,
  spelling panel positions) is not copied. See [updates](updates.md).
- **Windows:** the installer keys on the product name, so the update installs
  in place. The taskbar groups windows by AppUserModelID, which is now
  `io.addlayer.dbine`. A pin made with an older version keeps the old ID, so
  the running window may show next to it as a separate button until DBine is
  pinned again. The Start menu and desktop shortcuts are re-created with the
  new ID by the installer.
- **Linux:** the `.deb` and `.rpm` package name and the `.desktop` file come
  from the product name, so an upgrade replaces the package. Nothing changes
  for the user.
- **Everywhere:** the old folders stay on disk (on macOS the clones share
  their space). A large model may be downloaded again on Windows and Linux.
