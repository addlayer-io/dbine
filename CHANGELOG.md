# Changelog

## [Unreleased]

### New
- **Results kept when DBine closes:** each query editor's result tabs are saved to an encrypted file on your computer, so after closing DBine, or a crash, every editor shows them again as they were, pinned ones included, without running anything. The key never leaves your computer and user; **Settings › General** sets the space they may take, clears them, or turns it off.

## [1.5.0] - 2026-11-11

### New
- **AI in Compare schemas:** columns and tables that look renamed are pointed out, so they can be renamed and keep their data instead of being dropped and added, and **Find renames with AI** finds the ones whose names don't look alike. In the sync dialog, **Review with AI** has the assistant check the script for what can fail or lose data, measuring it with reads you approve, and **Summarize with AI** describes the changes for an approval request or a ticket.
- **AI in Compare data:** **Explain differences** tells what the differences have in common and their likely cause, such as a time zone, a rounding or a sync running late, from counts and typical gaps over all of them. **Filter with AI** turns a request in words into the column filters and the order, and **Suggest key** finds the key of a table without a primary key. The AI never gets the rows' values.
- **Sort and filter in Compare data:** with thousands of differences, sort by any column and filter each one from its three dots (equal, different, greater, contains…), or keep only the rows where a column changed. It works on all the rows, not only those on screen, and "All" then marks only the rows that pass.
- **Result tabs in the query editor:** an earlier run's results can stay in a tab of their own, with its query and the time it ran, so you can go back to them without running the query again. **Pin** a result and later runs open next to it instead of replacing it; **Settings › General** sets how many unpinned results each editor keeps, and a tab's menu runs its query again, copies it or closes the unpinned ones.
- **SSH tunnels from `~/.ssh/config`:** a tunnel can use a `Host` of your SSH config instead of loading the server by hand. DBine uses what OpenSSH resolves for it (server, user, keys, agent), including `ProxyJump` chains where each server signs in with its own user and keys, and `ProxyCommand`, which runs only after you confirm the exact command. It shows the way before you connect.
- **Contexts for the AI assistant:** a new **Contexts** node on each connection and database holds notes the assistant gets in every chat there, such as what a status code means or how a metric is calculated, so you don't repeat them. **Settings › AI assistant** has a text for every conversation. The chat shows which ones are in use and can turn one off for that conversation. They guide the assistant; they don't change what it's allowed to do.
- **KDB+ data sync and clone:** **Compare data** finds and fixes the rows that differ between two kdb+ tables, hashing them on each server so only the changed rows travel, and a migration can clone a kdb+ process into another with its tables, keys, types, attributes, linked columns, functions and views.
- **KDB+ namespaces as schemas:** the explorer groups each namespace's tables, views and functions under it, **New schema…** and **Drop schema…** create and drop namespaces, and migrations into kdb+ keep each source schema as a namespace.
- **KDB+ health check and properties:** the health check reads the process itself (memory against the `-w` limit, garbage collection, interned symbols, slow subscribers, no query timeout, a port without a password, TLS), and **Properties** shows and changes the settings kdb+ lets you change while it runs (`\g`, `\T`, `\P`, `\o`, `\z`, `\w`).
- **KDB+ attributes as indexes:** `s#`, `u#`, `p#` and `g#` show in a table's structure and can be set and removed from the table designer, compare and sync; schema conversion turns them into indexes and back.
- **KDB+ renames:** **Rename…** works on views, functions, variables and namespaces too, besides tables and columns.

### Improvements
- **Schema compare filters by status:** each badge above the list (different, only left, only right, equal) now turns its rows on and off, so you can see only what's equal, or only what's on one side.
- **KDB+ notebook writes and row counts:** a notebook's write cell writes into kdb+ tables in every mode (insert, update by key, insert or update, replace), applied in one step so a failure leaves the table as it was, and the explorer's documentation and migrations get each table's row count without scanning.

