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
| **HashiCorp Vault (database credentials)** | A user and a password Vault makes for this connection (database secrets engine), revoked when it closes. |
| **CyberArk Conjur** | A variable, read with a host's or user's API key. |
| **CyberArk Central Credential Provider** | An account's password (and user), usually with a client certificate. |
| **Delinea Secret Server** | A field of a secret, on premises or Secret Server Cloud (also through Delinea Platform). |

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
secret's `username` key; KeePass and Zoho Vault have the same option (the
entry's `UserName`, the secret's `username`).

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
| Vault sign-in | **Token** (above) or **AppRole**: the role ID, the secret ID (kept in the system keychain) and AppRole's path (`approle` by default). DBine signs in (`auth/<path>/login`), reads, and revokes that token. |

DBine talks to Vault's HTTP API itself (no `vault` CLI needed). The token
only goes in the `X-Vault-Token` header to the address in the form, and no
redirect is followed. Errors: permission denied (the token's policy, or it
expired), secret not found, key not in the secret, Vault sealed.

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

## HashiCorp Vault: database credentials

| Field | |
|---|---|
| Address, namespace, CA certificate, Vault sign-in | As for [HashiCorp Vault](#hashicorp-vault): a token (typed, `VAULT_TOKEN` or `~/.vault-token`) or AppRole. |
| Database engine | The secrets engine's path: `database` by default. |
| Role | The role whose credentials are asked for (`<engine>/creds/<role>`). |
| Static role | A role with a fixed user whose password Vault rotates (`<engine>/static-creds/<role>`). |

Vault's database secrets engine makes a user and a password for whoever
asks, with a lease. Both come from the same response, so the source fills
the connection's **user** too (the form's user is replaced).

How long the credentials live:

- They belong to the connection while it's open: every session it opens
  (explorer, editor tabs, a reconnect) uses the same user and password, kept
  in memory only, never on disk (neither are the lease ID or the token).
- DBine renews the lease before it ends (a third of its duration before, at
  most five minutes before) while the connection has a session open (if it
  has none by then, the lease is revoked instead), and the AppRole
  token that made it too, up to the role's maximum TTL. When the lease can't
  be renewed any more, the next session gets new credentials; the old lease
  is kept until the connection closes, because sessions opened with it may
  still be using it.
- Disconnecting, editing or deleting the connection, or closing DBine,
  revokes every lease of it (`sys/leases/revoke`) and the AppRole token: Vault
  then removes the database user. If DBine can't reach Vault at that moment
  (or is killed), the lease ends at its TTL anyway.
- **Test** next to the field asks for credentials and revokes them at once;
  **Test connection** revokes them two minutes after the test.
- A static role has no lease: its credentials are reused until shortly
  before Vault's next rotation, and nothing is revoked.

The token needs `read` on `<engine>/creds/<role>` (or `static-creds`) and
`update` on `sys/leases/renew` and `sys/leases/revoke`. Errors: permission
denied, role not found, AppRole's role ID or secret ID rejected.

## CyberArk Conjur

| Field | |
|---|---|
| URL | Conjur's appliance URL (`https://conjur.example.com`); Conjur Cloud: `https://<subdomain>.secretsmgr.cyberark.cloud/api`. |
| Account | Conjur's account (`conjur` in Conjur Cloud). |
| Login | `host/<id>` for a workload, or a user's name. |
| Variable ID | `db/prod/password`. |
| Variable with the user | Optional (the password field only): another variable whose value is the connection's user. |
| API key | The host's or user's API key, kept in the system keychain. |
| CA certificate | Optional: PEM file of the CA that signed Conjur's certificate. |

DBine trades the API key for a short-lived access token
(`POST /authn/<account>/<login>/authenticate`) and reads the variables with
it (`GET /secrets/<account>/variable/<id>`); the token lives in memory for
that read only. **JSON key** works as for the other sources. HTTPS only
(plain HTTP only to this computer), and no redirect is followed. Errors:
login or API key rejected, account not found, no `execute` privilege on the
variable, variable not found or without a value.

