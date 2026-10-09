# Integrated authentication (Windows / Kerberos)

Some engines accept signing in with the domain account (Active Directory)
instead of a database user and password. In DBine it is chosen in the
**Authentication** field of the connection form. Which engines have it and
why the others don't: [`engine-support.md`](engine-support.md#integrated-authentication-windows--kerberos).

## SQL Server

Two new options in **Authentication** (only in SQL Server: Azure SQL
Database, Microsoft Fabric and Babelfish have no Windows logins):

| Option | Asks for | Windows | macOS and Linux |
|---|---|---|---|
| **Windows: current user** | Nothing: neither user nor password | The Windows session (SSPI) | The session's Kerberos ticket |
| **Windows: domain username and password** | `DOMAIN\user` and password | NTLMv2 | NTLMv2 (without being joined to the domain) |

The domain password is stored in the system keychain, like the other
passwords; never in the state file or the logs.

### On Windows

With the computer on the domain, **Windows: current user** signs in with the
user who logged in, without asking for anything. For now the current user
authenticates with NTLM through SSPI (not with Kerberos): if the domain
blocks NTLM, the server rejects the login.

**Windows: domain username and password** is for signing in with another
domain account, or from a computer that isn't on it. The user goes as
`DOMAIN\user` (for example, `CONTOSO\ana`).

### On macOS and Linux: Kerberos

**Windows: current user** uses the Kerberos ticket the session has. To have
it:

1. The computer must be able to reach a domain controller (on the company
   network or over VPN).
2. Request the ticket, with the realm in uppercase:

   ```sh
   kinit ana@CONTOSO.LOCAL
   klist        # shows the ticket and when it expires
   ```

   On macOS the **Ticket Viewer** app also works
   (`/System/Library/CoreServices/Applications/Ticket Viewer.app`). If the Mac
   is joined to the domain, the ticket is obtained at login.
3. On Linux, `/etc/krb5.conf` must know the realm (`default_realm` and, if
   DNS doesn't publish them, the KDCs). The `libgssapi_krb5` library is
   needed (package `libgssapi-krb5-2` on Debian and Ubuntu, `krb5-libs` on
   Fedora and RHEL), which almost all distributions ship.

**Server** takes the server's full name in the domain (`sql01.contoso.local`),
not an IP or an alias: Kerberos looks for the service
`MSSQLSvc/sql01.contoso.local:1433`, and that name (SPN) must be registered
in the domain for the SQL Server service account.

If the ticket is missing, expired or there is no KDC, the connection says so
and suggests `kinit user@REALM`.

**Domain username and password from macOS and Linux:** it works without the
machine being joined to the domain or having a ticket: DBine speaks NTLMv2
with the `DOMAIN\user` user and the password. If the domain blocks NTLM, use
**Windows: current user** with `kinit` for that account.

## MongoDB

In **Authentication**, **Kerberos (GSSAPI)**: **User** takes the principal
(`ana@CONTOSO.LOCAL`) and no password is asked for. It uses the session's
ticket (`kinit` on macOS and Linux; on Windows, the Windows session). In
**Advanced**, **Kerberos service** if the server wasn't registered as
`mongodb`. Kerberos is a MongoDB Enterprise feature: FerretDB and Amazon
DocumentDB don't have it.

## ODBC engines

Each ODBC driver has its own attributes for Kerberos or Windows integrated
security. They go in **Additional attributes** and replace the attributes of
the same name that DBine builds (for example, Hive's `AuthMech`). User and
password are left empty. Some examples (check the documentation of the driver
you have installed):

| Engine | Additional attributes |
|---|---|
| IBM Db2 | `Authentication=KERBEROS` |
| Teradata | `MechanismName=KRB5` (or `TD2`, `LDAP`) |
| Apache Hive, Impala, Spark (Simba / Cloudera drivers) | `AuthMech=1;KrbRealm=CONTOSO.LOCAL;KrbHostFQDN=hive.contoso.local;KrbServiceName=hive` |
| Vertica | `KerberosServiceName=vertica;KerberosHostName=vertica.contoso.local` |

With the generic ODBC connection, the DSN or the connection string can
already carry `Trusted_Connection=yes` or the attribute the driver asks for.