### Fixes
- **Security fix in the built-in SQLite:** DBine's own SQLite, used for its settings and for SQLite and libSQL files, is updated to a version that fixes CVE-2026-119375.
- **Security fixes in two libraries:** the Arrow library of the DuckDB driver, and a system library used by DBine and by its DuckDB, SAP HANA and MySQL drivers, are updated to versions that fix memory-safety issues (RUSTSEC-2026-0442, RUSTSEC-2026-0486).
- **SQL Server Windows sign-in (NTLM):** a malicious or intercepted server could make DBine's NTLM answer hide its own MIC flag and service name, or make the SQL Server driver ask for gigabytes of memory. DBine now writes those lengths correctly and checks the server's offsets.
- **AI assistant and other databases:** with Claude Code or Codex, the assistant asks before each structure read the model picks, and it reads another database of the connection only when the connection lists it. Text the server wrote (a table comment, an object's name) can no longer pass itself off as a tool's answer or as your message to the model.
- **SSH profiles from a backup:** saving an SSH profile restored from a backup no longer confirms its command source without asking, and a backup whose profile has an id that isn't a profile's is refused instead of restored.
- **KDB+ reserved names:** DBine never writes into kdb+'s own namespaces (`.z`, `.Q`…), the root or a file, also when a malicious server's name comes bare (`.z`), padded with blanks or as a file handle (`:/path`), as in a schema compare's sync script.
- **Sync scripts between engines:** numbers read from PostgreSQL went into the script as text, so MongoDB and other engines that keep types could store `"9"` instead of `9`, or refuse the script. Each value is now written the way its target column takes it.
- **Compare data with booleans across engines:** PostgreSQL's `true` and `false` didn't match another engine's, so every row of a table with a boolean column showed as different, and the sync script could carry a value the target's column doesn't take. They now compare as what they are.
- **Compare data on large tables:** when a table has more rows than a comparison reads (200,000 per side), each side could read different rows, so a row could show as missing from the other side when it wasn't, and the sync script could delete or insert it. Both sides are now read in key order, and a partial comparison only updates rows found on both sides.
- **Large results in Databricks:** a query whose result passed 25 MiB failed with "Inline byte limit exceeded", even when only its first rows were shown. It now brings the rows you asked for and marks them as partial, and exports and charts read the whole result.
- **Databricks result downloads:** when a part of a result couldn't be downloaded, the error showed its link, which carries a temporary credential. It no longer does.
- **SSH profile picker:** in a connection's SSH tunnel tab, the profile list had shrunk to an empty box. It now takes the row's width and says when the connection uses its own settings.
- **Schema compare with a schema selected:** full-text catalogs and stoplists no longer disappear from the comparison, so their differences show and the script can create the catalog an index needs.
- **Syncing SQL Server full-text indexes:** an index carried to a table whose primary key has another name now uses that table's key, and a difference in that name alone no longer shows as a change. Dropping a full-text catalog or stoplist in schema compare now lists the indexes that use it.
- **Migrations into KDB+:** emptying a kdb+ table to copy it again (or to resume a cut copy) keeps its columns, types and key, instead of leaving it without columns.

## [1.4.2] - 2026-11-04

### New
- **Old SQL Server versions over TLS 1.0/1.1:** DBine connects to SQL Server 2012 and earlier when the server only speaks TLS 1.0 or 1.1. It always tries TLS 1.2 or later first. If the server only accepts an old protocol, DBine says so (naming the server's version) and asks once whether to connect anyway; on yes, it downloads the **SQL Server (TLS 1.0/1.1)** component and remembers the choice for that connection. A **Legacy TLS** indicator shows while it's in use, the certificate is still verified (or pinned with **Trust server certificate**), and the option can be turned off in the connection's SSL tab. Updating the server to TLS 1.2 (SQL Server 2012 SP4) remains the safer fix.

### Improvements
- **Engine logos:** 51 more connection types show their product's logo instead of initials, among them IBM Db2, Informix and Netezza, Vertica, Exasol, InterSystems IRIS, Microsoft Access, Greenplum, StarRocks, OceanBase, TDengine, Materialize, Memgraph, RisingWave and Dremio.

### Fixes
- **Engines without databases (SQLite, libSQL, DuckDB files, KDB+…):** their connection's menu now has **Search the database**, **Health check** and, where the engine has them, **Properties**, which were offered only on database nodes; the command palette offers search and the health check for them too.

## [1.4.1] - 2026-11-03

### New
- **Automatic updates:** DBine checks for a new version when it starts and every 6 hours, downloads and verifies it in the background, and shows **Restart to update** in the status bar when it's ready. If you just quit, the update installs then, and the next start is the new version, with a summary of what's new. Installs that can't update themselves (deb, rpm, MSI, Flatpak) keep the notice with the download link. `DBINE_AUTO_UPDATE=0` turns automatic updates off on managed machines.
- **q in the editor:** KDB+ queries get q syntax highlighting and completion of keywords, built-in functions, tables, columns, functions and namespaced names such as `.u.upd`. ⌘/ comments lines with `/`, and there are more q snippets.
- **KDB+ schema compare, sync and conversion:** compare and sync tables in memory (add, drop, retype and rename columns, change the key), rename tables, and convert schemas between kdb+ and the other engines, with q types and keyed tables mapped to primary keys. Partitioned and splayed tables are refused, with the reason.

### Improvements
- **KDB+ foreign keys, bulk load and charts:** linked columns show as foreign keys in the ER diagram and in jump by key, imports and migrations load rows as typed tables over IPC (100,000 rows in about a second), and charts are aggregated on the server over the whole result.
- **KX logo:** KDB+ connections show KX's logo.

## [1.4.0] - 2026-11-03

### New
- **KDB+ support:** connect to kdb+ processes (plain or over TLS) and work in q: browse the tables, views, functions and variables of every namespace, run q scripts statement by statement, filter, sort and edit table rows from the grid, and watch memory and open connections in the Monitor. On kdb+ 3.6 and later, read-only connections are also enforced by the server (`reval`); on older servers a read-only connection only opens tables and variables. **Copy connection string** gives the q `hopen` handle and PyKX.
- **Databricks sign-in through the browser:** Databricks and Azure Databricks connections can sign in with **Browser sign-in (SSO)**: DBine opens the workspace's sign-in page (Microsoft Entra ID on Azure), so your organization's SSO and MFA work without a personal access token or a service principal. The session renews itself while DBine is open.

### Improvements
- **Permissions to grant come from the server:** in **Users and permissions**, the list of permissions is what that server can grant, read from its own catalog where it has one (SQL Server's `sys.fn_builtin_permissions`, PostgreSQL's ACLs, MySQL's `SHOW PRIVILEGES`, ClickHouse, Oracle, SAP HANA, Redis, MongoDB, Neo4j, Couchbase, OpenSearch…) and from the engine's documentation elsewhere, so it follows the server's version: SQL Server now offers `VIEW DATABASE STATE`, `VIEW DATABASE PERFORMANCE STATE`, `SHOWPLAN` and the rest. It shows only what applies to the target you pick (the whole database, a schema, a table, a procedure…), and the same list feeds **New schema…**. A permission can still be typed.

### Fixes
- **Safer spreadsheet import:** reading `.xls` and `.xlsb` files uses calamine 0.38, so a crafted file can no longer crash DBine or exhaust its memory (RUSTSEC-2026-0436).
- **Text fields keep exactly what you type:** user names, passwords, host names, connection URLs, searches, filters and every other text field no longer get their first letter capitalized, autocorrected or spell-checked by the operating system, so `demo_user` stays `demo_user`. Shift and Caps Lock work as always. The same applies to the connection filter in the phone app.

## [1.3.1] - 2026-11-02

### Improvements
- **Recovering a connection after a network cut:** a connection that failed or is stuck connecting can now be disconnected from its menu, which closes its sessions and its SSH tunnel; connecting again starts from scratch. A query that fails because the connection broke now drops its session and, when the SSH tunnel no longer reaches the server, the tunnel too, so the next query reconnects on its own. Disconnecting asks first when a tab has an open transaction or a statement still running, and an SSH tunnel's connection and sign-in can no longer wait forever.
- **Clearer message when updates can't be checked offline:** without an internet connection, checking for or downloading an update now says the update server can't be reached, instead of showing the network library's error.

### Fixes
- **SSH tunnels, security:** the SSH library is updated past several advisories, the most serious letting a key held in the SSH agent sign without the agent asking for confirmation; a server can no longer stall a tunnel or make DBine use unbounded memory with crafted packets.

## [1.3.0] - 2026-10-30

### New
- **SSH tunnel profiles:** save an SSH tunnel (server, bastions, user and authentication) once as a profile in Settings and pick it in any connection; each connection keeps its own database server. Editing the profile applies to every connection that uses it on its next connect, a connection's current tunnel can be saved as a profile, and the profile's secrets stay in the system keychain.
- **Table columns on hover:** resting the pointer on a table's name in the SQL editor shows its columns with their type, whether they allow nulls, the primary key and each column's comment, for every engine.

### Improvements
- **AI assistant with Claude Code and Codex:** when you ask about another database on the same connection, the assistant reads its tables and columns without switching the tab's database. These models still never receive table rows.

### Fixes
- **SAP HANA on Linux, security:** cancelling a query no longer reads the folder of trusted certificates after the session has removed it, a moment another user of the same computer could have used to plant their own certificate authority.
- **Security, dependencies:** `ctutils` and `crypto-bigint` are updated past their constant-time fixes.
- **Excel export of very long text:** a cell longer than Excel's limit is cut to fit again when it holds characters such as emoji, instead of stopping the export.
- **Trino and Presto, security:** copying or exporting data from a table whose column type is nested deeper than any real schema can no longer crash DBine; the depth limit added in 1.2.2 now covers that path too.

Thanks to [@dylanwu](https://github.com/dylanwu), whose detailed reports led to the three new features in this release.

## [1.2.2] - 2026-10-28

### Fixes
- **Security: critical vulnerability in ZIP handling fixed.** On October 28 a critical vulnerability was published in `zip`, the library DBine uses to read and write ZIP-based files: Excel imports and exports, and the download of the AI assistant's built-in model ([GHSA-m665-v7pm-7grj](https://github.com/advisories/GHSA-m665-v7pm-7grj)). A specially crafted archive could show different contents depending on how it was read. This version moves every part of DBine to the fixed library; it ships a few hours after 1.2.1 for that reason alone, so updating is recommended.
- **Trino and Presto, security:** a column type nested deeper than any real schema (as a malicious server could send) is shown as it came instead of being parsed level by level, so it can no longer crash DBine.

## [1.2.1] - 2026-10-28

### Fixes
- **Column completion follows the query you're writing:** the editor offers only the columns of the tables, CTEs and subqueries the statement under the cursor reads, not those of other statements in the file. A column that two of those tables share is offered once per table, labeled with its alias and inserted qualified (`c.customer_id`), and a CTE's or a derived table's columns are offered in the query and after its alias.
- **Field names in rows and structs:** Trino and Presto `ROW` values, PostgreSQL composite types and Arrow Flight SQL structs show each field under its name, as JSON, also in nested rows, arrays of rows and maps whose values are rows (`[{"customer_id":42,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]` instead of `[[42,[["demo-product",2]],null]]`). `null`, an empty string and an empty array stay distinct. ClickHouse named tuples keep their declared field order.
- **Migrations with a SQL Server login without VIEW DEFINITION:** when the source login could read the tables but not their definitions (common on Azure SQL), the migration script wrote `CHECK ()`, which failed with "Incorrect syntax near ')'", and dropped column defaults without saying so. Now those CHECKs, defaults and computed expressions are left out of the script, and the preview and the run report list each one at the top, with the reason and the `GRANT VIEW DEFINITION` that lets DBine read them. Generated scripts and schema sync say it too, and a sync no longer drops a CHECK on the target because the source couldn't read its own. No engine writes an empty CHECK or DEFAULT anymore.

## [1.2.0] - 2026-10-27

### New
- **Ukrainian and Polish:** DBine and the phone app are available in Українська and Polski, including server messages and the AI assistant. Pick the language in **Settings › General › Language**, or DBine follows the system's.
- **Indonesian:** DBine and the phone app are available in Bahasa Indonesia, including server messages and the AI assistant. Pick the language in **Settings › General › Language**, or DBine follows the system's.

### Improvements
- **Completion across databases:** on engines where one query can read another database of the same connection (ClickHouse, MySQL and MariaDB, SQL Server, Snowflake, Databricks, Trino, BigQuery, Oracle and more), the editor offers the other databases' names, their tables after `other_db.` and their columns after a table or an alias, also in joins between databases. Each database's tables are read the first time you type its name with a dot.
- **DuckDB engine updated to 2.0.0:** local DuckDB files and DataFrame SQL in notebooks now run on DuckDB 2.0.0 (up from 1.5.5). New database files use DuckDB 2.0's storage format, which older DuckDB versions cannot open; existing files continue to open and remain readable by the version that created them.
- **Connection encryption for SQL Server, PostgreSQL and MySQL:** the connection form shows each engine's own TLS options, with the names its tools use: `Encrypt` (Optional, Mandatory, Strict), `TrustServerCertificate`, `ServerCertificate` and `HostNameInCertificate` for SQL Server and Azure SQL; `sslmode` and `sslrootcert` for PostgreSQL and its family; `ssl-mode` and `ssl-ca` for MySQL and its family; `ssl`, `ssl-verify-server-cert` and `ssl-ca` for MariaDB and OceanBase. New connections are encrypted and check the server's certificate, and each setting can be turned off. Saved connections keep working exactly as before. Through an SSH tunnel the certificate is checked against the real server's name, and a certificate failure says which option to change. MySQL's `PREFERRED` falls back to an unencrypted connection only when the server doesn't offer TLS, before any password is sent.
- **Connection encryption for InfluxDB, IoTDB, ksqlDB, TDengine, Phoenix, Arrow Flight SQL and the cloud services:** the connection form shows each engine's own TLS options: `ssl` and `unsafeSsl` for InfluxDB 1, the URL scheme with `skip-verify` (InfluxDB 2) or `tls-no-verify` and `tls-ca` (InfluxDB 3), `use_ssl` and `ca_certs` for IoTDB and TimechoDB, `useTls`, `verifyHost` and `trustStore` for ksqlDB, `useSSL` for TDengine, the URL scheme, `hostname_verification` and `truststore` for Phoenix and Avatica, `useEncryption` and `tlsRootCerts` for Flight SQL, and `DisableServerCertificateValidation` for the Cosmos DB emulator; where an engine's tools have no name for skipping the check or for the CA file, curl's `insecure` and `cacert`. New connections are encrypted and check the server's certificate, and each setting can be turned off. Saved connections keep working exactly as before. Through an SSH tunnel the certificate is checked against the real server's name, and a certificate failure or a server without TLS is reported as such. BigQuery, Spanner, DynamoDB, Athena, Snowflake, Databricks and libSQL always use verified HTTPS, except an emulator endpoint typed as `http://`.
- **Keyboard and screen readers:** a visible focus ring shows where the keyboard is; list rows, cards, sortable headers and collapsible sections can be reached with Tab and used with Enter or Space; context menus work with the arrow keys; icon-only buttons have names a screen reader reads; the result of a query (finished, or failed with its error) is announced; animations stop when the system asks for reduced motion; and secondary text has more contrast in both themes.

### Fixes
- **"Don't verify" now pins the server's certificate, security:** a connection that doesn't verify the server's certificate (`sslmode=require`, `insecure`, `bolt+ssc`, `validate=false`…) accepts the certificate the server presents the first time and from then on only that one, on PostgreSQL and its family, the HTTP engines (ClickHouse, Elasticsearch and OpenSearch, CouchDB, Trino, InfluxDB and more), Neo4j, Neptune, Redis and Cassandra. If the certificate changes, the connection stops and shows both fingerprints, with **Trust the new certificate** for a server that renewed it; the SSL/TLS tab shows the pinned fingerprint, with **Forget**. Saved connections pin on their next connect. `verify-ca` and the options that skip only the host name now check the whole chain against the CA instead of accepting any certificate. SQL Server, MySQL, MariaDB, MongoDB and SAP HANA keep their clients' own behaviour.
- **New databases show up without reconnecting:** a database created with `CREATE DATABASE` in a query (or dropped, or renamed) appears in the explorer and the database lists at once, and opening a database list (migration, compare, subset, backups, permissions, the query's database picker…) reads it again, so one created from another tool shows up too.
- **Importing from SQL Server Management Studio:** besides the registered servers, DBine now imports the servers SSMS's Connect dialog remembers (its connection history), with their login, database and encryption settings, from SSMS 18 to 20 and the new folders of SSMS 21 and later. On macOS and Linux, the file dialog now accepts `RegSrvr.xml`, `UserSettings.xml` and `.regsrvr` files (and DataGrip's `dataSources.xml`); before, only JSON files could be picked.
- **Importing connection strings keeps their encryption settings:** a pasted or imported string now brings its TLS options as the engine's own: SQL Server's `Encrypt` (including `Strict`) and `HostNameInCertificate`, PostgreSQL's `sslmode` as written (`require` no longer becomes a verified connection), MySQL's `ssl-mode` and Connector/J's `useSSL`, MariaDB's `sslMode`, and the `rediss://`, `bolt+s://` and `bolt+ssc://` schemes. A string without TLS settings imports as its own client would connect. The same applies to connections imported from DBeaver, DataGrip and the other JetBrains IDEs, DbGate and Azure Data Studio.
- **Certificate errors on macOS and Windows:** a server certificate the system doesn't trust ("The certificate was not trusted", "issued by an authority that is not trusted", "The target principal name is incorrect") is now recognized as such, and the message says which TLS option to change instead of showing a generic TLS error.
- **ClickHouse 22.8 and older:** connecting no longer fails with `Unknown setting http_write_exception_in_output_format`; DBine stops asking for that setting on servers that don't have it.
- **Notebooks, Python export:** a PostgreSQL connection that checks the server's certificate is exported with `sslmode=verify-full`, as **Copy connection string** does, instead of `require`, which doesn't check it.
- **HashiCorp Vault, security:** a password source's Vault address must be `https://`; plain `http://` is accepted only for a Vault on this computer, as for the other secret managers, so the Vault token never travels in clear over the network.

## [1.1.0] - 2026-10-21

### New
- **Cloud sync status at a glance:** the status bar shows **Synced** with a green dot, **Pending** in amber while a change waits to upload, and red on an error; hover for the last sync time, click for the sync settings.
- **Copy connection string:** right-click a connection (or use the button in its form) and pick **Copy connection string…** to get it as a URI, ADO.NET, JDBC, SQLAlchemy, ODBC, libpq, Node.js or Go string, whichever apply to its engine, for any database, with its TLS and sign-in settings. The password is left out (a `${DB_PASSWORD}` placeholder) unless you turn on **Include password**; then it's fetched when you copy, and cleared from the clipboard a minute later if it's still there.
- **Send a message** step in scheduled tasks: post how the run went to Microsoft Teams, Slack, Telegram or ntfy, with the task's variables (`{results}` lists what each step did), **Only if…** like the email step, and a **Test** button. The webhook URL and tokens are kept in the keychain, never in the task, and messages go only over HTTPS to the service's own servers.

### Fixes
- **SSH tunnels, security:** the SSH library is updated to fix two advisories (CVE-2026-111061, a denial of service through the SSH agent, and CVE-2026-111062).
- **RSA keys, security:** signing in to SSH with an RSA key, and opening Zoho Vault's and Bitwarden's organization keys, now run in AWS-LC in constant time, so the timing leak in the RSA library (RUSTSEC-2023-0071, "Marvin") no longer applies. RSA keys sign with SHA-256 or SHA-512; a server that only accepts SHA-1 (`ssh-rsa`) asks for an Ed25519 or ECDSA key, or the SSH agent.
- **TLS, security:** the TLS library and its certificate checks are updated in DBine and in 33 drivers (RUSTSEC-2026-0346 to 0349 and 0381 to 0383), so certificates from distrusted authorities are refused. The updated drivers download by themselves.
- **Built-in AI engine:** unpacking the engine keeps only the files' read, write and run permissions.
- **Scheduled tasks, security:** `{results}` and `{alerts}` only fill in the text of mails and messages. A script's SQL, a file or folder name, an address or an attachment path no longer takes them, so a database error echoing crafted data can't turn into SQL or a path.
- **Scheduled task messages:** `{results}` and `{alerts}` in a message or a mail come in DBine's interface language instead of always in Spanish, also when the task runs with DBine closed.
- **Notebooks, big DataFrame SQL results:** **Advanced export… › All** writes the whole result, not just the rows shown. The cell's SQL runs again in DuckDB over the DataFrames its last run read, without reading files or querying the sources again. Its charts start computed over the whole result, and you can switch that off. When a result is cut, the grid also says that sorting and **Copy all** only cover the loaded rows.

## [1.0.0] - 2026-10-19

### New
- **Light theme:** a light theme and a **Same as system** option (the default) that follows your OS as it changes; interface zoom with Cmd/Ctrl + "+", "−", "0" or the mouse wheel, and separate font sizes for the editor and the results grid, in **Settings › General**.
- **Vim mode and your own shortcuts:** Vim mode for the SQL editor (Settings › General), and every keyboard shortcut can be changed in **Settings › Keyboard shortcuts**, with search, reset and conflict warnings.
- **Split editor:** up to three editor groups side by side or one above the other (the tab's menu, the tab strip's button or Cmd/Ctrl+\); drag tabs between groups or onto a group's right or bottom half to split it. The layout is restored on restart.
- **Cloud sign-in for managed databases:** PostgreSQL and MySQL connections to Amazon RDS and Aurora, Google Cloud SQL and AlloyDB, and Azure Database can sign in with **AWS IAM**, **Google Cloud IAM** or **Microsoft Entra ID**. DBine makes a fresh token from your AWS profile (SSO included), gcloud or az before every connection, so there are no more tokens to paste that expire in 15 minutes, and **Test** shows which identity signed in.
- **Passwords from a command or a secret manager:** any secret field of any engine, and the SSH tunnel's password and key passphrase, can take its value from a command, 1Password, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault or Google Secret Manager. It's fetched at each connection and kept in memory only, with a JSON key option and a per-field Test that shows only the length. A command or secret-manager setting that came from an import or a restored backup runs only after you confirm it on this computer.
- **KeePass and Zoho Vault as password sources:** any secret field (and the SSH tunnel's) can take its value from an entry of a KeePass database (.kdbx, read by DBine itself; master password from the keychain, asked when connecting or from a command; optional key file) or from a Zoho Vault secret (any data centre, decrypted on your computer with your master passphrase), with the connection's user taken from the same entry if you want.
- **Vault database credentials, CyberArk and Delinea as password sources:** connection passwords can come from HashiCorp Vault's database secrets engine (credentials made for the connection, renewed while it's open and revoked when it closes; AppRole sign-in for Vault too), CyberArk Conjur, CyberArk's Central Credential Provider (with client certificates) and Delinea Secret Server.
- **Bitwarden, Bitwarden Secrets Manager and AWS Parameter Store as password sources:** any secret field (and the SSH tunnel's) can take its value from a Bitwarden item, personal or an organization's, on bitwarden.com, bitwarden.eu or your own server, decrypted on your computer with your master password; from a Bitwarden Secrets Manager secret (with a machine account's access token); or from an AWS Systems Manager parameter (SecureString, String or one element of a StringList), with a JSON key and the connection's user from the same item if you want.
- **Credentials first, and browse your secret manager:** the connection form asks for **Credentials** first: keep the password in the keychain, ask for it every time, or take the user and password from a command line or a secret manager, whose fields replace them. A **Browse** button lists a manager's entries and their field names so you can pick them instead of typing references; it shows names only, never values.
- **User and password from the entry:** with KeePass, Bitwarden, 1Password, Zoho Vault, Delinea or CyberArk CCP as the credentials, both the user and the password come from the entry, and **Advanced settings** lets you pick other fields from the entry's own list. Every source asks for its sign-in first and the entry last, and Browse is ready once that's filled.
- **Zoho Vault accounts, connected once:** a guided **Connect Zoho Vault** assistant walks you through Zoho's console, finds your data centre and checks your master passphrase; then any connection just picks the account. Accounts stay on this computer and are managed in **Settings › Secret managers**.
- **Accounts for every secret manager:** Bitwarden, Bitwarden Secrets Manager, HashiCorp Vault and OpenBao, Delinea, CyberArk Conjur and Central Credential Provider, 1Password, AWS, Azure Key Vault, Google Secret Manager and KeePass now connect once in **Settings › Secret managers** with a guided assistant that explains where each value is, checks the account live and shows who signed in; then any connection just picks the account, and credentials only go to that manager's own server.
- **Azure tenants to choose from:** the Azure Key Vault account assistant lists your Microsoft Entra tenants (you can still type one), and when you aren't signed in to the chosen tenant it shows the `az login --tenant` command to copy.
- **Autocomplete in DataFrame SQL cells:** the cells above are suggested as tables (even ones that haven't run yet), with their columns after `name.` or an alias and in the select list, and DuckDB's file readers after FROM and JOIN; picking a reader puts the cursor between its quotes, where **Choose file…** opens the file dialog. The table list after FROM no longer closes by itself while the notebook refreshes.
- **Map view:** results with geometry columns (PostGIS, MySQL, SQL Server, SQLite/SpatiaLite, DuckDB, MongoDB, Elasticsearch and more) or latitude/longitude columns can be shown on a map, drawn on this machine over a built-in offline world map. Detailed online map tiles can be turned on in **Settings › Maps**; the provider then sees which area you're looking at, never the data.
- **Oracle on the map:** SDO_GEOMETRY columns now show on the results map, both in table data and in your own queries (converted on the server to WKT with their SRID); those cells are read-only in the grid.
- **Open data files directly:** SQLite, DuckDB, CSV, TSV, Parquet, JSON and Excel files open by double-clicking them or using "Open with DBine", by dropping them on the window, or with **File › Open…** (Cmd/Ctrl+O). Each one opens as a temporary connection you can keep with **Save connection**, and data files show their first 1,000 rows right away.
- **Command palette:** Cmd/Ctrl+Shift+P finds and runs any action, showing only what the current engine supports, and **Go to…** (Cmd/Ctrl+P) jumps to connections, databases, tables, views, routines, saved queries and open tabs. Every palette action can also be given its own shortcut.
- **Jump to related rows:** in a table's data or a query's results, **Go to referenced row** (or Cmd/Ctrl+click) opens the parent table filtered by that foreign key, and **Rows that reference it** opens each child table filtered by the value, composite keys included. On every engine with foreign keys.
- **SQL notebooks:** **File › New notebook** opens a notebook: SQL cells that run on any connection, each keeping its result and chart under it, and text cells to explain them. It's saved as a standard Jupyter `.ipynb` that GitHub, VS Code and Jupyter show, and Azure Data Studio and Jupyter `%%sql` notebooks open too. The file keeps a preview of each result (100 rows and 1 MB per cell, in **Settings › Notebooks**) with its total rows, time and duration, never connection details or passwords; inside a project, notebooks are saved without results unless you turn them on. The assistant can explain a result, propose the next cell or a grouped version of a large query, without seeing the rows. Export to HTML or Markdown.
- **Charts over the whole result:** charts are computed over every row of the result, not only the rows loaded in the grid. On SQL engines the grouping runs on the server, in a notebook's DataFrame SQL cells DuckDB does it, and on the other engines DBine does it.
- **More chart kinds:** histogram, heatmap, box plot (with whiskers and outliers), bars with lines, and KPI (the total of the whole result).
- **Several charts per notebook cell:** a notebook cell can show more than one chart of its result, and it remembers which source its map uses.
- **Charts in notebook exports:** notebooks exported to HTML or Markdown include their charts as images.
- **Join results from different databases in a notebook:** each cell's result is a DataFrame with the cell's name (shown and editable in its header), and a **DataFrame SQL** cell queries and joins the DataFrames above it as tables with DuckDB's SQL and no connection, so one cell can join a PostgreSQL result with a SQL Server one and a MongoDB one. It runs in a DuckDB database in memory that can't write files or reach the network, and is dropped when the notebook closes; each source's rows come from its run in this session or its query run again, read-only, on its own connection (up to 1,000,000 rows per DataFrame, in **Settings › Notebooks**).
- **Notebooks as scheduled reports:** a scheduled task can run a notebook with DBine closed (**Run a notebook**), save the executed copy and export it to HTML or Markdown, and the next **Send an email** step can attach it or send the HTML export as the email's body. Nothing is asked unattended: it reads only files with **Always allow** for that notebook, runs on environments that ask for confirmation only with the step's explicit permission, and its cells that change data join the task's approval. Scheduled exports include tables; charts are included when exporting from the app.
- **Local files in DataFrame SQL cells:** a DataFrame SQL cell can read CSV, Parquet, JSON or NDJSON and Excel files on your computer with `read_csv`, `read_parquet`, `read_json` and `read_xlsx`, including patterns such as `data/*.csv` and lists of files, geographic files (GeoJSON, Shapefile with its companion files, GeoPackage, KML, GPX, FlatGeobuf) with `st_read`, and tables of SQLite and DuckDB database files with `sqlite_scan` and `read_duckdb`, each one a DataFrame it can join with the results of cells from any engine; geometries keep working with spatial functions such as `ST_Contains` and show on the map. Files are only read, never written. Before reading a file for the first time, the notebook asks: allow it for this run, always for this notebook on this computer (a pattern covers its new files), or deny it; credentials, keys, shell history and system files are never read. If a file's content changed since the last run, the result says so. Settings › Notebooks lists the files each notebook may read, and lets you remove them. The same readers also read files from the web (`https://`) and from Amazon S3, Google Cloud Storage and Azure Blob Storage (`s3://`, `gs://`, `az://`, `abfss://`, with patterns such as `s3://bucket/2026/*.parquet`), with the same approval, which shows the server first; DBine downloads them itself to a private temporary folder that is removed after the read, using this computer's AWS, Google Cloud or Azure sign-in (an AWS profile can be chosen in Settings › Notebooks), and never reaches local or private network addresses, follows redirects to other servers or accepts credentials in the URL. A GeoPackage open in another program now includes the changes still in its `-wal`.
- **Export a notebook as a Python notebook:** **Export › Python notebook (.ipynb)** writes a Jupyter notebook that reproduces it: one SQLAlchemy engine (or the engine's own Python client, such as pymongo or redis) per connection, each query cell read into a pandas DataFrame with the cell's name, DataFrame SQL cells run with DuckDB over those DataFrames, and text cells as Markdown. Passwords and tokens are never exported: the code reads them from environment variables, which the first cell lists with the packages to install. Results are left out unless you choose to include the previews.
- **Write a DataFrame into a table from a notebook:** a **write cell** (added under any cell with **Write to a table…**) writes a cell's whole result into a table of SQL Server, PostgreSQL, MySQL, MariaDB, Oracle, SQLite, DuckDB, MongoDB and more: insert, update by key, insert or update, or replace the table's contents, with a column mapping, a preview, and a new table if you want one. Every write asks first, showing where, how and how many rows (production connections ask you to type the table's name), runs in one transaction where the engine has them, and read-only connections are never written. In a scheduled task, a write cell runs only when the step authorizes that cell, table and mode.
- **Notebook parameters:** a notebook can define parameters (text, number, date, date and time, boolean or a fixed list, with a default) that its cells use as `{{name}}`; running asks for the missing values, each value goes to the database as data (bound by the driver, or quoted by the engine's own rules), and every result shows the values that produced it.
- **Five more languages:** DBine and its phone app now speak German, Russian, Simplified Chinese, Japanese and Korean, besides Spanish, English, Portuguese, French and Italian; the language follows your system's, and only the one in use is loaded.

### Improvements
- **Signed on macOS:** the macOS app is signed with an Apple Developer ID and notarized by Apple, so it opens without the "can't verify the developer" warning or any terminal command. The first time the signed version opens, macOS asks once whether DBine may use its key in the keychain: choose **Always Allow**.
- **Cloud sign-in only trusts the instance's certificate:** connections that sign in with AWS IAM, Google Cloud IAM or Microsoft Entra, and Cloud SQL and AlloyDB connections with the instance's CA file, now accept only that CA (or Amazon's RDS bundle), not any public certificate, before sending a token or password. A server that asks for the password in cleartext while **Trust server certificate** is on now fails instead of receiving it.
- **Signed on Windows:** the Windows installer and the app are signed with a code-signing certificate, so Windows shows who publishes DBine instead of "Unknown publisher".

### Fixes
- **Server monitor:** its figures, tables and notes are translated in every engine, instead of showing some labels in Spanish.
- **AI assistant and optimizer:** the instructions to the model are in the interface's language, so a local model answers in that language instead of in Spanish. The line that tells what was sent to the model is translated too.

## [0.1.14] - 2026-10-17

### New
- **DBine on your phone (Android):** in **Settings › Devices**, turn on **Allow phones** and pair the DBine phone app by scanning a QR code, then confirm on the computer that both screens show the same code. From the phone you see each connection's Monitor, who is blocking whom, the sessions running now and a database's health check; in **Activity**, the queries you run in DBine's editor and the scheduled tasks, with their detail. If you allow that phone, you end a session, cancel a query, or run or cancel a task, each time with your fingerprint, face or PIN. The phone notifies you when a long query finishes and when a task fails or alerts, also with the app closed (every 15 minutes). Your connections and passwords never leave the computer: the phone asks DBine over an end-to-end encrypted channel.
- **Phone access from outside your network:** off by default (local network only). Turn it on in **Settings › Devices › Outside the local network**, through your own relay server or iroh's public servers, which see IP addresses but never the content. How it's protected: [dbine.com/phone-security](https://dbine.com/en/phone-security).
- **Long queries keep the computer awake:** while a query from the editor has been running for 30 seconds or more, DBine keeps the computer from going to sleep, so the connection isn't dropped.
- **Phone app download:** the phone app (Android) is published with every release and on dbine.com.

## [0.1.13] - 2026-10-16

### Improvements
- **Support the project:** the heart button (**Support the project**), its reminder and **Help › Support the project** open Buy Me a Coffee.
- **Script data for chosen tables:** in **Generate database script**, when **Data (INSERT)** is ticked you can keep data for every table or pick which ones get it, from a list with a filter, so the script can carry the structure of the whole database and the rows of just a few tables.
- **Database menu:** **Export database…** is gone. It opened the same dialog as **Generate script…**, which stays and does everything it did (structure, data and saving to a file).
- **Usage statistics:** when the statistics server doesn't answer or fails, DBine retries for up to an hour instead of dropping the event. Nothing else about what's sent changes, and turning statistics off stops any pending retry.
- **Flatpak download:** each release now includes a `.flatpak` file for Linux (`x86_64` and `aarch64`), to install with `flatpak install --user ./dbine-X.Y.Z-x86_64.flatpak`; Flatpak fetches the GNOME runtime from Flathub by itself. It doesn't update itself: install the next version's file the same way.

## [0.1.12] - 2026-10-15

### New
- **Flathub:** DBine is submitted to Flathub as `io.addlayer.dbine`. That build includes every driver, DuckDB and the AI assistant's engine, so nothing is downloaded on first use, and Flatpak keeps it updated instead of DBine. Inside the sandbox, the Azure firewall action explains that it can't use the Azure CLI and offers the portal, T-SQL or a terminal, and scheduled tasks are saved but can't be registered with the system scheduler.
- **winget:** DBine is submitted to winget on Windows as `AddLayer.DBine` (`winget install AddLayer.DBine` once it's accepted), and each new version reaches it by itself. Installed with winget, DBine still updates itself as before.

### Improvements
- **New app identifier:** DBine is now `io.addlayer.dbine`. On the first start after updating, your connections, saved passwords, open tabs, assistant chats, settings and scheduled tasks are copied over by themselves; the previous version's data is left as it was, so going back to it loses nothing. On Windows and Linux a large AI model may be downloaded again. On macOS the keychain may ask once for access, and some privacy permissions (such as local network access) may be asked again; on Windows, a taskbar pin made with an older version may need pinning again.

## [0.1.11] - 2026-10-13

### New
- **Sort by column:** click a column header in a table's data to sort by it (again for descending, a third time to clear; Shift+click adds more columns). The database sorts the whole table where the engine allows it, the same way filters work; elsewhere, or when the server refuses the order, the loaded rows are sorted and a notice says why. Query results can be sorted the same way.
- **Status bar controls:** on a query tab, the blue status bar now holds the connection, the database, the transaction mode (Auto by default, or Manual) and the row limit. Click any of them to change it with a quick picker, like in VS Code. The query toolbar no longer loses buttons when the sidebar is wide: what doesn't fit goes under a **…** menu.
- **Send feedback:** a button at the bottom of the activity bar opens a short form to report a bug, suggest an improvement, request a new engine or ask a question. GitHub opens with the issue already filled in and labeled, so you only press Create. Optional diagnostics (DBine version, OS, language, and the active connection's engine and server version) are shown before they're included, and never carry servers, users, database names or queries.

### Improvements
- **Long text in the grid:** resting on a cell whose text doesn't fit shows it whole in a tooltip that keeps line breaks and can be scrolled and selected.
- **When a connection fails:** expanding a connection connects first and shows **Connecting…**; the tree appears only once the server answers (then instantly, from what DBine remembers, while it refreshes). If it can't connect, the connection shows a banner with **Retry** and **What's wrong…**, and tabs (a table's data, a query run, and the other tools) show the same error instead of staying blank. If the connection drops later, it goes back to the banner. **What's wrong…** explains the failure (firewall, timeout, refused, host name, certificate or login) and what to check, and on Azure SQL it can add your IP to the server's firewall with your Azure CLI login, after confirming the server, rule and IP. Works on every engine; the cause is most precise on SQL Server, Azure SQL, PostgreSQL and MySQL.

### Fixes
- **Query editor:** a tab whose connection isn't open yet (for example, restored at startup) completes schema, table and column names again: asking for completion opens the connection and the list appears when the names arrive. Snippets no longer join the list after a dot.
- **Query editor:** the completion list and hints open upwards when you type on the last lines, instead of hiding under the results.
- **Azure SQL firewall:** **Add my IP** only uses this machine's public IP, looked up over HTTPS and checked again before the rule is created. An IP named in a server's error message is shown as unverified and never used.
- **Retry after a failed connection:** if the tab moved to another connection or database in the meantime, retrying asks for the production confirmation again.
- **Rename on Snowflake and Trino, Presto and Starburst:** what names the renamed object (views, materialized views, functions, procedures and, on Snowflake, tasks, streams and dynamic tables) is listed for you to fix by hand instead of being re-created, so it keeps its owner and security settings. The rename is also refused when stored code would split into several statements, and rewriting dependents no longer changes who the code runs as (`DEFINER`, `SQL SECURITY`, `AUTHID`, `EXECUTE AS`).
- **Queries from AI clients and the assistant:** a query runs without asking only when the database enforces it as read-only and it calls nothing but the engine's built-in, side-effect-free functions. One that calls any other function (a user's function, or one that acts outside the transaction such as pausing replication or taking a lock) waits for your approval first.

## [0.1.10] - 2026-10-10

### New
- **Drivers update on their own, apart from the app:** DBine looks in a signed index for the newest driver compatible with your version, downloads it in the background and goes back to the previous one if something fails. In Settings › Drivers there is a **Check for updates** button, the status of each driver and **Roll back**. A driver can be released on its own, without a new app version.
- **Rename a database:** **Rename…** on a database in the explorer. The dialog shows the other sessions open on it (which the rename ends), whether the new name exists, how many objects move and the full script. Afterwards the connection's default database, open tabs, saved queries, migrations, project targets and scheduled task steps follow the new name; tasks that change data in it ask for approval again. Available in SQL Server, Azure SQL, Babelfish, the PostgreSQL family, MySQL, MariaDB, Snowflake and MongoDB; where the engine can't rename (or move) a database, it isn't offered.
- **Which column is this value?** In an `INSERT … VALUES`, placing the cursor on a value shows a tooltip with its column (for example "Column 14 of 48: Name") and highlights that column in the list. Without a column list, it uses the table's columns in order. Works on every SQL and CQL engine.
- **More ways to sign in to SQL Server with Microsoft Entra ID:** interactive with MFA (in the browser), integrated (the Windows account, through the organization's federated ADFS), managed identity and default (environment variables, managed identity, Azure CLI or Azure Developer CLI). The token is kept while DBine runs and renewed before it expires.
- **Map a login to a database:** **Assign login…** in Users and permissions creates the database user for a server login that already exists (`CREATE USER … FOR LOGIN`, with its default schema), listing the logins that don't have a user there yet. Available in SQL Server, Azure SQL Database (the login is typed there), Babelfish and SAP ASE.

### Improvements
- **What each version brings:** the new-version notice shows its changes and those of the versions in between, starting from the one you have installed, in the app's language.
- **Old app versions:** from now on, an app older than the last five versions has to update in order to download new drivers. The drivers it already has installed keep working.
- **Users and permissions:** changes are queued and applied together. A **pending changes** bar lists them, one review shows the whole script, and they run one at a time: applied ones leave the list and, if one fails, it and the rest stay pending.
- **Reads by MCP clients and the AI assistant are protected by the database itself:**
  - **Enforced by the server:** on the PostgreSQL family, MySQL, MariaDB, SQLite and libSQL, each read runs as a single statement inside a read-only transaction that the server enforces and DBine always rolls back.
  - **Approval:** on the other engines, every read asks for your approval in DBine, the same way writes do. The approval dialog says whether it's a read or a write, and **Approve all** applies to reads and writes separately.
  - **Write check:** DBine still checks each query for writes, as an extra layer.

### Fixes
- **Read-only is stricter:** the queries of MCP clients at the Read level, the AI assistant and read-only connections are checked word by word, not only by their first word. They now refuse:
  - a write hidden after a read in a SQL Server batch;
  - a data-modifying `WITH`;
  - `SELECT … INTO`, `EXEC` and `SET`;
  - functions that act outside the query, such as `set_config`, `dblink_exec` and `xp_cmdshell`.

  An estimated plan refuses scripts that would turn plan mode off, so AI alternatives in **Optimize query** can't run anything. Scheduled tasks whose scripts now count as writing ask to be approved again.
- **Library with git:** a shared repository can no longer make DBine read, write or delete files outside the Library's folder.
- **SSH tunnels:** each server's host key is checked on its own. A key accepted for a jump host no longer counts for the next server, `known_hosts` is checked first and a changed key is always refused. Servers you accepted before are asked once more. The tunnel's local port only serves your own user's programs.
- **Cloud backup:** a backup file someone else modified can no longer weaken the encryption of your next upload.
- **libSQL / Turso:** an `authToken` in a pasted URL is kept in the system keychain, not in the connection's address, name or history. Connections saved before are cleaned up when DBine starts.
- **Document the database:** column names can't inject HTML into the Markdown data dictionary.
- **Task notifications on Windows:** a database error message can no longer run commands through the notification. Notifications on macOS and Linux get the text as separate arguments too.
- **SSH tunnels:** a server whose key you accepted in DBine and that now presents a different one is refused, instead of asking you again. The connection's SSH section lists the accepted servers, each with **Forget**.
- **SQL against PostgreSQL:** string values are written as `E'…'` with backslashes escaped, so a value can't end the string early on a server with `standard_conforming_strings` off. This covers the PostgreSQL family, CockroachDB and similar engines.
- **Snowflake scripts:** a backslash inside a `"quoted name"` no longer changes where a statement ends.
- **CSV and TSV export:** text cells and column names that start with `=`, `+`, `-`, `@`, a tab or a carriage return get a `'` in front, so spreadsheets don't run them as formulas. Numbers are never changed. An option in the export dialog turns it off.
- **Discarding changes in Projects:** a file named like a pattern (`*`) only discards that file.
- **Copy a subset:** masking uses a new random 256-bit key on every run.
- **Driver updates:** the app never accepts a driver index older than the one it was released with, even on a new installation, nor one that has stopped being renewed. Installed drivers keep working either way.
- Connection import, the linter and the health check no longer stop on accented or other multi-byte characters.
- **Read-only on SQL Server:** each query runs in a transaction that is always rolled back. Backups, restores, enabling or disabling triggers, text-pointer writes, Service Broker and transaction statements are refused when they come after a read in the same batch.
- **Read-only on PostgreSQL:** names written with Unicode escapes (`U&"…"`) are refused, so a forbidden function can't be called under another spelling.
- **CSV and TSV export:** the formula protection also applies to text stored in columns declared as numbers, which SQLite allows.
- **Generated scripts:** object names from the server can't end a comment and run as code. This covers the fixes suggested by **Health check**, and the users, backup and structure scripts. ClickHouse names with a backslash are quoted correctly.
- **Read-only:** a statement that doesn't start with a word, such as a bracketed name that runs a procedure on SQL Server, is refused. The words inside parenthesized reads are checked too.
- **Modify table:** the warnings in the script you open as a query stay on their comment line.
- **Passwords and secret options** are masked wherever they're edited, including backup steps in scheduled tasks.
- **View dependencies and Rename** no longer stop on routines with unusual quoted names.
- **Read-only on SQL Server:** a batch can only start with `SELECT`, `WITH`, `USE` or `PRINT`. What follows `SHOW`, `DESCRIBE` or `PRAGMA` is checked on every engine.
- **Import connections:** an Oracle JDBC URL with a user and password keeps them out of the connection's name: the password goes to the system keychain.
- **Read-only on SQL Server:** a procedure whose name starts like `print_` or `select1` is no longer taken for a read.
- **Import connections:** an Oracle password containing `@` is kept whole in the system keychain.
- **SSH tunnels on Linux:** the tunnel's local port only trusts open connections from your own user.
- **Read-only:** a query with a lone carriage return or an unusual Unicode space is refused, because engines disagree on where a comment or a statement ends there. Functions with side effects are refused too: `load_extension` in SQLite, `pg_notify` and logical replication reads in PostgreSQL, and session or query cancellation in Snowflake.
- **Import connections:** a SQL Server URL with no host no longer puts its password in the connection's name.
- **Users and permissions:** the preview hides PostgreSQL passwords in every form.
- **Read-only:** queries that take locks other sessions wait on are refused, for example `pg_advisory_lock`, `GET_LOCK`, `LOCK IN SHARE MODE` and `FOR SHARE`.
- **Snowflake and Cassandra:** `//` comments are read the way the server reads them.
- **Read-only SQL Server connections** refuse lock hints and `WAITFOR`.
- **Read-only connections** refuse advancing a sequence (`NEXT VALUE FOR`, `.NEXTVAL`).
- **Rename:**
  - On every engine, a dependent whose code would split into more than one statement is refused.
  - Snowflake routine bodies containing `$$` are kept whole.
  - OrientDB names that DBine couldn't quote safely are refused.
- **SQL export:** string values are written the way the source engine reads them, so they come back exact when you run the script there. They also can't add statements on an engine that reads backslashes differently.
  - **PostgreSQL, SQL Server, Oracle, SQLite, DuckDB, Firebird, SAP HANA, Trino, Db2, Teradata, Vertica, Exasol, Netezza and Dremio:** a backslash is written with the engine's own character function, so no backslash appears inside a string. Firebird scripts carry one row per `INSERT`, which is all Firebird accepts.
  - **Engines that read backslash escapes** (MySQL, ClickHouse, Snowflake, Redshift…): backslashes and quotes are escaped the way those engines read them.
  - **Unknown sources** (a grid from several databases) get a form that's safe on any engine. The new option **Target reads strings the standard way** writes their backslashes as they are.
- **CSV and TSV export:** formula protection also covers text after a `,` or `;`, which a spreadsheet with another list separator would split into its own cell.
- **Rename on Snowflake:** views, functions and procedures that name the renamed object are listed for you to fix by hand instead of being re-created, so they keep their owner, `EXECUTE AS` and `SECURE`.

## [0.1.9] - 2026-10-09

### New
- **Rename with impact:** **Rename…** in the explorer changes the name of a table, view, routine, column, index or schema and, in the same script, rewrites the views, procedures, functions and triggers that use it. Before running, it shows what the engine updates by itself, what gets rewritten and what needs a manual check (dynamic SQL, unreadable code), together with the full script. It runs in a transaction where the engine allows it. It is available in every engine that can rename something; each one's limits are in `docs/engine-support.md`.
- **Modify a table:** **Modify…** opens the designer on an existing table and builds the engine's `ALTER`. It keeps what the designer doesn't show (CHECKs, index options, the order of the key columns) and recreates the views and triggers that depend on the table. Renaming a column there goes through the impact review; on production connections it asks you to type the table name before running.
- **Per-query history:** the **History** bar follows the active tab, like a timeline: versions of the saved query with differences and restore, its runs and, in project files, its git commits.
- **Navigation in the editor:** Cmd/Ctrl+click on a table, view or routine opens its structure or definition, and **Show in explorer** locates it in the tree. Tables and columns that don't exist are flagged before running.
- **Query parameters:** `:name` and `?` are asked for when running, and the last value is remembered.
- **Snippets** per engine (for example, `sel` + Tab) and a right-click menu in the editor.
- **Selection totals:** selecting cells in the grid shows count, sum, average, minimum and maximum.
- **Scheduled tasks:** scripts, exports, schema comparison, backups, **Document the database** and **Send an email** (SMTP) that run with DBine closed, through the system scheduler. With notifications per task and a run history. Anything that changes data is approved explicitly.
- **Code quality:** per-engine rules in the editor and **View problems**.
- **Document the database:** a data dictionary in HTML or Markdown, with a diagram, estimated rows and comments on views and routines. Estimated rows and comments come from the engine's metadata, without reading tables or using quota on cloud engines.
- **Design query:** a visual query builder.
- **Copy a subset** of data, with masking.
- **Optimize query:** rewrites, suggested indexes, AI alternatives and a measured comparison. AI alternatives are validated against the database's estimated plan before they are shown.
- **Health check** of a database, in every engine, with its own checks in SQL Server, the PostgreSQL family, the MySQL family, Oracle, SAP HANA, Firebird, ClickHouse, Snowflake, BigQuery, Databricks and ODBC profiles.
- **Search in database:** object names, view and routine code, and column names (with their table and type).
- **Generate test data** for a table.
- **Database properties** and advanced options when creating a database, in tabs and per engine, with a script preview.
- **JSON tree view** of results, with editing, and **Add row** / **Add document** in the Data tab and in the grid.
- **New brand:** the logotype with the halo.

### Improvements
- The connection color shows as a stripe on the edge of the row, and the dot only indicates the state (green connected, red disconnected).

### Fixes
- Schema comparison is no longer cancelled by explorer reads, and SQL Server reconnects.
- Connection rows without a color line up with those that have one.
- Dragging tables into the query builder works on macOS.
- SQL Server properties: long file names no longer overflow the dialog, and the "ANSI and security options" tab is translated.
- The editor no longer flags the columns of an aliased subquery as unknown.

### Already available
- **Run a query on several databases at once:** you pick one or more databases of a connection, and the results are joined with a column that shows each row's database. It arrived in 0.1.4. See `docs/multi-database-queries.md`.

## [0.1.8] - 2026-10-06

### New
- **PostgreSQL behind gateways that only accept the simple protocol:** the connection has a new option, **Query protocol**: Automatic or Simple protocol only. It is meant for gateways that reject the extended protocol with error 0A000. In that mode, anything that needs the extended protocol shows a clear message instead of failing.

### Fixes
- **New query with the New connection tab open** failed with "FOREIGN KEY constraint failed". Now the query opens on the last connection you had open, or asks you to pick a database in the explorer.

## [0.1.7] - 2026-10-05

### Fixes
- **Compare data with identity columns:** syncing rows into a SQL Server table with an `IDENTITY` column failed with "Cannot insert explicit value for identity column". Now DBine turns on `IDENTITY_INSERT` only while it inserts those rows.
- In PostgreSQL, after copying rows with their ids, the sequence advances so the next insert doesn't collide with a copied id.

## [0.1.6] - 2026-10-04

### New
- **Processes in the Monitor:** next to the dashboard, the **Processes** tab lists the server's sessions and running queries, with filters. From there you can cancel a query or end a session. Available in every engine that exposes it: SQL Server, PostgreSQL, MySQL, Oracle, MongoDB, Redis and most of the others.
- **Windows authentication in SQL Server:** with the current user (SSPI on Windows, Kerberos on macOS and Linux) or with a domain user and password, also from Mac and Linux.
- **Kerberos in MongoDB.**

### Improvements
- In ODBC, the connection's extra attributes replace those of the template.
- The AI assistant has its own icon and is no longer confused with **Format**.
- The queries the assistant makes are shown translated in every language.
- Anonymous telemetry also counts the use of the assistant, the MCP server, syncs, migrations and multi-database queries. Never names, queries or data; it is turned off in Settings › General.

## [0.1.5] - 2026-10-03

### New
- **The AI assistant reads your database, with your approval:** with a local model it can query the structure and index usage of the connection (for example, "analyze the indexes and tell me which one is redundant"). Before reading rows or running a query it shows you the exact SQL and the database, with Approve or Reject. It never changes data or structure.

### Improvements
- **Stop** cuts the assistant's answer at any time and **New conversation** is always available.
- The chat's "structure" option is no longer needed: the assistant asks for the details when it needs them.
- The text cursor appears where you can select or type.

## [0.1.4] - 2026-10-03

### New
- **Projects:** Git repositories of SQL linked to your connections, from the second icon in the sidebar. File tree, active database or environments (dev/qa/prod) without credentials in the repo, changes with diff, commit, pull and push. Each database shows its linked projects in the Explorer.
- **Run a query on several databases at once:** the same query on several databases of a connection, with the results together and a column that shows the database.
- **DBine updates itself:** it downloads the new version, verifies its signature and restarts (it asks first if there are background tasks). 0.1.4 is installed by hand for the last time. On Linux it works with the AppImage; with .deb/.rpm it still offers the download.
- **Disable and enable indexes** from the explorer and the Indexes tab, in the engines that allow it (SQL Server, MySQL, MariaDB, TiDB, Oracle, Firebird, CockroachDB, MongoDB…).
- **Cell selection in the grid:** a block (by dragging, Shift+click or Shift+arrows) to copy it, or skipped cells and rows with Cmd/Ctrl+click.
- **Schema comparison:** it can drop an element on the left, on the right or on both sides, and before running it shows what depends on it.

### Improvements
- **AI assistant:** recommends a larger built-in model according to your computer's memory, knows the particularities of each dialect, retries if it refuses to answer and keeps the conversation history in a panel.
- The Tasks panel has **Remove finished** at the top and closes with Escape or a click outside.
- Azure SQL Database (also Hyperscale): connected to master, it lists all the server's databases.
- CockroachDB: indexes show as BTREE and GIN, as in PostgreSQL.
- The AI chat text can be selected and copied.

### Fixes
- The query bar no longer falls apart when the AI panel opens.
- libSQL schema sync no longer fails because of a `PRAGMA` statement the server rejects.

## [0.1.3] - 2026-10-02

### New
- **Several windows** in the same instance: **New window** from the Dock, the taskbar, File › New window or Cmd/Ctrl+Shift+N. Connections, saved queries and settings are shared between windows.
- **Background tasks:** long operations (syncing schemas or data, backups, generating scripts, importing, exporting, cloning tables, dropping objects) can keep running in the background. The Tasks panel shows progress, elapsed time, estimated remaining time and Cancel, and notifies you when done. When closing the application with tasks running, it asks for confirmation before cancelling them.
- **Index usage** in every engine that reports it: PK and FK keys on the columns, an Indexes folder, percentage of reads per index with a color according to seeks and scans, and dropping an index from the explorer.
- **View dependencies…:** what depends on a table, column, view or routine.

### Improvements
- Data sync applies each side in a single transaction.
- SQL autocompletion after "schema." and "table.".
- Schema comparison syncs comments, has reversible arrows and a resizable list.

### Fixes
- Schema sync drops duplicate foreign keys one by one, and in SQL Server it safely changes a table's clustered index.

## [0.1.2] - 2026-10-01

### New
- **Script execution like each engine's own tool:** statement by statement, with `GO` / `GO N`, `DELIMITER`, `/` and `SET TERM`. A **Continue on error** option, live ordered messages, errors with code and line, and running the statement at the cursor.
- **Auto/Manual transactions** with Commit and Roll back, and confirmation before an UPDATE or DELETE without WHERE.
- **Schemas:** create and drop schemas with owner and permissions; empty schemas show up in the explorer.
- **New version notice:** DBine tells you when there is a new version, on startup and from Help › Check for updates….
- Reorder connections and folders by dragging.
- Delete rows from the data grid and save the changes with Cmd/Ctrl+S.

### Improvements
- Cancelling a query keeps the session.

### Fixes
- The Solr driver was republished (it shares code with the Elasticsearch one).

## [0.1.1] - 2026-09-30

### New
- **Anonymous telemetry**, on by default, with a notice the first time. It is turned off in Settings or with `DO_NOT_TRACK` / `DBINE_TELEMETRY=0`.
- PostgreSQL: identity options (identity columns) when designing tables.

### Improvements
- Oracle: definitions include indexes.
- Full definition tab, with clearer migration error messages.
- Schema comparison keeps the rows and syncs in a single step.
- The data comparison tab remembers your selections.
- PostgreSQL and Oracle drivers updated to 0.1.2.

## [0.1.0] - 2026-09-30

### New
- First version of DBine, with installers for Windows, macOS (Apple Silicon and Intel) and Linux. Each engine's drivers, except SQLite, are downloaded the first time you connect.
