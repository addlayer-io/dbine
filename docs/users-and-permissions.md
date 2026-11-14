# Users and permissions

Right-click a database › **Users and permissions…** opens a tab with the
server's users and roles (or the database's, on engines where users are
per database, such as SQL Server and MongoDB).

## What it shows

- On the left, the **users** and **roles**, with search. Superusers, disabled
  ones and built-in ones (`sa`, `dbo`, `postgres`, `PUBLIC`…) are marked.
- On the right, the selected one:
  - its data (type, authentication, login, default schema, creation…,
    depending on the engine);
  - the **roles** it is a member of;
  - its **permissions**: the permission, on what (the whole database, a
    schema, a table…) and whether it is **direct** or held **through** a
    role. Those it can grant to others and denied ones (`DENY` in SQL Server)
    are marked.

## What you can do

- Create a user (with a password) or a role.
- Change the password, enable or disable login, delete.
- Add to a role or remove from a role.
- Grant permissions on the database, a schema or an object, with the option
  to let them grant them to others; revoke a direct permission.

Each change becomes a **script in the engine's language** (`CREATE LOGIN…`,
`GRANT…`, `db.createUser(…)`, `ACL SETUSER…`) that is shown before running.
Nothing runs without clicking **Run**.

- The **password** is hidden in the preview and when copying, and these
  scripts are **not saved in the history**.
- **Read-only** connections show users and permissions, but offer no changes.
- The permissions available to grant are the ones **this server** offers,
  read from its own catalog where the engine has one (SQL Server's
  `sys.fn_builtin_permissions`, MySQL's `SHOW PRIVILEGES`, ClickHouse's
  `system.privileges`…), so they follow its version: SQL Server 2022 offers
  `VIEW DATABASE PERFORMANCE STATE`, an older one doesn't. Engines without
  such a catalog offer the ones their documentation lists. The list shows
  only what applies to the target you pick (the whole database, a schema, a
  table, a procedure…), and you can also type another.

## Map a login

On engines whose database users sign in through a separate server login
(SQL Server, Azure SQL Database, Babelfish and Sybase ASE), **Map login…**,
next to **New user**, creates a user of the current database for a login
that already exists on the server, without creating a new login:

- **Login:** a list of the server's logins that have no user in this
  database yet; you can also type one. Where the logins can't be listed
  from the database (Azure SQL Database, where they live in `master`), you
  type it.
- **User name:** filled in with the login's name; it can be changed.
- **Default schema** (optional; SQL Server family only): `dbo` is proposed.

The script is shown and run like any other change:

- SQL Server, Azure SQL Database, Babelfish:
  `CREATE USER [user] FOR LOGIN [login] WITH DEFAULT_SCHEMA = [schema];`
- Sybase ASE: `exec sp_adduser 'login', 'user'` (ASE has no default schema).

After it runs, the list is refreshed and the new user is selected.

## Particularities

- **SQL Server:** users belong to the database and sign in with a server
  login. Creating a user creates the login and the user. Dropping a user
  doesn't drop its login (the script leaves it commented out, in case the
  login is used in other databases). In Azure SQL, contained users are used,
  with the password on the user, and disabling means removing `CONNECT`.
- **Roles and groups with a prefix:** some engines need to know, when writing
  the script, whether the recipient is a user or a role (`TO ROLE`,
  `TO GROUP`). That's why their roles are listed as `role:name` and groups as
  `group:name`:
  - roles: IoTDB, Dremio, Hive, Impala, Netezza, Db2 for z/OS and Ingres;
  - groups: Couchbase and Ocient.
- **Snowflake:** permissions are granted to roles. When adding or removing a
  member, the script finds out at run time whether it is a user or a role.
- **SAP HANA:** repository roles (`package::role`) are granted with
  `_SYS_REPO.GRANT_ACTIVATED_ROLE`.
- **Trino:** a catalog's roles are shown as `role IN catalog`.
- **Databricks:** grants and revokes Unity Catalog permissions. Users and
  groups are managed in the console.