## CyberArk Central Credential Provider

| Field | |
|---|---|
| URL | The CCP's server (`https://ccp.example.com`): DBine calls `/AIMWebService/api/Accounts`. With a path (`https://ccp.example.com/AIMWebServiceProd`), `/api/Accounts` goes after it. |
| AppID | The application's ID in CyberArk. |
| Safe, Folder | Optional. |
| Object | The account's name in the safe; or, instead or as well, the account's **user** and **address**. |
| Client certificate | Optional (usually required by the CCP): a PEM file with the certificate (and its key, or the key in **Private key**), or a PKCS#12 file (`.p12`/`.pfx`). |
| Passphrase | Of the PKCS#12 file or of an encrypted PKCS#8 key; kept in the system keychain. |
| CA certificate | Optional: PEM file of the CA that signed the CCP's certificate. |
| Take the user too | The connection's user comes from the account's `UserName`. |

The CCP answers the account's `Content` (the password) and `UserName`. HTTPS
only (plain HTTP only to this computer), and no redirect is followed. A PEM
key encrypted the old OpenSSL way (`Proc-Type: 4,ENCRYPTED`) isn't read:
convert it with `openssl pkcs8 -topk8`. Errors: CyberArk's codes and
messages (`APPAP004E`: no account matches; authentication failed for the
AppID), the certificate refused, the passphrase wrong.

## Delinea Secret Server

| Field | |
|---|---|
| URL | Secret Server's address: on premises (`https://ss.example.com/SecretServer`) or Secret Server Cloud (`https://<tenant>.secretservercloud.com`, `.eu` and the other regions' domains). |
| Delinea Platform URL | Optional: `https://<tenant>.delinea.app`, to sign in through the platform. |
| User | A Secret Server user or application account; with Delinea Platform, a service user's client ID. |
| Domain | Optional: the user's domain. |
| Secret ID or name | Its numeric ID, or its exact name (else ignoring case); more than one secret with that name is an error listing their IDs. |
| Field | Optional: the field's slug (`password` by default; `username`, `server`, …) or its name. |
| Password | **Saved in the keychain**, **Ask when connecting** or **From a command**, as for KeePass. With Delinea Platform, the client secret. |
| Take the user too | The connection's user comes from the secret's `username` field. |

DBine gets an OAuth2 access token from Secret Server (`POST /oauth2/token`,
the password grant, with the domain if given) or, with Delinea Platform, from
the tenant (`/identity/api/oauth2/token/xpmplatform`, client credentials),
then reads the secret (`GET /api/v1/secrets/<id>`). The password only goes to
those two URLs, over HTTPS (plain HTTP only to this computer to Secret Server),
and no redirect is followed. Secrets that need a comment, a check-out, an
approval or a double lock can't be read this way, and the error says so.
Other errors: user or password rejected (with **Ask when connecting** it asks
again), permission denied, secret not found, field not found or a file.

This integration follows Delinea's REST API documentation; it was tested
against a mock of the API, not against a live Secret Server.

## Connections that come from elsewhere

