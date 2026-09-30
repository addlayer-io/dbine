# Usuarios y permisos

Clic derecho sobre una base › **Usuarios y permisos…** abre una pestaña con
los usuarios y roles del servidor (o de esa base, en los motores donde los
usuarios son por base, como SQL Server y MongoDB).

## Qué muestra

- A la izquierda, los **usuarios** y los **roles**, con búsqueda. Se marcan
  los superusuarios, los deshabilitados y los del sistema (`sa`, `dbo`,
  `postgres`, `PUBLIC`…).
- A la derecha, el elegido:
  - sus datos (tipo, autenticación, login, esquema predeterminado, alta…,
    según el motor);
  - los **roles** de los que es miembro;
  - sus **permisos**: el permiso, sobre qué (la base entera, un esquema, una
    tabla…) y si es **directo** o lo tiene **por** un rol. Se marcan los que
    puede otorgar a otros y los denegados (`DENY` en SQL Server).

## Qué se puede hacer

- Crear un usuario (con contraseña) o un rol.
- Cambiar la contraseña, habilitar o deshabilitar el ingreso, borrar.
- Agregar a un rol o sacar de un rol.
- Otorgar permisos sobre la base, un esquema o un objeto, con la opción de
  que pueda otorgarlos a otros; revocar un permiso directo.

Cada cambio se convierte en un **script en el lenguaje del motor**
(`CREATE LOGIN…`, `GRANT…`, `db.createUser(…)`, `ACL SETUSER…`) que se
muestra antes de ejecutarse. Nada corre sin el clic en **Ejecutar**.

- La **contraseña** se oculta en la vista previa y al copiar, y estos scripts
  **no se guardan en el historial**.
- Las conexiones de **solo lectura** muestran usuarios y permisos, pero no
  ofrecen cambios.
- Los permisos disponibles para otorgar son los del motor. También se puede
  escribir otro.

## Particularidades

- **SQL Server:** los usuarios son de la base y entran con un login del
  servidor. Crear un usuario crea el login y el usuario. Borrar un usuario no
  borra su login (el script lo deja comentado, por si el login se usa en
  otras bases). En Azure SQL se usan usuarios contenidos, con la contraseña
  en el usuario, y deshabilitar significa quitarle `CONNECT`.
- **Roles y grupos con prefijo:** algunos motores necesitan saber, al
  escribir el script, si el destinatario es un usuario o un rol (`TO ROLE`,
  `TO GROUP`). Por eso sus roles se listan como `role:nombre` y los grupos
  como `group:nombre`:
  - roles: IoTDB, Dremio, Hive, Impala, Netezza, Db2 for z/OS e Ingres;
  - grupos: Couchbase y Ocient.
- **Snowflake:** los permisos se otorgan a roles. Al agregar o quitar un
  miembro, el script averigua al ejecutarse si es un usuario o un rol.
- **SAP HANA:** los roles del repositorio (`paquete::rol`) se otorgan con
  `_SYS_REPO.GRANT_ACTIVATED_ROLE`.
- **Trino:** los roles de un catálogo se ven como `rol IN catálogo`.
- **Databricks:** otorga y revoca permisos de Unity Catalog. Los usuarios y
  grupos se administran en la consola.
- **BigQuery:** los permisos son roles de IAM sobre el dataset, sus tablas y
  vistas. Se muestran los del dataset de la conexión.
- **Azure Cosmos DB:** se usan usuarios y permisos por contenedor (`ALL` o
  `READ`), con sentencias propias de DBine: `CREATE USER`, `GRANT ALL ON "c"
  TO "u"`, `REVOKE`, `DROP USER`.
- **Solr:** lee usuarios, roles y permisos de `security.json`. Los scripts
  son pedidos a `/admin/authentication`. Otorgar permisos o roles se hace
  desde la consola, porque Solr reemplaza la lista entera.
- **Couchbase:** usa los comandos de usuarios de SQL++, que existen desde la
  versión 8.0.

## Contrato

- `Driver::security()` dice qué ofrece el motor (`SecuritySpec`): permisos,
  tipos de objeto, si crea usuarios y roles, si maneja contraseñas y
  membresías, y si es por base.
- `Session::principals()` y `Session::grants(nombre)` leen.
- `Driver::security_script(acción)` escribe el cambio.

Qué soporta cada motor está en
[`soporte-por-motor.md`](soporte-por-motor.md#usuarios-y-permisos).
