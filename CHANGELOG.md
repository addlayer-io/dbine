# Changelog

## [Unreleased]

### New
- **Drivers update on their own, apart from the app:** DBine looks in a signed index for the newest driver compatible with your version, downloads it in the background and goes back to the previous one if something fails. In Settings › Drivers there is a **Check for updates** button, the status of each driver and **Roll back**. A driver can be released on its own, without a new app version.

### Improvements
- **What each version brings:** the new-version notice shows its changes and those of the versions in between, starting from the one you have installed.
- **Old app versions:** from now on, an app older than the last five versions has to update in order to download new drivers. The drivers it already has installed keep working.

## [0.1.9] - 2026-10-09

### New
- **Rename with impact:** **Rename…** in the explorer changes the name of a table, view, routine, column, index or schema and, in the same script, rewrites the views, procedures, functions and triggers that use it. Before running, it shows what the engine updates by itself, what gets rewritten and what needs a manual check (dynamic SQL, unreadable code), together with the full script. It runs in a transaction where the engine allows it. It is available in every engine that can rename something; each one's limits are in `docs/soporte-por-motor.md`.
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
- **Run a query on several databases at once:** you pick one or more databases of a connection, and the results are joined with a column that shows each row's database. It arrived in 0.1.4. See `docs/ejecutar-en-varias-bases.md`.

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