A source runs a program or sends your credentials to a service, so a
definition that arrives from outside this computer **never runs without a
confirmation here**: connections imported from a file or from other tools,
or restored from a cloud backup. The first time such a connection needs it
(connecting, **Test connection**, the field's **Test**), DBine shows the connection,
the field and the exact definition (the whole command, the reference, the
Vault address and path, the KeePass file and entry, the Zoho data centre and
client…) and runs it only if you confirm.

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
sandbox. Vault, Zoho Vault, CyberArk and Delinea work (plain HTTPS), AWS Secrets Manager works
with keys from the environment or `~/.aws/credentials`, and KeePass reads
files the sandbox can see (your home folder).

## For developers

- Definitions: `crates/dbine-core/src/secret_sources.rs` (`SecretSource`,
  stored as JSON in the connection option `secret_sources`, by field key;
  validation, the confirmation digests, JSON keys, Zoho's data centres). A
  Vault token or a master password typed in the form travels as the option
  `source_token:<field>`, Zoho's client secret and refresh token as
  `source_token:<field>#client_secret` / `#refresh_token`, and
  `save_connection` moves to the keychain the slots the definition keeps
  (`SecretSource::token_slots`: none for a master password that's asked or
  comes from a command).
- Resolution: `src-tauri/src/secret_sources/` (`fill`, called by
  `AppState::connect_config` before the tunnel and the driver; the providers;
  the command runner; `keepass.rs` with the `keepass` crate, `zoho.rs` with
  Zoho's AES-CTR, PBKDF2 and RSA). The cloud credential chains and SigV4 come
  from `crates/drivers/postgres/src/cloud_auth.rs`, included with `#[path]`.
- Tests: KDBX 4 databases are written by the tests (Argon2d/Argon2id, AES,
  ChaCha20, Twofish, key files); `src-tauri/src/secret_sources/testdata/` has
  KDBX 3.1 files from other writers and the Zoho vectors
  (`zoho-vectors.json`, made with Zoho's web crypto, Node and OpenSSL as
  `zoho-vectors.mjs` says). `secret_sources::tests::live_postgres` connects
  to `dbine-test-postgres` with a password from a command, a KeePass file and
  (if running) Vault.
- Vault database credentials: the lease store is `vault_db.rs` (held by
  connection, renewer task, `release` from `AppState::close_connection_sessions`,
  `release_all` from `exit_cleanup`). `secret_sources::tests::live_vault_database`
  needs Vault in dev mode with the engine pointing at `dbine-test-postgres`:

  ```sh
  docker run -d --name dbine-test-vault-db -p 28201:8200 -e VAULT_DEV_ROOT_TOKEN_ID=dbine-root --cap-add IPC_LOCK hashicorp/vault
  docker exec dbine-test-postgres psql -U postgres -c "CREATE ROLE static_app LOGIN PASSWORD 'initial'"
  V() { docker exec -i -e VAULT_ADDR=http://127.0.0.1:8200 -e VAULT_TOKEN=dbine-root dbine-test-vault-db vault "$@"; }
  V secrets enable database
  V write database/config/pg plugin_name=postgresql-database-plugin allowed_roles="ro,static-app" \
    connection_url="postgresql://{{username}}:{{password}}@host.docker.internal:25010/postgres?sslmode=disable" username=postgres password=pw
  V write database/roles/ro db_name=pg default_ttl=1h max_ttl=24h \
    "creation_statements=CREATE ROLE \"{{name}}\" WITH LOGIN PASSWORD '{{password}}' VALID UNTIL '{{expiration}}'; GRANT pg_read_all_data TO \"{{name}}\";"
  V write database/static-roles/static-app db_name=pg username=static_app rotation_period=86400
  V auth enable approle
  printf 'path "database/creds/ro" { capabilities = ["read"] }\npath "sys/leases/*" { capabilities = ["update"] }\n' | V policy write dbine-db -
  V write auth/approle/role/dbine token_policies=dbine-db token_ttl=20m
  echo "$(V read -field=role_id auth/approle/role/dbine/role-id):$(V write -f -field=secret_id auth/approle/role/dbine/secret-id)"  # DBINE_TEST_VAULT_APPROLE
  ```

- CyberArk Conjur: `secret_sources::tests::live_conjur` runs against Conjur
  Open Source (`cyberark/conjur` as `dbine-test-conjur`, `postgres:16` as
  `dbine-test-conjur-db` on a network of their own, `CONJUR_AUTHENTICATORS=authn`,
  account `myorg`, a policy with the host `apps/dbine` and the variables
  `db/prod/password` = `pw`, `db/prod/username` = `postgres` and an unpermitted
  `db/forbidden`). The CCP's test (`cyberark.rs`) makes a CA and client
  certificates with `rcgen` and serves the Accounts API over TLS that requires
  them; Delinea's runs against a mock of its API.
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
