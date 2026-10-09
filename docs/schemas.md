# New schema and drop schema

On engines whose explorer shows schemas, DBine creates and drops schemas
without writing the SQL by hand. Each action builds a script in the engine's
language, which you can see before running it.

## New schema

Right-click a database › **New schema…**.

- **Name:** the schema's name. It is quoted when needed, so it can have
  spaces or uppercase letters where the engine allows it.
- **Owner** (optional): a server user or role. If left empty, the owner is the
  connection's user.
- **Permissions** (optional): who is granted what on the new schema, with the
  option to let them grant it to others. The privileges offered are those the
  engine accepts on a schema (`USAGE` and `CREATE` in PostgreSQL, `SELECT`,
  `EXECUTE`, `ALTER`… in SQL Server).

The script (create the schema, assign the owner and grant the permissions) is
updated as the form is filled in. From there it can be copied, opened in a
query to edit it, or run, with a confirmation first.

## Drop schema

Right-click a schema › **Drop schema…**.

- It shows how many objects the schema has, read from the server at that
  moment.
- **With its contents** also drops all its objects (`CASCADE`). Without that
  option, the server rejects the drop while the schema has objects.
- It asks for confirmation, because it can't be undone.

## Rules

- **Read-only** connections don't offer these actions.
- If the server says the user can't create schemas, "New schema…" appears
  disabled with the missing permission. If the engine doesn't allow knowing
  that, it stays enabled and the server answers.
- These scripts aren't saved in the query history.
- After creating or dropping, the database tree is refreshed.
- An empty schema shows in the tree: engines with this action list their
  schemas. System schemas (`sys`, `pg_catalog`, `INFORMATION_SCHEMA`…) are
  hidden while they have no objects.

## Particularities

- **SQL Server and derivatives:** `DROP SCHEMA` has no `CASCADE`: a schema
  with objects isn't dropped until it is emptied. The owner (user or role)
  goes in `AUTHORIZATION`: a later `ALTER AUTHORIZATION` would erase the
  permissions just granted, and Babelfish doesn't have it. Whoever creates
  without being `db_owner` needs `db_securityadmin` to grant on a schema that
  isn't theirs; in SQL Server, in addition, `CREATE SCHEMA` and `IMPERSONATE`
  on the owner user (or `ALTER` on the owner role), and in Babelfish,
  `db_ddladmin`. Fabric creates the schema without an explicit owner.
- **PostgreSQL and compatibles:** the schema is created without an owner,
  then the permissions are granted and finally it is handed over with
  `ALTER SCHEMA … OWNER TO`, so someone who isn't a superuser can also grant.
  CockroachDB and H2 put the owner in `AUTHORIZATION`. In Redshift,
  RisingWave and H2 the owner is always a user. Materialize runs the script
  one statement at a time.
- **Snowflake:** dropping a schema always drops its contents, and the owner
  is a role; it is handed over at the end with `COPY CURRENT GRANTS`.
- **Databricks:** "with grant option" grants `MANAGE` on the schema (Unity
  Catalog has no `WITH GRANT OPTION`).
- **Dremio:** a schema is a folder. The name is written inside the space or
  source where the menu was opened (`folder`) or with its full path
  (`source.folder`). With SQL, folders are created only in catalog sources
  (Nessie, Iceberg REST, Arctic); those in a space are dropped.
- **Couchbase:** a schema is a scope; the name comes completed with the
  bucket (`bucket.scope`) and dropping it always drops its collections.
- **Oracle, the MySQL family, SAP HANA, BigQuery, Athena and ClickHouse:**
  they don't have this action because the schema is a user (Oracle) or is
  the explorer's database (the others): it is created with "Users and
  permissions" or with "New database".

What each engine supports is in
[`engine-support.md`](engine-support.md#new-schema-and-drop-schema).

## Contract

- `Driver::schema_spec()` says what the engine offers (`SchemaSpec`): whether
  the owner is chosen and who can be one (`owner_kinds`: users, roles or
  both), whether it drops with its contents and which privileges are granted
  on a schema. `None` hides the actions.
- All methods receive `database`: the database the menu was opened on
  (`None` if there is none), for engines where the schema's path depends on
  it (Dremio's source, Flight SQL's catalog).
- `Driver::create_schema_script(base, name, owner)` and
  `Driver::drop_schema_script(base, name, cascade)` write the scripts.
  Permissions come from `Driver::schema_grant_script(base, name,
  privileges, to, grantable)`, which by default uses `Driver::security_script`
  on an object of type `schema`.
- The "New schema…" script goes in this order: create, grant permissions and
  only then change the owner. `Driver::schema_owner_script(base, name,
  owner)` returns `None` (the default) when the owner goes inside the
  `CREATE` (`AUTHORIZATION`): SQL Server, where a later `ALTER
  AUTHORIZATION` would erase the permissions; CockroachDB, where the owner's
  members keep their rights; H2, which doesn't change owners; and Db2 LUW.
  Where the creator loses the right to grant when handing over the schema
  (PostgreSQL family, Snowflake `GRANT OWNERSHIP`, Databricks `ALTER SCHEMA …
  OWNER TO`, Trino `ALTER SCHEMA … SET AUTHORIZATION`, Aurora DSQL, Exasol,
  Hive/Impala), it returns the owner change and the schema is created
  without an owner.
- `Session::list_schemas()` lists the schemas of the session's database
  (`SchemaInfo`, with `system` for the engine's own), so the tree shows empty
  ones. `None` (the default) lets them come from the object list.
- The permission to create is reported in `Permissions::create_schema`.
