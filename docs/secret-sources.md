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
secret's `username` key.

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

## Connections that come from elsewhere

A source runs a program or sends your credentials to a service, so a
definition that arrives from outside this computer **never runs without a
confirmation here**: connections imported from a file or from other tools,
or restored from a cloud backup. The first time such a connection needs it
(connecting, **Test connection**, the field's **Test**), DBine shows the connection,
the field and the exact definition (the whole command, the reference, the
Vault address and path…) and runs it only if you confirm.

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
sandbox. Vault works (plain HTTP), and AWS Secrets Manager works with keys
from the environment or `~/.aws/credentials`.

## For developers

- Definitions: `crates/dbine-core/src/secret_sources.rs` (`SecretSource`,
  stored as JSON in the connection option `secret_sources`, by field key;
  validation, the confirmation digests, JSON keys). A Vault token typed in the
  form travels as the option `source_token:<field>` and `save_connection`
  moves it to the keychain.
- Resolution: `src-tauri/src/secret_sources/` (`fill`, called by
  `AppState::connect_config` before the tunnel and the driver; the providers;
  the command runner). The cloud credential chains and SigV4 come from
  `crates/drivers/postgres/src/cloud_auth.rs`, included with `#[path]`.
- Commands (`args` in snake_case):

| Command | args | Returns |
|---|---|---|
| `test_secret_source` | `{ connection_id \| null, driver, field, source, token \| null }` | `{ chars, username }` |
| `approve_secret_source` | `{ connection_id \| null, field, source }` | `void` |

- Errors: `secret_source` (a provider failed; the message says which and
  why) and `secret_source_approval` (not confirmed; `approval` carries
  `{ connection_id, connection_name, field, field_label, source }` for the
  dialog, the message doesn't).
