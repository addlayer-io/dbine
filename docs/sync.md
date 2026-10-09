# Cloud sync

DBine stores everything in a local database (SQLite, in the app's
configuration folder). Sync keeps an **encrypted backup copy** of that
database in the user's own account. That way, on another machine you connect
the same account and recover everything.

It opens in **Settings › Sync**, from the sidebar gear or with ⌘,.

## What is stored

- Connections: name, color, folder and configuration (host, user, options…).
- Passwords and secret fields of the connections that save them.
- Explorer folders.
- Saved queries.
- Preferences: copy format, maximum rows and so on.

Panel sizes, open tabs and the explorer state aren't stored: those belong to
each machine.

## Security

- **Encrypted on the machine, before it leaves.** The full backup is
  encrypted with XChaCha20-Poly1305. The key is derived from a passphrase the
  user chooses, with Argon2id (64 MiB, 3 iterations and a random salt).
  Everything is encrypted, not just the passwords: hosts, users, queries and
  preferences. The only readable part is a header with the date, the machine
  name, the version and the encryption parameters. That header is
  authenticated, so if it's tampered with, the backup doesn't open.
- **In the user's account.** The backup goes to the app's private folder in
  Google Drive (`appDataFolder`), to the app's folder in OneDrive
  (`Apps/DBine`) or to a folder on disk. AddLayer has no servers for this and
  never receives the data.
- **Minimal permissions.** On Google, `drive.appdata` is requested and on
  Microsoft `Files.ReadWrite.AppFolder`. With that DBine only sees its own
  folder and no other file of the user.
- **Credentials encrypted on each machine.** The passphrase and the account
  tokens stay in the encrypted secrets file, with the key in each machine's
  system keychain, never in the state file or in the logs. The login is done
  in the system browser (OAuth with PKCE and redirect to `127.0.0.1` or
  `localhost`), so DBine never sees the account password.
- **Without the passphrase there's no access.** Nobody can read the backup
  without it: not Google, not Microsoft, not AddLayer. For the same reason,
  if it's lost, there's no way to recover it.

The format is in `crates/dbine-sync/src/crypto.rs`. A backup can be opened
without the app:

```sh
DBINE_BACKUP_PASSPHRASE='…' cargo run -p dbine-sync --example open_backup -- dbine-backup.json
```

## How it syncs

- **When turning it on:**
  - if there's no backup, a passphrase is chosen (10 characters minimum) and
    this machine's data is uploaded;
  - if there's already one, its passphrase is entered and it's restored on
    this machine, or it's replaced with this one's data.
- **Automatic** (can be turned off):
  - uploads about 4 seconds after the last change;
  - pulls the changes from other machines when the app opens and every 10
    minutes.

On each sync two things are compared: whether this machine has changes not
yet uploaded and whether the backup changed since it was last seen.

| Local | Backup | What it does |
|---|---|---|
| no changes | same | nothing |
| with changes | same | uploads |
| no changes | changed | restores |
| with changes | changed | the most recent wins |

**Nothing is lost silently:**
- Before restoring, what was on the machine is saved in an encrypted local
  copy: `backups/` in the configuration folder, keeping the last 10. You can
  go back to any of them from Settings.
- Before replacing a backup written by another machine, that backup is saved
  in the cloud as `dbine-backup.previous.json`.

**Passphrase or access changes:**
- If the passphrase is changed on one machine, the others ask for it the next
  time they sync.
- If access to the account is revoked, you're asked to connect it again.

## Register the app with Google and Microsoft

For the login to work, the app has to be registered with each provider. It's
a procedure AddLayer does **only once**; users register nothing.

While the registration is missing, that provider shows as "Not available in
this version". The **Folder** option always works.

### Google Drive

1. At <https://console.cloud.google.com>, create a project (for example,
   "DBine").
2. **APIs & Services › Library**: enable **Google Drive API**.
3. **OAuth consent screen** (Google Auth Platform):
   - type **External**;
   - name "DBine", support email and logo;
   - under **Data Access**, add the scope
     `https://www.googleapis.com/auth/drive.appdata`.
4. **Clients › Create client**: type **Desktop app**. This gives a
   *client ID* and a *client secret*. In desktop apps the secret isn't
   secret: it goes inside the app, and what protects the exchange is PKCE.
5. While the app is in **Testing** mode, only the users added under
   **Audience › Test users** can sign in. To publish it you have to pass
   Google's verification. `drive.appdata` isn't a restricted scope, so it
   doesn't require the security audit.

### OneDrive (Microsoft)

1. At <https://entra.microsoft.com>, go to **App registrations › New
   registration**:
   - name "DBine";
   - accounts: **any organizational directory and personal Microsoft
     accounts**.
2. **Authentication › Add a platform › Mobile and desktop applications**:
   - redirect URI `http://localhost`;
   - turn on **Allow public client flows**.
3. **API permissions › Microsoft Graph › Delegated**:
   `Files.ReadWrite.AppFolder`, `User.Read` and `offline_access`.
4. No secret is needed: it's a public client with PKCE. The *Application
   (client) ID* is the only thing needed.

### Where the IDs go

- **In the build** (the normal way to distribute), as environment variables
  at compile time:

  ```sh
  DBINE_GOOGLE_CLIENT_ID=… DBINE_GOOGLE_CLIENT_SECRET=… DBINE_MICROSOFT_CLIENT_ID=… cargo tauri build
  ```

  To build locally without typing them, they can go in a `.env` file at the
  repo root (git ignores it):

  ```sh
  DBINE_GOOGLE_CLIENT_ID=….apps.googleusercontent.com
  DBINE_GOOGLE_CLIENT_SECRET=…
  DBINE_MICROSOFT_CLIENT_ID=…
  ```

  If a variable is also in the environment, the environment wins (that's how
  it works in CI, with the repository secrets). After creating the `.env` for
  the first time, run `cargo clean -p dbine-sync` once; from then on, each
  change in the file recompiles only what's needed.

- **At runtime** (to test without recompiling), in the `cloud-clients.json`
  file inside the app's configuration folder. On macOS it's
  `~/Library/Application Support/com.addlayer.dbine/`. What the file says
  overrides the build's:

  ```json
  {
    "google_client_id": "….apps.googleusercontent.com",
    "google_client_secret": "…",
    "microsoft_client_id": "…"
  }
  ```

## Code

- `crates/dbine-sync`: format and encryption (`crypto.rs`), decision engine
  (`engine.rs`), OAuth (`oauth.rs`) and the providers (`gdrive.rs`,
  `onedrive.rs`, `folder.rs`).
  - Tests: encryption, OAuth with loopback, the engine with two machines and
    conflicts, and both providers against local imitations of their APIs,
    including token renewal after a 401.
- `src-tauri/src/sync.rs`: the handler and the automatic task.
- `src-tauri/src/commands/sync.rs`: the commands (docs/api-commands.md).
- `web/src/components/SettingsDialog.vue`, `web/src/stores/sync.ts` and
  `web/src/stores/settings.ts`: the UI and the preferences.
