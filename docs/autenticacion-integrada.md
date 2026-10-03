# Autenticación integrada (Windows / Kerberos)

Algunos motores aceptan entrar con la cuenta del dominio (Active Directory)
en lugar de un usuario y una contraseña de la base. En DBine se elige en el
campo **Autenticación** del formulario de la conexión. Qué motores la tienen
y por qué los demás no: [`soporte-por-motor.md`](soporte-por-motor.md#autenticación-integrada-windows--kerberos).

## SQL Server

Dos opciones nuevas en **Autenticación** (solo en SQL Server: Azure SQL
Database, Microsoft Fabric y Babelfish no tienen logins de Windows):

| Opción | Pide | Windows | macOS y Linux |
|---|---|---|---|
| **Windows: usuario actual** | Nada: ni usuario ni contraseña | La sesión de Windows (SSPI) | El ticket de Kerberos de la sesión |
| **Windows: usuario y contraseña de dominio** | `DOMINIO\usuario` y contraseña | NTLMv2 | NTLMv2 (sin estar unido al dominio) |

La contraseña de dominio se guarda en el llavero del sistema, como las demás
contraseñas; nunca en el archivo de estado ni en los registros.

### En Windows

Con la computadora en el dominio, **Windows: usuario actual** entra con el
usuario que inició sesión, sin pedir nada. Por ahora el usuario actual se
autentica con NTLM a través de SSPI (no con Kerberos): si el dominio
bloquea NTLM, el servidor rechaza el login.

**Windows: usuario y contraseña de dominio** sirve para entrar con otra
cuenta del dominio, o desde una computadora que no está en él. El usuario va
como `DOMINIO\usuario` (por ejemplo, `CONTOSO\ana`).

### En macOS y Linux: Kerberos

**Windows: usuario actual** usa el ticket de Kerberos que tenga la sesión.
Para tenerlo:

1. La computadora tiene que llegar a un controlador de dominio (en la red de
   la empresa o por VPN).
2. Pedir el ticket, con el dominio en mayúsculas:

   ```sh
   kinit ana@CONTOSO.LOCAL
   klist        # muestra el ticket y cuándo vence
   ```

   En macOS también sirve la app **Ticket Viewer**
   (`/System/Library/CoreServices/Applications/Ticket Viewer.app`). Si el Mac
   está unido al dominio, el ticket se obtiene al iniciar sesión.
3. En Linux, `/etc/krb5.conf` tiene que conocer el dominio (`default_realm`
   y, si el DNS no los publica, los KDC). Hace falta la biblioteca
   `libgssapi_krb5` (paquete `libgssapi-krb5-2` en Debian y Ubuntu,
   `krb5-libs` en Fedora y RHEL), que casi todas las distribuciones traen.

En **Servidor** va el nombre completo del servidor en el dominio
(`sql01.contoso.local`), no una IP ni un alias: Kerberos busca el servicio
`MSSQLSvc/sql01.contoso.local:1433`, y ese nombre (SPN) tiene que estar
registrado en el dominio para la cuenta del servicio de SQL Server.

Si falta el ticket, venció o no hay KDC, la conexión lo dice y sugiere
`kinit usuario@DOMINIO`.

**Usuario y contraseña de dominio desde macOS y Linux:** funciona sin que
la máquina esté unida al dominio ni tenga un ticket: DBine habla NTLMv2 con
el usuario `DOMINIO\usuario` y la contraseña. Si el dominio bloquea NTLM,
usá **Windows: usuario actual** con `kinit` de esa cuenta.

## MongoDB

En **Autenticación**, **Kerberos (GSSAPI)**: en **Usuario** va el principal
(`ana@CONTOSO.LOCAL`) y no se pide contraseña. Usa el ticket de la sesión
(`kinit` en macOS y Linux; en Windows, la sesión de Windows). En
**Avanzado**, **Servicio de Kerberos** si el servidor no se registró como
`mongodb`. Kerberos es de MongoDB Enterprise: FerretDB y Amazon DocumentDB
no lo tienen.

## Motores por ODBC

Cada driver ODBC tiene sus atributos para Kerberos o la seguridad integrada
de Windows. Van en **Atributos adicionales** y reemplazan a los atributos del
mismo nombre que arma DBine (por ejemplo, el `AuthMech` de Hive). Usuario y
contraseña se dejan vacíos. Algunos ejemplos (revisá la documentación del
driver que tengas instalado):

| Motor | Atributos adicionales |
|---|---|
| IBM Db2 | `Authentication=KERBEROS` |
| Teradata | `MechanismName=KRB5` (o `TD2`, `LDAP`) |
| Apache Hive, Impala, Spark (drivers Simba / Cloudera) | `AuthMech=1;KrbRealm=CONTOSO.LOCAL;KrbHostFQDN=hive.contoso.local;KrbServiceName=hive` |
| Vertica | `KerberosServiceName=vertica;KerberosHostName=vertica.contoso.local` |

Con la conexión ODBC genérica, el DSN o la cadena de conexión ya pueden
llevar `Trusted_Connection=yes` o el atributo que pida el driver.
