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
- The permissions available to grant are the engine's. You can also type
  another.

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

## Contract

- `Driver::security()` says what the engine offers (`SecuritySpec`):
  permissions, object types, whether it creates users and roles, whether it
  handles passwords and memberships, and whether it is per database.
- `Session::principals()` and `Session::grants(name)` read.
- `Driver::security_script(action)` writes the change.

What each engine supports is in
[`engine-support.md`](engine-support.md#users-and-permissions).