- **BigQuery:** permissions are IAM roles on the dataset, its tables and
  views. Those of the connection's dataset are shown.
- **Azure Cosmos DB:** per-container users and permissions are used (`ALL` or
  `READ`), with DBine's own statements: `CREATE USER`, `GRANT ALL ON "c"
  TO "u"`, `REVOKE`, `DROP USER`.
- **Solr:** reads users, roles and permissions from `security.json`. The
  scripts are requests to `/admin/authentication`. Granting permissions or
  roles is done from the console, because Solr replaces the whole list.
- **Couchbase:** uses SQL++ user commands, which exist since version 8.0.
- **Power BI:** the tab is the model's. It lists the workspace's users and
  groups with their role (Admin, Member, Contributor, Viewer), the model's
  users with their rights, and the model's row-level security roles with
  their members. The model's rights are **Read**, **Build** and **Reshare**
  (and **Write**, shown but never changed from DBine). Permissions are on the
  whole model, not on tables, and there are no passwords, users to create or
  roles to create: identities belong to Microsoft Entra ID and RLS roles to
  the model. See [Power BI](#power-bi-model-permissions).

## Power BI model permissions

On a Power BI connection that isn't read-only, the tab grants and revokes a
user's or group's rights on the model. Each change is a **script** that is
shown before running, in the driver's own commands, one per line, with the
names as DAX strings:

```text
GRANT Read, Build TO "ana@contoso.com"          -- AS GROUP when the name is an object id
REVOKE Reshare FROM "ana@contoso.com"
REVOKE ALL FROM "ana@contoso.com"
```

- Power BI keeps **one combined right** per user. DBine reads the current
  one first (`GET …/datasets/{d}/users`, which needs Read, Write and Reshare
  on the model), works out the new combined right, and sends it: a `POST` for
  someone not listed yet, a `PUT` for someone who is. Taking away **Read**,
  or leaving nothing, removes the user's access to the model. If the current
  right can't be read, nothing is sent.
- **Refused before any request**, with the reason: giving or taking
  **Write** (the API can't; it comes from a workspace role), service
  principals (`AS APP`), a right the user holds through a workspace role (to
  remove it, change the role in the workspace), and anything on a read-only
  connection.
- Granting offers **Read**, **Build** and **Reshare**; **Read** is always
  included.
- The commands run through the editor too (any unit that starts with
  `GRANT`, `REVOKE`, `RENAME` or `ALTER` is one of them). The scheduled-task,
  notebook, "Run on several databases" and MCP guards count such a unit as a
  write (a DAX query, by contrast, isn't one).
- The script and the confirmation are the tab's own: nothing runs without
  clicking **Run**. The same rule covers renaming, describing and deleting
  the model: [Rename](rename.md#renaming-a-database),
  [Database properties](database-properties.md) and **Delete database**.

## Contract

- `Driver::security()` says what the engine offers (`SecuritySpec`):
  permissions, object types, whether it creates users and roles, whether it
  handles passwords and memberships, and whether it is per database.
- `Session::principals()` and `Session::grants(name)` read.
- `Session::privileges()` lists what the server can grant, each `Privilege`
  with the `object_kinds` it applies to ("" = the whole database or
  server): from the server's catalog where it has one, else the engine's
  documented list by kind. It never fails just because the catalog can't be
  read (it falls back to the documented list). Where it answers
  `Unsupported` (the default, or a driver host built before it), the tab
  offers `SecuritySpec::privileges` on every kind.
- **Map login…** is offered where `Driver::supports_map_login()` is true;
  `Driver::map_login_script(login, user, default_schema)` writes the
  statement, and `Session::unmapped_logins()` lists the server's logins with
  no user in the current database. Where it answers `Unsupported`, the
  dialog lets the user type the login. All three default to off /
  `Unsupported`, so only the drivers that have it implement them.
- `Driver::security_script(action)` writes the change.

What each engine supports is in
[`engine-support.md`](engine-support.md#users-and-permissions).
