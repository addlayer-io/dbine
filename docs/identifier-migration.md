# Identifier migration

DBine's app identifier changed from `com.addlayer.dbine` to
`io.addlayer.dbine`. Tauri, the webviews and the keychain name things after
it, so the first start of the new version moves everything that carried the
old name. Nothing is lost and the user does nothing.

## Where the data lives now

| System | State, vault, logs, backups, components, models | Webview storage (open tabs, assistant chats, language, sidebar) | Cache |
|---|---|---|---|
| macOS | `~/Library/Application Support/io.addlayer.dbine` | `~/Library/WebKit/io.addlayer.dbine` (and `~/Library/HTTPStorages/io.addlayer.dbine` if present) | `~/Library/Caches/io.addlayer.dbine` |
| Windows | `%APPDATA%\io.addlayer.dbine` | `%LOCALAPPDATA%\io.addlayer.dbine\EBWebView` | `%LOCALAPPDATA%\io.addlayer.dbine` |
| Linux | `~/.config/io.addlayer.dbine` | `~/.local/share/io.addlayer.dbine` | `~/.cache/io.addlayer.dbine` |

`~/Library/Logs/<id>` is moved too on macOS, although DBine keeps its logs in
the state folder (`logs/`).

The keychain service is `io.addlayer.dbine`. On macOS the scheduled tasks'
LaunchAgents are `~/Library/LaunchAgents/io.addlayer.dbine.task.<id>.plist`.

## What moves on the first run

**Folders** (`src-tauri/src/identifier.rs`). At startup, before Tauri or a
webview exists, and also when the OS scheduler starts a task
(`dbine --run-task`):

- each folder above with the old name moves to the new one when the old one
  exists and the new one doesn't. Both existing: neither is touched, and the
  log says so;
- it's a rename (the two are siblings on one filesystem, so it's atomic).
  Across filesystems it copies into a temporary sibling, checks that every file
  arrived with its size, renames the copy into place and only then deletes the
  old folder. A copy interrupted before its rename starts over on the next
  start; one interrupted after it only finishes deleting the old folder;
- one process migrates at a time (a lock file in the temp folder);
- if another DBine process is running (the old version still closing after an
  update, a scheduled task), the app waits up to 20 seconds for it. If it's
  still there, nothing moves: its SQLite files may be open. That start runs on
  the old folders, and the new ones Tauri and the webview create are marked
  provisional; the next start sets them aside (renamed to
  `<id>.provisional-<date>`, never deleted) and moves the old ones in. A
  scheduled task started in that situation runs on the old folders and
  creates nothing new;
- each move is logged in `logs/app.log` (`identifier migration: …`), with the
  folder path only.

Only the real app migrates: a test instance with its own identifier (e2e,
updater tests) leaves every folder alone.

**Keychain** (`crates/dbine-core/src/secrets.rs`). The vault key, and any
older per-connection item, is looked up under `io.addlayer.dbine` first and,
if it isn't there, under `com.addlayer.dbine`. When found there, it's written
under the new service and deleted from the old one only after that write
succeeds. If reading the old item fails (for example, the user denies the
macOS prompt), the old item stays as it is, nothing is written and the next
start tries again. Deleting a secret deletes it under both services, so an old
copy never comes back. With `DBINE_KEYCHAIN_SERVICE` set (a test instance)
nothing migrates.

The cloud backup's passphrase and tokens are stored in the vault under names
that carried the identifier (`com.addlayer.dbine.sync.passphrase`,
`….sync.token.<provider>`). They're renamed inside the vault the first time
it opens.

**Scheduled tasks** (`src-tauri/src/tasks/os.rs`). On macOS the LaunchAgents
carried the identifier in their label. At startup, with the other entries'
re-sync, each one under the old label is replaced: an enabled task is
registered under the new label first, and its old entry is removed only once
that worked, so it never runs twice and never stops being scheduled. The
entry of a deleted or disabled task is removed. Removing a task also removes
an old-label entry if one is left. Windows (`DBine\task-<id>`) and Linux
(`dbine-task-<id>`, cron's `# dbine-task:<id>`) never carried the identifier:
nothing changes there.

## What the user may notice

- **macOS:** the first start asks once for access to the keychain item
  `com.addlayer.dbine` (as after every update while DBine is signed ad hoc).
  "Always Allow" also lets the old item be deleted without another prompt.
  macOS may ask again for permissions it keeps per app: local network access,
  and access to the Documents, Desktop or Downloads folders. A Full Disk
  Access the user granted by hand has to be granted again. Re-registering the
  scheduled tasks may show the "Background item added" notice. The
  `com.addlayer.dbine` preferences domain (the open dialog's last folder,
  spelling panel positions) is not migrated. See [updates](updates.md).
- **Windows:** the installer keys on the product name, so the update installs
  in place. The taskbar groups windows by AppUserModelID, which is now
  `io.addlayer.dbine`. A pin made with an older version keeps the old ID, so
  the running window may show next to it as a separate button until DBine is
  pinned again. The Start menu and desktop shortcuts are re-created with the
  new ID by the installer.
- **Linux:** the `.deb` and `.rpm` package name and the `.desktop` file come
  from the product name, so an upgrade replaces the package. Nothing changes
  for the user.
