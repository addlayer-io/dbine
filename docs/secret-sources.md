# Passwords from a command or a secret manager

Every secret field of a connection (the database password, an API key or
token, a service-account key, the SSH tunnel's password or key passphrase)
has a source selector in the connection form. It comes **first**, before the
field: for the database user and password it's **Credentials**; for any
other secret it's a small selector on the field's row, with the field's own
input next to it while the source is local.

The selector lists the local sources first, then a line, then the secret
managers sorted by name in the interface's language:

| Source | Where the value comes from |
|---|---|
| **Save in the keychain** (default) | What you type, kept in the system keychain, as before. |
| **Ask every time** (the database password) | Asked when connecting; never kept. |
| **Command line** | The output of a command you write. |
| **1Password** | An `op://vault/item/field` reference, read with the 1Password CLI. |
| **AWS Parameter Store** | A parameter of AWS Systems Manager Parameter Store (SecureString, String or StringList). |
| **AWS Secrets Manager** | A secret by name or ARN. |
| **Azure Key Vault** | A secret of a vault. |
| **Bitwarden** | A field of an item of your vault (personal or an organization's), decrypted by DBine with your master password. |
| **Bitwarden Secrets Manager** | A secret, with a machine account's access token. |
| **CyberArk Central Credential Provider** | An account's password (and user), usually with a client certificate. |
| **CyberArk Conjur** | A variable, read with a host's or user's API key. |
| **Delinea Secret Server** | A field of a secret, on premises or Secret Server Cloud (also through Delinea Platform). |
| **Google Secret Manager** | A secret version of a project. |
| **HashiCorp Vault** | A key of a KV secret (v1 or v2). |
| **HashiCorp Vault (database credentials)** | A user and a password Vault makes for this connection (database secrets engine), revoked when it closes. |
| **KeePass** | A field of an entry of a `.kdbx` file, read by DBine itself. |
| **Zoho Vault** | A field of a secret, decrypted by DBine with your master passphrase, through a Zoho account connected once on this computer. |

Every secret manager signs in through an **account** connected once on this
computer and used by any number of connections ([Accounts](#accounts)); the
sign-in can still be typed for one connection only (manual mode).

## The form

- **Local sources** (Save in the keychain, Ask every time): the user and
  password fields show as always. Engines without a user (a password only)
  show the password alone; engines with no credentials (a local file) have
  no selector.
- **A command line or a secret manager**: the user and password fields go
  away and that source's inputs take their place, in the form's own two
  columns. A secret manager asks first for its **Cuenta** (one of this
  computer's accounts of that manager, or **Conectar cuenta…**), then for
  the item with 🔍, **Probar**, and **Configuración avanzada** collapsed at
  the bottom, which also switches to manual mode. In manual mode (and for
  connections saved before accounts) each source asks first for what signs
  in to it, then for the item:
  KeePass: database file → key file → master password → entry; Bitwarden:
  server → client ID and secret → master password → item; Zoho Vault:
  account → secret (manual mode: data centre → client ID, secret and refresh
  token → passphrase → secret); Vault:
  address → namespace and CA → sign-in → path (or role); AWS and Parameter
  Store: region → profile → secret or parameter; Azure, Google, Conjur,
  Delinea and 1Password likewise. **Browse** stays disabled, its tooltip
  saying what's missing, until what signs in is filled (on a new connection
  that includes the passwords and secrets to be kept in the keychain).
- **Entries give the user and the password.** With KeePass, Bitwarden,
  1Password, Zoho Vault, Delinea or the Central Credential Provider as the
  credentials, both come from the entry, always: the connection's user field
  doesn't show. By default the user is the entry's own (KeePass `UserName`,
  the login's username in Bitwarden, the item's `username` in 1Password, the
  `username` field in Zoho Vault and Delinea, the account's `UserName` in the
  CCP) and the password its password.
- **Configuración avanzada**, collapsed at the very bottom of the source,
  picks other fields: **User field** and **Password field** (for another
  secret, just **Field**). Both list the entry's field names, which load in
  the background once the entry and the sign-in are there (names only); a
  master password set to "Ask when connecting" is asked only when you click
  **Load fields**. A name can also be typed. The Central Credential Provider
  has no fields to choose.
- **Single values** (AWS Secrets Manager, Parameter Store, Azure Key Vault,
  Google Secret Manager, Vault KV, Bitwarden Secrets Manager, Conjur) keep
  their JSON key; AWS and Parameter Store can also take the user from the
  same secret (**Also use the entry's user**), Conjur from a user variable,
  and Vault database credentials always give it.
- **Other secret fields** (tokens, API keys, the SSH tunnel's password and
  passphrase) work the same way: the selector, then the field's input or the
  source's inputs.
- **Test** at the end of the source's inputs resolves it with what the form
  has now and shows only how many characters came back.

What the form stores: the definitions go in the connection's
`secret_sources` option by field key, and "Ask every time" is the
connection's `save_password` set to false. The entry sources (KeePass,
Bitwarden, 1Password, Zoho Vault, Delinea) take an optional `user_field`
(left out when it's the entry's own), and 1Password a `fill_username`; a
definition without them reads and writes the same JSON, so its confirmation
stays valid. New entry definitions as the credentials are saved with
`fill_username: true`. An older one saved without it keeps working as it
did: the connection's user field shows, with a note and **Use the entry's
user** to switch.

## Browse

Next to the item input of the sources that can list, **Browse** (the
magnifier) opens the source's groups and items with a filter, then the
chosen item's field names; picking one fills the item and the field. It
needs the source's other inputs first (the file, the address, the client
ID…), and signs in as **Test** does. When the entry gives the database user
and password, the field step marks which field is the **User** and which
the **Password**. A master password set to **Ask when connecting** is
asked for this browse only and kept in memory while the dialog is open.

| Source | What it lists | Fields |
|---|---|---|
| KeePass | Every entry outside the recycle bin, by group, with its UUID | The entry's field names |
| 1Password | The vaults, then a vault's items (`op vault list`, `op item list`) | The item's field labels (`op item get`), as they go in the reference |
| Bitwarden | The items, by folder or organization (names decrypted on this computer) | The item's standard fields it has and its custom fields' names |
| Bitwarden Secrets Manager | The secrets the machine account can read | The JSON keys of the value |
| HashiCorp Vault | The KV mounts, then one level of keys at a time | The keys of the secret |
| Vault database credentials | The roles and static roles of the engine | — |
| AWS Secrets Manager | The secrets (`ListSecrets`), by `/` in their names | The JSON keys of the value |
| AWS Parameter Store | The parameters and their types (`DescribeParameters`), by path | The JSON keys of the value |
| Azure Key Vault | The secrets of the vault | The JSON keys of the value |
| Google Secret Manager | The secrets of the project | The JSON keys of the latest version |
| Zoho Vault | The secrets' names | The secret's field names |
| CyberArk Conjur | The variables the login can see, by `/` in their IDs | The JSON keys of the value |
| Delinea Secret Server | A search of the secrets, by folder | The field slugs of the secret's template |

The Central Credential Provider has no list: its inputs stay as typed.

- Browsing returns names, paths, IDs and field names, **never a value**.
  Where the field names are the keys of a JSON value or of a Vault secret,
  DBine reads the value, takes its keys and wipes it.
- An item whose name repeats is chosen by its ID (KeePass: its UUID), so the
  reference reads back the same item.
- It's only in the connection form on this computer: the MCP server, the AI
  assistant and the phone app can't browse. A definition that came from
  elsewhere (an import, a restored backup) is confirmed before it can be
  browsed, and while the connection being edited has such a definition for
  that field, it's confirmed first.
- A password or token typed only to browse is used for that call and not
  saved; it's kept only where you chose **Save in the keychain**, when the
  connection is saved.
- Listings stop at 5,000 entries (the dialog says so); filter to find the
  rest. Delinea searches on the server as you type.

## How it resolves

DBine resolves the value right before each connection opens, before the SSH
tunnel and the engine's driver get the settings, so it works the same way on
**every engine**. The value stays in memory only: a connect reuses it for a
minute so the sessions it opens together (explorer, editor tabs) don't each
run the command again; disconnecting, editing or deleting the connection
drops it, and the next connection fetches it again. A rotated password is
picked up at the next connection.

**Test** resolves it with what the form has now and shows only how many
characters came back (`OK · 24 characters`), never the value.

## JSON secrets

Secrets are often a JSON object (`{"username": "app", "password": "…"}`).
**JSON key** picks the key whose value is used (strings as they are, numbers
and booleans as written). If the key isn't there, the error lists the keys
the secret has (never their values). With AWS Secrets Manager and Parameter
Store, **Also use the entry's user** also fills the connection's user from
the value's `username` key. The entry sources give the user without asking
(see [The form](#the-form)).

## Command line

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

As the credentials, the user comes from the same item: its `username`
field, or the field chosen as **User field** under **Configuración
avanzada** (`[section/]field`). Both are read with one call,
`op item get <item> --vault <vault> --reveal --format=json` (so 1Password
asks to unlock once), and taken from the item by label (any case) or ID;
the rest of the item's JSON is wiped right away. A reference with a query
(`?attribute=…`) reads each field with its own `op read`. No value ever goes
on a command line.

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
current one), and the Microsoft Entra tenant (optional: its ID or domain,
passed to the CLI as `--tenant`; empty: the CLI's current one). The Entra ID token comes from the same chain as the Entra ID
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

Through an account (or with the definition's optional `account`, an e-mail),
the token is always that gcloud account's: `gcloud auth print-access-token
--account <e-mail>` (the flag and the e-mail as two arguments, no shell),
whatever gcloud's active account is, so `gcloud config set account` doesn't
change the identity DBine uses. Only a well-formed e-mail (nothing that reads
as a flag) goes to gcloud. When that account isn't signed in on this computer
or its credentials expired, the error names the command to run,
«gcloud auth login <e-mail>», and the wizard shows it in a box with **Copy**;
DBine never signs in itself. Without an account (accounts saved before the
e-mail was kept, and inline definitions, whose JSON and confirmation are
unchanged), the chain above runs as before, starting with gcloud's active
account; Settings › **Gestores de secretos** says so next to such an account, and
**Reconectar…** pins one.

## KeePass

| Field | |
|---|---|
| Database file | The `.kdbx` file (`~/` is your home folder). |
| Entry | `Group/Subgroup/Title` from the top of the database (the root group's own name may lead too), the title alone when no other entry has it, or the entry's UUID (with or without braces and dashes). |
| Password field, user field | Under **Configuración avanzada**: `Password` and `UserName` by default; `URL`, `Notes` or any custom (advanced) field. Exact name first, then ignoring case. |
| Key file | Optional: the key file the database needs besides (or instead of) the master password. |
| Master password | **Saved in the keychain** (typed here), **Ask when connecting**, **From a command** (its output, as for the Command line source) or **None** (the key file alone). |

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

## Accounts

Every secret manager is used through an **account** connected once on this
computer; any number of connections then name it. Settings › **Gestores de
secretos** lists them grouped by manager, with **Conectar cuenta…** (a menu
of every manager), and for each account **Renombrar**, **Reconectar…** (the
wizard again, keeping the account so its connections go on) and **Quitar**
(its keychain items and any cached access token are deleted; connections
that name it then ask to choose or connect an account).

In the connection form, a manager's block is the same for all of them:
**Cuenta** (this computer's accounts of that manager, or **Conectar
cuenta…**, which opens its wizard and selects the new account) → the item
with 🔍 → **Probar** → **Configuración avanzada**, collapsed, which holds
**modo manual**: the inline sign-in for one connection only. Connections
saved before accounts open in manual mode and work unchanged. HashiCorp
Vault's KV and database credentials share Vault accounts; AWS Secrets
Manager and Parameter Store share AWS accounts. With an account, a Vault
definition only names its path and key (or the role), an AWS one only the
secret or parameter, and so on.

### What is stored where

| What | Where |
|---|---|
| The account: name, who signed in, its manager and what isn't secret (server, client ID, role ID, profile, region, tenant, vault URL, project, file paths, where a master password comes from) | The local state DB (`secret_accounts` table). This computer only: **never** in an export, a cloud backup or a restore. |
| Its secrets | The system keychain, as `secret_account:<id>#<slot>`: `client_secret` (Bitwarden, Zoho), `refresh_token` (Zoho), `passphrase` (a master password kept for Bitwarden, KeePass or Zoho; a CCP key's passphrase), `token` (Vault, Secrets Manager), `secret_id` (AppRole), `password` (Delinea) and `api_key` (Conjur). A master password set to "ask" or to a command is never kept. |
| Nothing at all | 1Password, AWS, Azure and Google Cloud accounts: they name a target (a CLI account, a profile and region, a tenant and vault, a project); the credentials stay with the provider's own tools. |
| The connection's definition | `{"kind":"vault","path":…,"key":…,"secret_account":"<id>"}` in the connection's settings (Zoho Vault: `"account"`). It travels with the connection; the account it names doesn't. |

A connection imported or restored on a computer that doesn't have its
account shows **Elegí o conectá una cuenta de <manager>** in the form, and
connecting says the same. Like any definition from elsewhere, it is
confirmed before it first runs; the confirmation dialog shows the account by
its name.

### The wizards

Every wizard checks each step before the next, has **Atrás** on every step
and keeps nothing until the last one, **Conectar**: it shows where the
credentials will go, checks the account live against the manager and only
if that works writes the keychain and the account (**Cuenta conectada**, with
who signed in). Cancelling sends nothing (nothing is sent before the last
step) and wipes what was typed. Reconnecting fills the wizard again without
its secrets: one left empty keeps the saved one, but only while the account
still points to the same server, user and file; otherwise it's typed again.
Errors send you back to the step that fixes them.

The providers' labels below are quoted as their interfaces show them
(checked against their documentation in October 2026).

| Manager | Steps | Where the values are | The check | Network destinations |
|---|---|---|---|---|
| Bitwarden | Server (US cloud, EU cloud or self-hosted URL) → API key → master password (keychain, ask, command) | Web app: **Settings → Security → Keys → View API key** (asks for the master password; shows `client_id` and `client_secret`) | Logs in with the API key (`client_credentials`), syncs and opens the user key with the master password; shows the e-mail | `identity.bitwarden.com` and `api.bitwarden.com` (or `.eu`), or the self-hosted server's `/identity` and `/api` |
| Bitwarden Secrets Manager | Server → access token | Secrets Manager: **Machine accounts** → the account → **Access tokens** tab → **Create access token** (**Name**, **Expires**); shown once | Gets the token's session and lists the secrets the machine account can read (names only) | The same server's `/identity` and `/api` |
| HashiCorp Vault / OpenBao | Address, namespace, CA file → token or AppRole (role ID, secret ID, mount) | `vault login` / `vault token create`; `vault read auth/approle/role/<role>/role-id`, `vault write -f auth/approle/role/<role>/secret-id`. An empty token uses `VAULT_TOKEN` or `~/.vault-token` | `auth/token/lookup-self` (an AppRole login first, its token revoked right after); shows the display name and policies | The address typed (HTTPS; HTTP only to this computer) |
| Delinea | Secret Server URL (and Delinea Platform's tenant URL) → user and domain (or the platform's client ID) and password | Delinea Platform: **Access → Users → More → Add service user** | `POST /oauth2/token` (or the platform's `/identity/api/oauth2/token/xpmplatform`), then `GET /api/v1/users/current` | The Secret Server URL and `https://<tenant>.delinea.app` |
| CyberArk Conjur | URL, account, CA file → login and API key | The API key Conjur shows when the policy that creates the host is loaded, or when an administrator rotates it | `POST /authn/{account}/{login}/authenticate`, then `GET /whoami` | The Conjur URL |
| CyberArk Central Credential Provider | URL, AppID, CA file → client certificate, key and passphrase | The AppID and certificate the CyberArk administrator set up for DBine | The CCP has no request that reads nothing (every Accounts query hands out a password), so the check is the TLS handshake with the client certificate and a request to the server's root, **without the AppID**; the AppID is tested with **Probar** in a connection | The CCP URL |
| 1Password | The CLI account (sign-in address or ID) | Detected with `op account list`; if there are none: the 1Password app's **Settings → Developer → Integrate with 1Password CLI** (on Linux first **Settings → Security → Unlock using system authentication**), or `op account add` | `op whoami --account …`; shows the e-mail | None from DBine: the `op` CLI talks to 1Password |
| AWS | Profile (detected from `~/.aws/config` and `~/.aws/credentials`, or the default chain) and region | `aws configure`, `aws configure sso` | `sts:GetCallerIdentity`, signed with SigV4; shows the ARN | `sts.<region>.amazonaws.com` (`.amazonaws.com.cn` in China), or `AWS_ENDPOINT_URL_STS` / `AWS_ENDPOINT_URL` when set |
| Azure Key Vault | Vault URL and tenant (detected with `az account show`) | Azure portal, the key vault's **Overview → Vault URI** | A token for Key Vault from the Azure CLI (`az account get-access-token --resource https://vault.azure.net --tenant …`); shows who it's for, and refuses a token of another tenant | None from DBine: `az` signs in; connections then go to the `*.vault.azure.net` host only |
| Google Secret Manager | Project (detected with `gcloud config list`) and gcloud account (a select with `gcloud auth list`'s accounts, the active one preselected) | `gcloud auth login` (`gcloud auth login <e-mail>` for another account), or `gcloud auth application-default login` with no account | A token from gcloud for that account (`--account`); with none, from the active account, the application default credentials or the metadata server; shows the account | None from DBine: gcloud signs in; connections then go to `secretmanager.googleapis.com` |
| KeePass | Database file and key file → master password (keychain, ask, command, none) | — | Opens the database; says how many entries it has | None: a local file |

Vault's OIDC and userpass sign-ins aren't account modes of their own: they
need a browser round trip (OIDC's address comes from Vault's answer) or a
human password kept in the keychain, for what a token already covers. Sign
in with `vault login -method=oidc` (or `-method=userpass`) and leave the
token empty: the account then uses that token, as the CLI does.

### Security

- Credentials only go to the account's own endpoint or the provider's fixed
  hosts (the table above): HTTPS only, plain HTTP only to this computer
  (`localhost`, `127.x`, `[::1]`), no redirect followed, and nothing sent to
  a host taken from a server's answer. Vault and Delinea accounts are
  stricter than inline definitions: no plain HTTP to another computer.
- Secrets never go to the state DB, the logs, the error messages, telemetry,
  MCP, the phone, exports or backups. The checks wipe them from memory when
  done, and a failed check drops any access token it got.
- The commands (`op`, `az`, `gcloud`, a master password command) run as for
  inline definitions, never with a secret on their command line.
- The account commands are the connection form's and Settings' only: MCP,
  the assistant and the phone have no way to them.

## Zoho Vault

Zoho Vault is used through an **account** connected once on this computer;
any number of connections then use it. With **Zoho Vault** chosen as the
credentials, the form asks for:

| Field | |
|---|---|
| Cuenta | One of this computer's Zoho Vault accounts, or **Conectar cuenta…**, which opens the wizard below and selects the new account. |
| Secret name or ID | The secret's exact name (else ignoring case), or its numeric ID. A name is looked for in the account's secrets list, page by page, up to the first 5,000 (Zoho's API has no name filter it accepts); an ID is read directly. The 🔍 button lists the account's secrets once an account is chosen. |
| Password field, user field | Under **Configuración avanzada**: `password` and `username` by default; any of the secret's fields, custom fields included. |

### Connecting an account

**Conectar Zoho Vault** (from the form, or Settings › **Gestores de
secretos**) walks through Zoho's own Self Client set-up. Zoho's API
labels are quoted as Zoho shows them.

1. **Cliente.** **Abrir la consola de API de Zoho** opens
   <https://api-console.zoho.com/>. There: **GET STARTED** (or **ADD
   CLIENT**, top right, if you already have clients) → **Self Client** →
   **CREATE NOW** → **CREATE** → **OK**. In the **Client Secret** tab, copy
   the **Client ID** and the **Client Secret** into the wizard. An existing
   Self Client can be reused.
2. **Código.** In the Self Client's **Generate Code** tab: **Scope**
   `ZohoVault.secrets.READ,ZohoVault.user.READ` (the wizard has a copy
   button), **Time Duration** 10 minutes, **Scope Description** `DBine`,
   then **CREATE**. If Zoho asks for the app and the portal or organization,
   choose Zoho Vault and yours, and **CREATE** again. Paste the code.
3. **Conexión.** DBine exchanges the code for a refresh token itself
   (`POST <accounts server>/oauth/v2/token`, `grant_type=authorization_code`).
   You never choose the data centre: a Self Client exists only in its
   owner's data centre, so DBine tries Zoho's accounts servers in turn until
   one doesn't answer `invalid_client`, and uses that data centre's Vault
   host. It then reads the account (`GET_LOGIN`) for its e-mail, offered as
   the account's name.
4. **Frase.** The master passphrase and where it comes from: **Guardada en
   el llavero** (default), **Preguntar al conectar** (then asked once per
   run, for all the account's connections) or **De un comando**. It's
   checked against the account right away: the wizard ends with **Cuenta
   conectada**, or says the passphrase doesn't match.

Each step is checked before the next and has **Atrás**. Nothing is kept
until the last step: cancelling leaves nothing in the keychain (the refresh
token the code gave is revoked at Zoho). Errors say what to do: an expired
or already used code (codes last the minutes chosen in **Time Duration**)
sends you back to generate a new one; a wrong client ID or secret, to the
first step.

Settings › **Gestores de secretos** (*Cuentas de gestores de secretos*) lists
the accounts:
**Renombrar**, **Reconectar…** (the wizard again with a new code, keeping
the account, so its connections go on) and **Quitar** (its keychain items
and its cached access token are deleted; connections that use it then ask to
choose or connect an account).

### What is stored where

| What | Where |
|---|---|
| The account: name, e-mail, data centre, client ID, passphrase choice (and its command) | The local state DB (`secret_accounts` table). This computer only: **never** in an export, a cloud backup or a restore. |
| Client secret, refresh token, the master passphrase when kept | The system keychain, as `secret_account:<id>#client_secret`, `#refresh_token`, `#passphrase`. |
| The connection's definition | `{"kind":"zoho","secret_name":…,"account":"<id>"}` in the connection's settings: it travels with the connection, but the account it names doesn't. |
| Access tokens | Memory only, one per account, shared by all its connections (Zoho allows 10 token requests per 10 minutes). |

A connection imported or restored on a computer that doesn't have its
account shows **Elegí o conectá una cuenta de Zoho Vault** in the form, and
connecting says the same instead of failing obscurely. Like any definition
from elsewhere, it is confirmed before it first runs.

### Manual mode

Under **Configuración avanzada**, **Usar un cliente propio sin cuenta
guardada (modo manual)** switches to the inline fields, for whoever needs a
sign-in for one connection only. Connections saved before accounts existed
open in this mode and keep working unchanged (their definition and its
confirmation are the same):

| Field | |
|---|---|
| Data centre | The one of your Zoho account: `zoho.com` (US), `zoho.eu`, `zoho.in`, `zoho.com.au`, `zoho.jp`, `zohocloud.ca`, `zoho.sa`, `zoho.uk`, `zoho.ae`, `zoho.sg` or `zoho.com.cn`. |
| Client ID, client secret, refresh token | A **Self Client** and a refresh token generated with the scopes `ZohoVault.secrets.READ` and `ZohoVault.user.READ`. The client secret and the token are kept in the system keychain, for this connection's field. |
| Master passphrase | **Saved in the keychain**, **Ask when connecting** or **From a command**, as for KeePass. |

### How values are read

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

Codes, client secrets and tokens only go to Zoho's own hosts: while
connecting an account, the accounts servers of Zoho's data centres (the
table above, plus `accounts.zohohq.in`, which Zoho lists but where DBine
doesn't know a Vault host, so an account there gets a clear error); after
that, the two hosts of the account's data centre (`accounts.` and `vault.`).
Always over HTTPS, and no redirect is followed. Errors: client ID or secret
rejected, code expired or used, refresh token invalid or revoked, missing
scopes, too many token requests, secret not found (with similar names),
field not found, wrong master passphrase.

### With Zoho's command-line tool

If you already use Zoho Vault's official CLI (`zv`), the **Línea de
comandos** source works with it: a command that prints the secret's value
(see [Command line](#command-line)). Zoho's help documents signing in
(`zv login`, then `zv unlock`) and a `get` command that shows a password's
details, but not the exact options to print a single field, so check
`zv get -h` for the command that prints only the value. The CLI keeps its own
session; DBine only runs the command and reads what it prints.

This integration follows Zoho Vault's REST API v1 documentation, Zoho's
OAuth Self Client documentation and the encryption its web client uses; it
was tested against recorded vectors and mocks of Zoho's accounts and Vault
APIs, not against a live Zoho account.

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
  **Test connection** revokes them as soon as the test's session is closed.
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
| User | As the credentials, the connection's user always comes from the account's `UserName`. |

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
| User field | Under **Configuración avanzada**: as the credentials, the connection's user comes from the secret's `username` field, or the slug chosen here. |

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

## Bitwarden

| Field | |
|---|---|
| Server | `bitwarden.com` (US cloud), `bitwarden.eu` (EU cloud), or your self-hosted server's URL (`https://bitwarden.example.com`; its `/identity` and `/api` are used). Plain `http://` only for this computer (`localhost`, `127.0.0.1`). |
| Item | The item's ID, or its exact name (else ignoring case). Items in the trash don't count. |
| Password field, user field | Under **Configuración avanzada**: the login's password and username by default; `notes`, `totp`, or the name of a custom field (text, hidden, boolean, or linked to the username or password). |
| JSON key | Optional: when the field holds JSON (often the notes), the key to use. |
| client_id, client_secret | Your **personal API key** (web app: *Settings › Security › Keys › View API key*). The `client_id` (`user.…`) is part of the definition; the `client_secret` is kept in the system keychain. |
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
client, the Bitwarden server, or the saved account it names, by its name…)
and runs it only if you confirm.

- The confirmation is kept on this computer only, next to the state
  (`secret-source-approvals.json`), never in the cloud backup or an export.
  It's bound to the connection, the field and the definition as the form
  writes it (spaces around the values and inputs that do nothing don't
  count): any other change to the definition asks again.
- What you type in the form on this computer counts as confirmed. Leaving an
  imported definition as it was, or duplicating it into a new connection,
  doesn't; and while a connection's imported definition for a field is
  unconfirmed, whatever you type over it in that connection's form also
  asks before it first runs.
- A connection whose definitions can't be read (a damaged or unknown entry)
  counts as one from elsewhere: its fields ask first.
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
sandbox. Vault, Zoho Vault, CyberArk, Delinea, Bitwarden and Bitwarden
Secrets Manager work (plain HTTPS), AWS Secrets Manager and Parameter Store
work with keys from the environment or `~/.aws/credentials`, and KeePass
reads files the sandbox can see (your home folder).

## For developers

- Definitions: `crates/dbine-core/src/secret_sources.rs` (`SecretSource`,
  stored as JSON in the connection option `secret_sources`, by field key;
  validation, the confirmation digests, JSON keys, Zoho's data centres). A
  Vault token or a master password typed in the form travels as the option
  `source_token:<field>`, Zoho's client secret and refresh token (manual
  mode) as `source_token:<field>#client_secret` / `#refresh_token` (Bitwarden's
  client secret as `#client_secret`; a Secrets Manager access token is the
  main slot), and
  `save_connection` moves to the keychain the slots the definition keeps
  (`SecretSource::token_slots`: none for a master password that's asked or
  comes from a command).
- Accounts: `crates/dbine-core/src/secret_accounts.rs` (the local-only
  `secret_accounts` table, the keychain names `secret_account:<id>#<slot>`,
  `AccountConfig` tagged by kind: `zoho`, `bitwarden`, `bitwarden_sm`,
  `vault`, `delinea`, `conjur`, `ccp`, `onepassword`, `aws`, `azure`, `gcp`,
  `keepass`) and `src-tauri/src/secret_sources/accounts.rs` (Zoho's wizard
  commands `zoho_connect_exchange` / `_finish` / `_cancel`; every other
  manager's `connect_secret_account`, which checks live and only then saves,
  and `detect_secret_account`; `open_secret_manager_page` with a fixed list
  of pages; rename and remove; and `expand`, which turns a definition naming
  an account into the inline one right before it runs, after its
  confirmation is checked). A definition names its account in
  `secret_account` (Zoho Vault's in `account`, which came first); the
  sign-in fields it leaves to the account are optional in the JSON and left
  out, so an inline definition's JSON and its confirmation digest are the
  same as before accounts (`inline_definitions_of_every_kind_are_unchanged`
  pins the digests). Each provider module has the check its wizard runs
  (`whoami`, `sm_whoami`, `conjur_whoami`, `ccp_handshake`,
  `caller_identity`, `keepass::verify`…). The UI: `AccountConnectWizard.vue`
  over `composables/accountWizards.ts` (steps, checks, what is sent; tests in
  `accountWizards.test.mjs`), `SecretAccountsSettings.vue`, and the account
  block of `SecretSourcePicker.vue`. The live test
  `secret_sources::accounts::tests::live_accounts` checks the Vault,
  Bitwarden, Conjur and AWS wizards against `dbine-test-vault`,
  `dbine-test-vaultwarden`, `dbine-test-conjur` and `dbine-test-localstack`
  (with `sts` among its services) and connects to `dbine-test-postgres`
  through those accounts; `fake_clis_for_the_wizards`
  runs the 1Password, Azure and Google wizards against fake CLIs on `PATH`.
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
| `browse_secret_source` | `{ connection_id \| null, driver, field, source, tokens \| null, path?, item?, query? }` (`source` may leave its item input empty; `path`: the group to list, `item`: the item whose field names to list, `query`: Delinea's search) | `{ entries: [{ id, name, path?, kind: 'group' \| 'item', detail? }], fields?, complete, truncated, search }` |

- Browsing (`secret_sources/browse.rs`, a `browse` function in each
  provider's module): `complete` listings come whole and the dialog walks
  their `path`s; Vault KV and 1Password list one level at a time (a group's
  `id` is the next `path`). It's registered only as a Tauri command; the
  MCP server, the assistant and the phone have no route to it. Tests run
  against the mocks and fixtures of each provider;
  `secret_sources::vault::tests::live_vault_browse` and the LocalStack tests
  (`aws`, `ssm`) browse real servers.
- Errors: `secret_source` (a provider failed; the message says which and
  why), `secret_source_approval` (not confirmed; `approval` carries
  `{ connection_id, connection_name, field, field_label, source }` for the
  dialog, the message doesn't) and `secret_source_unlock` (a master password
  to ask for; `unlock` carries `{ connection_id, connection_name, field,
  field_label, kind, wrong }`).
