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
- **Not tested against real cloud databases** (there are none in the test
  environment): RDS / Aurora with IAM, Cloud SQL and AlloyDB with IAM,
  Azure Database for PostgreSQL / MySQL with Entra ID, the metadata servers
  and managed identity.
