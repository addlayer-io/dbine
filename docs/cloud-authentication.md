# Cloud sign-in for managed databases (AWS IAM, Google Cloud IAM, Entra ID)

Managed PostgreSQL and MySQL services accept a short-lived token as the
password: an RDS IAM auth token lasts 15 minutes, a Google or Entra ID access
token about an hour. With the **Authentication** field set to a cloud
method, DBine makes a new token right before every connection and every
reconnect, from the credentials already on the computer. Nobody pastes
tokens, and a session that reconnects after the token expired signs in
again by itself.

| Option | Engines (connection form) | Fields | Where the token comes from |
|---|---|---|---|
| **AWS IAM** | PostgreSQL, Amazon Aurora PostgreSQL, MySQL, MariaDB, Amazon Aurora MySQL | AWS profile (optional), AWS region (optional) | DBine signs the RDS auth token itself (SigV4, `rds-db:connect`), as `aws rds generate-db-auth-token` does. |
| **Google Cloud IAM** | PostgreSQL, Cloud SQL for PostgreSQL, AlloyDB for PostgreSQL, MySQL, Cloud SQL for MySQL | Google Cloud account (optional) | `gcloud sql generate-login-token` (a token limited to the Cloud SQL login); for AlloyDB, `gcloud auth print-access-token`. |
| **Microsoft Entra ID** | PostgreSQL, MySQL (Azure Database for PostgreSQL / MySQL flexible server) | Entra ID tenant (optional) | `az account get-access-token --resource-type oss-rdbms`. |

The database user is the one the cloud maps to the identity: the IAM
database user with `rds_iam` (PostgreSQL) or `AWSAuthenticationPlugin`
(MySQL), the Google account or service account, the Entra ID user or group.
With Google Cloud and Entra ID the user can be left empty: DBine takes it
from the identity (the email for PostgreSQL, without
`.gserviceaccount.com` for service accounts; the part before the `@` for
MySQL; the Entra ID `upn`).

**Test** shows the identity that signed in after the server version
(`… — identity: AWS IAM: arn:aws:sts::123456789012:assumed-role/…`). Only
the test does; the explorer, the feedback report and the database
documentation keep showing the version alone.

## Credentials

### AWS

The standard chain, in this order:

1. If the form names no profile: `AWS_ACCESS_KEY_ID`,
   `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
2. The profile (the form's, else `AWS_PROFILE`, else `default`) in
   `~/.aws/credentials` and `~/.aws/config` (or `AWS_SHARED_CREDENTIALS_FILE`
   and `AWS_CONFIG_FILE`): its static keys.
3. Its `credential_process`, run through the shell as the AWS SDKs do.
4. Anything else the AWS CLI resolves (IAM Identity Center / SSO, `aws
   login`, assumed roles, web identity, the instance or container role):
   `aws configure export-credentials --format process`. If the SSO session
   expired, the error says so (`aws sso login`).

Credentials that come from a process or the CLI are kept in memory until
five minutes before they expire. The region is the form's, else the
endpoint's (`<name>.<id>.<region>.rds.amazonaws.com`, also `cluster-`,
`cluster-ro-`, `proxy-` and `.amazonaws.com.cn` endpoints), else
`AWS_REGION`, `AWS_DEFAULT_REGION` or the profile's.

The test's identity comes from `aws sts get-caller-identity` with the same
credentials; without the AWS CLI it shows the access key masked
(`AKIA…WXYZ`).

### Google Cloud

1. gcloud with the form's account, or the active one (`gcloud auth login`).
2. If no account is named: the application default credentials (`gcloud
   auth application-default print-access-token`).
3. If no account is named: the metadata server, when DBine runs on Google
   Cloud (Compute Engine, GKE).

### Azure

1. Azure CLI (`az login`), with the form's tenant if given.
2. Azure Developer CLI (`azd auth login`).
3. Managed identity, when DBine runs on an Azure VM.

Google and Entra ID tokens are kept in memory until five minutes before they
expire (gcloud doesn't say when its tokens expire: they are kept ten
minutes). If no source works, the error lists what was tried and why each
one failed.

The CLIs are looked up in `PATH` and then in the usual install folders
(Homebrew, `/usr/local/bin`, `~/google-cloud-sdk/bin`, `C:\Program
Files\Amazon\AWSCLIV2`, the Google Cloud SDK and Azure CLI folders on
Windows), since apps opened from the Finder don't get the shell's `PATH`.
They run without a shell and with fixed arguments; the account and tenant
fields only accept the characters of an email or a domain.

## TLS

A cloud token only travels over TLS with the server certificate verified:
the cloud methods turn TLS on, and with **Trust server certificate**
checked the connection is refused before the token is made.

- **AWS:** RDS certificates are signed by Amazon RDS root CAs that no system
  store (nor webpki-roots) has. DBine carries Amazon's global bundle
  (`global-bundle.pem`, all commercial regions) and trusts it when no CA
  file is given; it does the same for Aurora PostgreSQL and Aurora MySQL
  with TLS and a password. GovCloud and China regions have their own
  bundles: give it in **CA certificate**. A CA file with several
  certificates (like the global bundle) is now read whole in PostgreSQL;
  before, only its first certificate counted.
- **Cloud SQL:** the instance's `server-ca.pem` goes in **CA certificate**.
  Its certificate names the instance, not the IP DBine dials, so with that
  CA only the chain is checked (as before for the Cloud SQL variants; now
  also for PostgreSQL or MySQL with Google Cloud IAM).
- **Azure:** the certificates chain to public roots; nothing to add.

## SSH tunnels

Through an SSH tunnel the token is signed for the real endpoint and port
(the RDS token names them), not for the tunnel's local end. PostgreSQL dials
the local end and verifies the certificate against the endpoint's name;
MySQL checks the chain only (its client can't verify a name other than the
one it dials).

## Flathub

The Flathub build can't run aws, gcloud or az (the Flatpak sandbox has no
access to the system's programs): those steps answer that they aren't
available in the sandbox. AWS keys in the environment or in
`~/.aws/credentials` still work there, when the sandbox can read them.

## Power BI and Fabric semantic models (Entra ID)

The **Power BI** engine (semantic models in the Power BI service and Microsoft
Fabric) signs in only with Microsoft Entra ID, and uses the token as the
bearer of the REST APIs: there is no database password. **Authentication**
has five methods, and every one ends in a token for the cloud's Power BI
resource (`https://analysis.windows.net/powerbi/api` in the public cloud).

