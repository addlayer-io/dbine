# Nuevo esquema y borrar esquema

En los motores cuyo explorador muestra esquemas, DBine crea y borra esquemas
sin escribir el SQL a mano. Cada acción arma un script en el lenguaje del
motor, que se ve antes de ejecutarlo.

## Nuevo esquema

Clic derecho sobre una base › **Nuevo esquema…**.

- **Nombre:** el del esquema. Se escribe entre comillas cuando hace falta, así
  que puede tener espacios o mayúsculas donde el motor lo permite.
- **Dueño** (opcional): un usuario o rol del servidor. Si queda vacío, el
  dueño es el usuario de la conexión.
- **Permisos** (opcionales): a quién se le otorga qué sobre el nuevo esquema,
  con la opción de que pueda otorgarlo a otros. Los privilegios que se ofrecen
  son los que el motor acepta sobre un esquema (`USAGE` y `CREATE` en
  PostgreSQL, `SELECT`, `EXECUTE`, `ALTER`… en SQL Server).

El script (crear el esquema, asignar el dueño y otorgar los permisos) se
actualiza mientras se completa el formulario. Desde ahí se puede copiar,
abrir en una query para editarlo o ejecutar, con una confirmación antes.

## Borrar esquema

Clic derecho sobre un esquema › **Borrar esquema…**.

- Muestra cuántos objetos tiene el esquema, leídos del servidor en ese
  momento.
- **Con su contenido** borra también todos sus objetos (`CASCADE`). Sin esa
  opción, el servidor rechaza el borrado mientras el esquema tenga objetos.
- Pide confirmación, porque no se puede deshacer.

## Reglas

- Las conexiones de **solo lectura** no ofrecen estas acciones.
- Si el servidor dice que el usuario no puede crear esquemas, «Nuevo esquema…»
  aparece deshabilitado con el permiso que falta. Si el motor no permite
  saberlo, queda habilitado y responde el servidor.
- Estos scripts no se guardan en el historial de queries.
- Después de crear o borrar, el árbol de la base se actualiza.
- Un esquema vacío se ve en el árbol: los motores con esta acción listan sus
  esquemas. Los esquemas del sistema (`sys`, `pg_catalog`,
  `INFORMATION_SCHEMA`…) se ocultan mientras no tengan objetos.

## Particularidades

- **SQL Server y derivados:** `DROP SCHEMA` no tiene `CASCADE`: un esquema con
  objetos no se borra hasta vaciarlo. El dueño (usuario o rol) va en
  `AUTHORIZATION`: un `ALTER AUTHORIZATION` posterior borraría los permisos
  recién otorgados, y Babelfish no lo tiene. Quien crea sin ser `db_owner`
  necesita `db_securityadmin` para otorgar sobre un esquema que no es suyo;
  en SQL Server, además, `CREATE SCHEMA` e `IMPERSONATE` sobre el usuario
  dueño (o `ALTER` sobre el rol dueño), y en Babelfish, `db_ddladmin`. Fabric crea el esquema sin
  dueño explícito.
- **PostgreSQL y compatibles:** el esquema se crea sin dueño, después se
  otorgan los permisos y al final se cede con `ALTER SCHEMA … OWNER TO`, así
  también puede otorgar quien no es superusuario. CockroachDB y H2 ponen el
  dueño en `AUTHORIZATION`. En Redshift, RisingWave y H2 el dueño es siempre
  un usuario. Materialize ejecuta el script de a una sentencia.
- **Snowflake:** borrar un esquema siempre borra su contenido, y el dueño es
  un rol; se le cede al final con `COPY CURRENT GRANTS`.
- **Databricks:** «con opción de otorgar» otorga `MANAGE` sobre el esquema
  (Unity Catalog no tiene `WITH GRANT OPTION`).
- **Dremio:** un esquema es una carpeta. El nombre se escribe dentro del
  espacio u origen donde se abrió el menú (`carpeta`) o con su ruta completa
  (`origen.carpeta`). Con SQL solo se crean carpetas en orígenes de catálogo
  (Nessie, Iceberg REST, Arctic); las de un espacio se borran.
- **Couchbase:** un esquema es un scope; el nombre viene completado con el
  bucket (`bucket.scope`) y borrarlo siempre borra sus colecciones.
- **Oracle, la familia MySQL, SAP HANA, BigQuery, Athena y ClickHouse:** no
  tienen esta acción porque el esquema es un usuario (Oracle) o es la base del
  explorador (los demás): se crea con «Usuarios y permisos» o con «Nueva base».

Qué soporta cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#nuevo-esquema-y-borrar-esquema).

## Contrato

- `Driver::schema_spec()` dice qué ofrece el motor (`SchemaSpec`): si se elige
  el dueño y quiénes pueden serlo (`owner_kinds`: usuarios, roles o ambos),
  si borra con su contenido y qué privilegios se otorgan sobre un esquema.
  `None` oculta las acciones.
- Todos los métodos reciben `database`: la base sobre la que se abrió el menú
  (`None` si no hay), para los motores donde la ruta del esquema depende de
  ella (origen de Dremio, catálogo de Flight SQL).
- `Driver::create_schema_script(base, nombre, dueño)` y
  `Driver::drop_schema_script(base, nombre, cascade)` escriben los scripts.
  Los permisos salen de `Driver::schema_grant_script(base, nombre,
  privilegios, a, grantable)`, que por defecto usa `Driver::security_script`
  sobre un objeto de tipo `schema`.
- El script de «Nuevo esquema…» va en este orden: crear, otorgar permisos y
  recién después cambiar el dueño. `Driver::schema_owner_script(base, nombre,
  dueño)` devuelve `None` (por defecto) cuando el dueño va dentro del
  `CREATE` (`AUTHORIZATION`): SQL Server, donde un `ALTER AUTHORIZATION`
  posterior borraría los permisos; CockroachDB, donde los miembros del dueño
  conservan sus derechos; H2, que no cambia dueños; y Db2 LUW. Donde el
  creador pierde el derecho de otorgar al ceder el esquema (familia
  PostgreSQL, Snowflake `GRANT OWNERSHIP`, Databricks `ALTER SCHEMA … OWNER
  TO`, Trino `ALTER SCHEMA … SET AUTHORIZATION`, Aurora DSQL, Exasol,
  Hive/Impala), devuelve el cambio de dueño y el esquema se crea sin dueño.
- `Session::list_schemas()` lista los esquemas de la base de la sesión (`SchemaInfo`, con `system`
  para los del motor), así el árbol muestra los vacíos. `None` (por defecto)
  deja que salgan de la lista de objetos.
- El permiso para crear se informa en `Permissions::create_schema`.
