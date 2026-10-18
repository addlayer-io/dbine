# Passwords from a command or a secret manager

Every secret field of a connection (the database password, an API key or
token, a service-account key, the SSH tunnel's password or key passphrase)
has an **Origin** selector in the connection form:

| Origin | Where the value comes from |
|---|---|
| **Saved in the keychain** (default) | What you type, kept in the system keychain, as before. |
| **Ask every time** (the database password) | Asked when connecting; never kept. |
| **Command** | The output of a command you write. |
| **1Password** | An `op://vault/item/field` reference, read with the 1Password CLI. |
| **HashiCorp Vault** | A key of a KV secret (v1 or v2). |
| **AWS Secrets Manager** | A secret by name or ARN. |
| **Azure Key Vault** | A secret of a vault. |
| **Google Secret Manager** | A secret version of a project. |
| **KeePass** | A field of an entry of a `.kdbx` file, read by DBine itself. |
| **Zoho Vault** | A field of a secret, decrypted by DBine with your master passphrase. |
| **Bitwarden** | A field of an item of your vault (personal or an organization's), decrypted by DBine with your master password. |
| **Bitwarden Secrets Manager** | A secret, with a machine account's access token. |
| **AWS Parameter Store** | A parameter of AWS Systems Manager Parameter Store (SecureString, String or StringList). |

DBine resolves the value right before each connection opens, before the SSH
tunnel and the engine's driver get the settings, so it works the same way on
**every engine**. The value stays in memory only: a connect reuses it for a
minute so the sessions it opens together (explorer, editor tabs) don't each
run the command again; disconnecting, editing or deleting the connection
drops it, and the next connection fetches it again. A rotated password is
picked up at the next connection.

**Test** next to the field resolves it with what the form has now and shows
only how many characters came back (`OK · 24 characters`), never the value.

## JSON secrets

Secrets are often a JSON object (`{"username": "app", "password": "…"}`).
**JSON key** picks the key whose value is used (strings as they are, numbers
and booleans as written). If the key isn't there, the error lists the keys
the secret has (never their values). With AWS Secrets Manager, **Take the
user from this secret too** also fills the connection's user from the
secret's `username` key; KeePass, Zoho Vault, Bitwarden and AWS Parameter
Store have the same option (the entry's `UserName`, the secret's `username`,
the item's username, the value's `username` key).

## Command

The command runs with your **login shell** (`$SHELL -l -c` on macOS and
Linux, so `PATH` finds the same tools as in a terminal; `cmd.exe /c` on
Windows), with no input, in its own process group. What it prints, without
the trailing line break, is the secret.

- It has **30 seconds**: then it's stopped, with everything it started.
- If it ends with an error, the message shows its exit code and the first
  lines of what it wrote to stderr (up to 300 characters). What it wrote to
  stdout is never shown or logged.
- The output can be up to 64 KB of text.

Examples: `pass show databases/production`, `security find-generic-password
-s prod-db -w`, `vault kv get -field=password secret/app/db`,
`bw get password prod-db`.

## 1Password

`op read <reference>` with the 1Password CLI, run directly (no shell) with
the reference as one argument, so vault and item names with spaces work. The
optional **Account** is passed as `--account`. The CLI has to be installed
and signed in: with the desktop app's CLI integration, `op` asks for Touch
ID / Windows Hello itself (DBine waits up to 60 s). The errors say which case
it is: CLI not installed, not signed in (`op signin`), unlock cancelled, item
or field not found.

## HashiCorp Vault

| Field | |
|---|---|
| Address | `https://vault.example.com:8200` |
| Secret path | As `vault kv get` takes it: `secret/myapp/db`. |
| Key | The key inside the secret: `password`. |
| KV version | **Detect** (asks Vault for the mount and its version; if the token can't, tries v2 and then v1), **KV v2** or **KV v1**. |
| Namespace | Vault Enterprise / HCP namespace (`X-Vault-Namespace`), optional. |
| Token | Optional. Without it: `VAULT_TOKEN`, then `~/.vault-token` (what `vault login` writes). A token typed here is kept in the system keychain, like a password. |
| CA certificate | PEM file of the CA that signed Vault's certificate, when it isn't a public one. |

DBine talks to Vault's HTTP API itself (no `vault` CLI needed). The token
only goes in the `X-Vault-Token` header to the address in the form, and no
redirect is followed. Errors: permission denied (the token's policy, or it
expired), secret not found, key not in the secret, Vault sealed.

OpenBao speaks the same KV API: use this source with its address (and its
token, or `VAULT_TOKEN`).

## AWS Secrets Manager

Secret name or ARN, region (optional: else the ARN's, `AWS_REGION`,
`AWS_DEFAULT_REGION` or the profile's) and profile (optional). DBine signs
the `GetSecretValue` request itself (SigV4) with the same credential chain as
the AWS IAM sign-in ([`cloud-authentication.md`](cloud-authentication.md#aws)):
environment keys, the profile's keys, `credential_process`, and the AWS CLI
for SSO and assumed roles. Only the signature travels, to
`secretsmanager.<region>.amazonaws.com`. Binary secrets are read as UTF-8
text. Errors: secret not found in that region, access denied
(`secretsmanager:GetSecretValue`, `kms:Decrypt`), credentials rejected or
expired (`aws sso login`).

## Azure Key Vault

Vault URL (`https://<name>.vault.azure.net`; also the China, US Government
and Germany clouds' suffixes), secret name and version (optional: the
current one). The Entra ID token comes from the same chain as the Entra ID
sign-in ([`cloud-authentication.md`](cloud-authentication.md#azure)): Azure
CLI (`az login`), Azure Developer CLI, managed identity, for the vault's
cloud. A URL that isn't a Key Vault host is refused, so the token never goes
anywhere else. Errors: token rejected, permission denied (the *Key Vault
Secrets User* role or an access policy with *get*), secret not found.

## Google Secret Manager

Project, secret and version (optional: `latest`). The access token comes
from the same chain as the Google Cloud IAM sign-in
([`cloud-authentication.md`](cloud-authentication.md#google-cloud)): gcloud,
the application default credentials, the metadata server. The request goes
to `secretmanager.googleapis.com`. Errors: token rejected, permission denied
(*Secret Manager Secret Accessor*), secret or version not found.

## KeePass

| Field | |
|---|---|
| Database file | The `.kdbx` file (`~/` is your home folder). |
| Entry | `Group/Subgroup/Title` from the top of the database (the root group's own name may lead too), the title alone when no other entry has it, or the entry's UUID (with or without braces and dashes). |
| Field | Optional: `Password` by default; `UserName`, `URL`, `Notes` or any custom (advanced) field. Exact name first, then ignoring case. |
| Key file | Optional: the key file the database needs besides (or instead of) the master password. |
| Master password | **Saved in the keychain** (typed here), **Ask when connecting**, **From a command** (its output, as for the Command origin) or **None** (the key file alone). |

DBine reads the file itself, with no KeePass program installed: KDBX 3.1 and
KDBX 4, AES-256, ChaCha20 or Twofish, AES-KDF or Argon2 (d and id). Opening
it takes the time its key derivation was set to take, on a background
thread; the value is then reused for a minute like the others, and the rest
of the database is dropped. Entries in the recycle bin don't count.

- **Ask when connecting**: the first connection of the run asks for the
  master password (*Master password* dialog) and keeps it in memory until
  DBine closes, the connection is edited or a connect fails; it never goes to
  the keychain. A wrong one asks again. Scheduled tasks, the MCP server and
  the AI assistant can't type it: they get an error that says to connect in
  DBine first. In the form, **Test** asks for it and uses it for that test only.
- Errors: file not found, the master password or key file doesn't open it,
  entry not found (with similar entries' paths, never values), more than one
  entry with that title (with where each one is: use the group path or the
  UUID), field not found (with the entry's field names).
- The path points at a file on this computer, so a definition that came from
  elsewhere needs the same confirmation as a command.

## Zoho Vault

| Field | |
|---|---|
| Data centre | The one of your Zoho account: `zoho.com` (US), `zoho.eu`, `zoho.in`, `zoho.com.au`, `zoho.jp`, `zohocloud.ca`, `zoho.sa`, `zoho.uk`, `zoho.ae`, `zoho.sg` or `zoho.com.cn`. |
| Secret name or ID | The secret's exact name (else ignoring case), or its numeric ID. |
| Field | Optional: `password` by default; `username` or any of the secret's fields, custom fields included. |
| Client ID, client secret, refresh token | A **Self Client** from Zoho's API console and a refresh token generated with the scopes `ZohoVault.secrets.READ` and `ZohoVault.user.READ`. The client secret and the token are kept in the system keychain. |
| Master passphrase | **Saved in the keychain**, **Ask when connecting** or **From a command**, as for KeePass. |

Zoho Vault encrypts every value on the client with your master passphrase:
its API only returns ciphertext. DBine does what Zoho's own clients do,
without any Zoho program installed:

1. Gets an access token from the refresh token at `accounts.<data centre>`
   (reused while it's valid: Zoho limits how often a refresh token may be
   used).
2. Reads the account's salt and iteration count (`GET_LOGIN`) at
   `vault.<data centre>`, derives the master key (PBKDF2-HMAC-SHA256; SHA-256
   for older accounts) and checks it against the account's check value, so a
   wrong passphrase says so instead of returning garbage.
3. Finds the secret (by ID, or by name: more than one with the same name is
   an error listing their IDs) and decrypts the field (AES-256 in counter
   mode). A shared (organization) secret is encrypted with the organization
   key, which DBine decrypts with your RSA private key, itself decrypted with
   the master key.

Tokens only go to the two hosts of the data centre chosen in the form
(`accounts.` and `vault.` of that domain), over HTTPS, and no redirect is
followed. Errors: client ID or secret rejected (wrong data centre?), refresh
token invalid or revoked, missing scopes, too many token requests, secret
not found (with similar names), field not found, wrong master passphrase.

This integration follows Zoho Vault's REST API v1 documentation and the
encryption its web client uses; it was tested against recorded vectors and a
mock of the API, not against a live Zoho account.

## Bitwarden

| Field | |
|---|---|
| Server | `bitwarden.com` (US cloud), `bitwarden.eu` (EU cloud), or your self-hosted server's URL (`https://bitwarden.example.com`; its `/identity` and `/api` are used). Plain `http://` only for this computer (`localhost`, `127.0.0.1`). |
| Item | The item's ID, or its exact name (else ignoring case). Items in the trash don't count. |
| Field | Optional: `password` by default; `username`, `notes`, `totp`, or the name of a custom field (text, hidden, boolean, or linked to the username or password). |
| JSON key | Optional: when the field holds JSON (often the notes), the key to use. |
| client_id, client_secret | Your **personal API key** (web vault: *Account settings › Security › Keys › View API key*). The `client_id` (`user.…`) is part of the definition; the `client_secret` is kept in the system keychain. |
| Master password | **Saved in the keychain**, **Ask when connecting** or **From a command**, as for KeePass. |

Bitwarden encrypts every value on the client: its API only returns
ciphertext. DBine does what Bitwarden's own clients do, with no `bw` program
installed (the `bw` CLI needs an interactive unlock and a session per run,
which a connection can't do):

1. Logs in with the API key (`client_credentials`) and reads the vault
   (`/sync`). The access token is reused while it's valid.
2. Derives the master key from the master password with the account's
   settings (PBKDF2-SHA256, or Argon2id) and opens the user key with it
   (AES-256-CBC + HMAC-SHA256: the MAC is checked first, so a wrong master
   password says so instead of returning garbage).
3. Finds the item (by ID, or by name: more than one with the same name is an
   error listing their IDs) and decrypts the field. An organization's item
   is encrypted with the organization's key, which DBine opens with your RSA
   private key (RSA-OAEP), itself under your user key; an item with its own
   key is handled too.

Tokens only go to the server in the form, over HTTPS, and no redirect is
followed. The access token, the user key and the vault stay in memory.
Errors: API key rejected, too many logins, wrong master password, item not
found (with similar names), more than one item with that name, field not
found (with the item's field names, never values).

Not supported (the error says so): accounts without a master password
(single sign-on with trusted devices, Key Connector) and accounts on the new
v2 encryption format. Tested live against a Bitwarden-compatible server with
items created by the official Bitwarden CLI; not against bitwarden.com
itself.

## Bitwarden Secrets Manager

| Field | |
|---|---|
| Server | As for Bitwarden: `bitwarden.com`, `bitwarden.eu` or your self-hosted URL. |
| Secret ID | The secret's ID (a UUID, as the Secrets Manager web app shows it). |
| Access token | A **machine account**'s access token. Kept in the system keychain; empty: `BWS_ACCESS_TOKEN`. |
| JSON key | Optional, as for the other sources. |

The access token carries the key that opens what Bitwarden sends back with
the login (the organization's key), which opens the secret: DBine does it
itself, as Bitwarden's SDK does, without the `bws` program. Errors: token
malformed or rejected (revoked, or from another server), secret not found
(or the machine account has no access to its project).

Tested against a mock of the API with values the official Bitwarden SDK
decrypted back; not against bitwarden.com itself.

## AWS Parameter Store

Parameter name (`/prod/myapp/db/password`) or ARN, version (optional: a
number or a label; empty: the latest), list element (optional, for a
`StringList`: 1 is the first), region and profile (optional, as for AWS
Secrets Manager). DBine signs `GetParameter` with `WithDecryption` itself
(SigV4), with the same credential chain as AWS Secrets Manager. SecureString
values come decrypted (the credentials need `kms:Decrypt` on the key); a
`StringList` without an element chosen is an error that says how many it
has. The request goes to `ssm.<region>.amazonaws.com`, or to the endpoint in
`AWS_ENDPOINT_URL_SSM` / `AWS_ENDPOINT_URL` when set (the AWS tools' standard
variables, e.g. for LocalStack). Errors: parameter or version not found,
access denied (`ssm:GetParameter`, `kms:Decrypt`), credentials rejected or
expired.

## Connections that come from elsewhere

A source runs a program or sends your credentials to a service, so a
definition that arrives from outside this computer **never runs without a
confirmation here**: connections imported from a file or from other tools,
or restored from a cloud backup. The first time such a connection needs it
(connecting, **Test connection**, the field's **Test**), DBine shows the connection,
the field and the exact definition (the whole command, the reference, the
Vault address and path, the KeePass file and entry, the Zoho data centre and
client, the Bitwarden server…) and runs it only if you confirm.

- The confirmation is kept on this computer only, next to the state
  (`secret-source-approvals.json`), never in the cloud backup or an export.
  It's bound to the connection, the field and the exact text: any change to
  the definition asks again.
- What you type in the form on this computer counts as confirmed. Leaving an
  imported definition as it was, or duplicating it into a new connection,
  doesn't.
- Without a confirmation, scheduled tasks, the MCP server and the AI
  assistant get an error that says to open the connection in DBine; the error
  doesn't include the definition.

The definition itself is not secret: it's kept with the connection's settings
and travels with them (backup, export, import). Its value never does.

## What never sees the value

- The state file, the explorer cache, the logs, telemetry, the cloud backup,
  exports.
- The MCP server and the AI assistant: they use the connection through
  DBine, which resolves the value internally; they don't get the definition
  either (only the connection's name, engine and access level).
- The phone app ([`mobile-security.md`](mobile-security.md)): it never gets
  credentials or connection settings.
- Other programs: the value isn't passed on any command line or environment
  variable.

## Flathub

The Flathub build runs commands inside its sandbox, which doesn't have the
system's programs (`op`, `pass`, `aws`, `az`, `gcloud`): the 1Password, AWS
CLI, Azure and Google steps answer that they aren't available in the
sandbox. Vault, Zoho Vault, Bitwarden and Bitwarden Secrets Manager work
(plain HTTPS), AWS Secrets Manager and Parameter Store work with keys from
the environment or `~/.aws/credentials`, and KeePass reads files the sandbox
can see (your home folder).

## For developers

- Definitions: `crates/dbine-core/src/secret_sources.rs` (`SecretSource`,
  stored as JSON in the connection option `secret_sources`, by field key;
  validation, the confirmation digests, JSON keys, Zoho's data centres). A
  Vault token or a master password typed in the form travels as the option
  `source_token:<field>`, Zoho's client secret and refresh token as
  `source_token:<field>#client_secret` / `#refresh_token` (Bitwarden's
  client secret as `#client_secret`; a Secrets Manager access token is the
  main slot), and
  `save_connection` moves to the keychain the slots the definition keeps
  (`SecretSource::token_slots`: none for a master password that's asked or
  comes from a command).
- Resolution: `src-tauri/src/secret_sources/` (`fill`, called by
  `AppState::connect_config` before the tunnel and the driver; the providers;
  the command runner; `keepass.rs` with the `keepass` crate, `zoho.rs` with
  Zoho's AES-CTR, PBKDF2 and RSA, `bitwarden.rs` with Bitwarden's
  EncStrings, PBKDF2/Argon2id, HKDF and RSA-OAEP, `ssm.rs` with SigV4). The cloud credential chains and SigV4 come
  from `crates/drivers/postgres/src/cloud_auth.rs`, included with `#[path]`.
- Tests: KDBX 4 databases are written by the tests (Argon2d/Argon2id, AES,
  ChaCha20, Twofish, key files); `src-tauri/src/secret_sources/testdata/` has
  KDBX 3.1 files from other writers and the Zoho vectors
  (`zoho-vectors.json`, made with Zoho's web crypto, Node and OpenSSL as
  `zoho-vectors.mjs` says), the Bitwarden vectors (`bitwarden-vectors.json`:
  Secrets Manager values the official SDK decrypted back, KDF vectors) and a
  recorded vault (`bitwarden-recorded.json`: items the official CLI created
  on vaultwarden; `bitwarden-setup.mjs` says how).
  `secret_sources::tests::live_postgres` connects to `dbine-test-postgres`
  with a password from a command, a KeePass file and (if running) Vault;
  `secret_sources::bitwarden::tests::live_vaultwarden` (container
  `dbine-test-vaultwarden`) and `secret_sources::ssm::tests::live_localstack`
  (container `dbine-test-localstack`, `localstack/localstack:4.9` with
  `SERVICES=secretsmanager,ssm,kms`; parameters made with `awslocal ssm
  put-parameter`) do the same with Bitwarden and Parameter Store.
- Commands (`args` in snake_case):

| Command | args | Returns |
|---|---|---|
| `test_secret_source` | `{ connection_id \| null, driver, field, source, token \| null, tokens \| null }` (`tokens`: typed now, by slot; the kept ones fill the rest) | `{ chars, username }` |
| `approve_secret_source` | `{ connection_id \| null, field, source }` | `void` |
| `unlock_secret_source` | `{ connection_id, field, value }` (a master password asked for a saved connection; memory only) | `void` |

- Errors: `secret_source` (a provider failed; the message says which and
  why), `secret_source_approval` (not confirmed; `approval` carries
  `{ connection_id, connection_name, field, field_label, source }` for the
  dialog, the message doesn't) and `secret_source_unlock` (a master password
  to ask for; `unlock` carries `{ connection_id, connection_name, field,
  field_label, kind, wrong }`).