| Method | Fields | How |
|---|---|---|
| **Microsoft Entra ID (browser, with MFA)**, the default | **Usuario** (optional), **Inquilino** (optional), **Aplicación para iniciar sesión** (advanced) | Authorization code with PKCE in the system browser, through a loopback redirect (`http://localhost:<port>`), so the organization's sign-in, MFA and conditional access apply. The browser opens only the selected cloud's own sign-in page, and the wait ends after five minutes. |
| **Service principal** | **Inquilino** (required), **Client ID**, **Secreto del cliente** | Client credentials with the app's secret. Certificates are not supported yet (pending: they need a signed client assertion). |
| **Default credential** | **Inquilino** (optional) | In this order: the `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET` variables, a managed identity, `az account get-access-token --resource <Power BI resource>`, then `azd auth token`. The CLIs run without a shell and with fixed arguments. |
| **Managed identity** | **Client ID** (optional: a user-assigned identity; empty is the system's) | The Azure instance metadata endpoint, or the App Service endpoint, with the Power BI resource. |
| **Access token** | **Token de acceso** (secret) | A token pasted by hand for the cloud's Power BI resource. DBine never renews it: when it expires the connection says so and a new one has to be pasted. Its audience is read locally only to give a clear message. |

Managed identities are service principals for Power BI, so everything said
about service principals applies to them.

### The browser sign-in and its app

**Aplicación para iniciar sesión** (under advanced options) picks the
registered app the sign-in uses:

- **DBine (predeterminada):** the app "DBine Database Sign-in" (a public
  client; its id is not a secret).
- **Cliente de Azure CLI:** the Azure CLI's own public client, which many
  tenants already allow. It asks for the whole Power BI resource
  (`.default`), so its token carries every permission the account has
  consented to.
- **Propia (Client ID):** an app registered in your tenant, as a public
  client with the delegated Power BI permissions and the redirect URI
  `http://localhost`. It is needed for GCC High, DoD and China, because
  DBine's app is registered in the public cloud.

The permissions DBine's app asks for are delegated and only the ones the
connection needs:

- always: `Dataset.Read.All` and `Workspace.Read.All` of Power BI, plus
  `offline_access`, `openid` and `profile`;
- `Capacity.Read.All`, to show the capacity's SKU and state;
- `Dataset.ReadWrite.All`, only when the connection is not read-only
  (refresh now, cancel, schedules, model permissions, delete);
- `SemanticModel.ReadWrite.All` of the Fabric API
  (`api.fabric.microsoft.com`), only with **Leer la definición completa del
  modelo (API de Fabric)** on, which is the default. It reads the model's
  definition and renames or describes the model; DBine never writes a model
  definition. It needs write permission on the model.

Asking for fewer permissions only avoids requesting a consent that isn't
needed: it is not what keeps a read-only connection read-only. Once a
permission has been consented, Entra puts it in every later token of that
user, and the Azure CLI client's token carries all of them. The read-only
connection refuses refresh, cancel, schedule, permission, rename and delete
calls itself, before any request.

If the tenant has not approved DBine's app (Entra answers
`AADSTS650057`, `AADSTS65001` or `AADSTS90094`), the error says so and
names the two ways out: ask an administrator to approve the app, or choose
**Cliente de Azure CLI**. A Conditional Access block (`AADSTS53003`) is
reported the same way. DBine never opens a second browser sign-in by
itself.

Connections that leave **Usuario** empty share one signed-in account per
cloud, tenant and sign-in app. Signing in with another account in the
browser, or changing the Azure CLI account with `az login` for the default
credential, moves every such connection to that account. To keep two
accounts apart, write each one in **Usuario**. A connection that names a
user refuses a sign-in as another account and asks again on the next
attempt. A token renewal that comes back as another account is refused the
same way. When **Inquilino** is a GUID, a token from another tenant is
refused.

### Tenant settings and permissions

What the Power BI tenant and the model must allow:

- **Everyone:** workspace access plus **Read** and **Build** on the model,
  and the tenant setting **Semantic Model Execute Queries REST API**
  (Integration settings).
- **executeDaxQueries (Premium, Fabric or Embedded capacity):** the tenant
  setting **Allow XMLA endpoints and Analyze in Excel with on-premises
  semantic models** as well. Without it the model falls back to
  `executeQueries`, and the editor says so once.
- **Service principals and managed identities:** the tenant setting
  **Service principals can call Fabric public APIs** (Developer settings),
  the principal added to the workspace (Viewer with Build on the model, or
  Contributor and above) or given Build on the model, and no admin-consented
  Power BI application permissions on its app. `executeQueries` doesn't
  serve a service principal on a model with row-level security or single
  sign-on, and on `executeDaxQueries` a service principal can test roles only
  as a workspace admin. A service principal's refreshes and schedules go
  without e-mail notification.
- **Reading the full definition** (measures, relationships and hierarchies on
  Pro workspaces): read and write permission on the model.

### Clouds

**Nube** picks the sign-in host and the API host together, from a fixed
table (the API host is never a free field, so a token can't be sent
elsewhere): Public (`api.powerbi.com`), US Government GCC, GCC High and DoD,
and China (21Vianet). DBine's app serves the public cloud and GCC; GCC High,
DoD and China need **Propia (Client ID)**. The Fabric definition API is
offered in the public cloud only. The sovereign clouds are untested.

## Secrets

Tokens and cloud credentials live in the memory of the driver process only.
The same credential chains also read passwords from AWS Secrets Manager,
Azure Key Vault and Google Secret Manager
([`secret-sources.md`](secret-sources.md)).
They are never written to the state file, the keychain or the logs, never
shown in an error, and the phone app and MCP clients never receive them
(they only reach the database's login). The connection keeps the method and
the optional profile, region, account or tenant; with a cloud method there
is no password to save or ask for.

## Verified

- Unit tests: the SigV4 presigning against the AWS signing test suite
  (`get-vanilla`, `get-vanilla-query-order-key-case`,
  `get-vanilla-with-session-token`, query-string variants); RDS tokens
  identical to the ones the official AWS CLI (2.36) makes with the same
  keys and instant, with and without session token; HMAC-SHA256 against RFC
  4231; region parsing; `~/.aws` profiles; `credential_process` / CLI JSON;
  Azure CLI and Azure Developer CLI output; gcloud output; Entra ID token
  claims; the fields each variant offers; the refusal over an unverified
  server; that the password is only asked when the sign-in takes one.
- On this machine: the Azure CLI step returns an `oss-rdbms` token and the
  identity is read from it; the AWS step calls the AWS CLI for an `aws
  login` profile and reports its expired session.
- Against the `dbine-test-postgres`, `dbine-test-mysql` and
  `dbine-test-mariadb` containers: the password sign-in is unchanged.
- Power BI: unit tests with fixtures shaped like Microsoft Learn's samples
  and fake token endpoints (scopes per connection, the host allowlist, the
  account binding, consent errors). No Power BI tenant was available: the
  sign-in with DBine's app and with the Azure CLI client, the service
  principal, managed identity and the sovereign clouds were not tried against
  real services.
- **Not tested against real cloud databases** (there are none in the test
  environment): RDS / Aurora with IAM, Cloud SQL and AlloyDB with IAM,
  Azure Database for PostgreSQL / MySQL with Entra ID, the metadata servers
  and managed identity.
