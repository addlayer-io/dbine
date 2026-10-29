# Cambios

## [1.2.2] - 2026-10-28

### Correcciones
- **Seguridad: corregida una vulnerabilidad crítica en el manejo de ZIP.** El 28 de octubre se publicó una vulnerabilidad crítica en `zip`, la biblioteca que DBine usa para leer y escribir archivos basados en ZIP: importaciones y exportaciones de Excel, y la descarga del modelo integrado del asistente de IA ([GHSA-m665-v7pm-7grj](https://github.com/advisories/GHSA-m665-v7pm-7grj)). Un archivo especialmente armado podía mostrar contenidos distintos según cómo se lo leyera. Esta versión pasa todas las partes de DBine a la biblioteca corregida; sale unas horas después de la 1.2.1 solo por eso, así que se recomienda actualizar.
- **Trino y Presto, seguridad:** un tipo de columna anidado a más profundidad que cualquier esquema real (como podría enviarlo un servidor malicioso) se muestra tal como llegó en lugar de analizarse nivel por nivel, así que ya no puede hacer fallar a DBine.

## [1.2.1] - 2026-10-28

### Correcciones
- **El autocompletado de columnas sigue la consulta que estás escribiendo:** el editor ofrece solo las columnas de las tablas, CTE y subconsultas que lee la sentencia bajo el cursor, no las de otras sentencias del archivo. Una columna que comparten dos de esas tablas se ofrece una vez por tabla, rotulada con su alias e insertada calificada (`c.customer_id`), y las columnas de una CTE o de una tabla derivada se ofrecen dentro de la consulta y después de su alias.
- **Nombres de campo en filas y structs:** los valores `ROW` de Trino y Presto, los tipos compuestos de PostgreSQL y los structs de Arrow Flight SQL muestran cada campo con su nombre, como JSON, también en filas anidadas, arrays de filas y mapas cuyos valores son filas (`[{"customer_id":42,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]` en lugar de `[[42,[["demo-product",2]],null]]`). `null`, una cadena vacía y un array vacío siguen siendo distintos. Las tuplas con nombre de ClickHouse conservan el orden de campos declarado.
- **Migraciones con un login de SQL Server sin VIEW DEFINITION:** cuando el login de origen podía leer las tablas pero no sus definiciones (habitual en Azure SQL), el script de migración escribía `CHECK ()`, que fallaba con "Incorrect syntax near ')'", y descartaba los valores por defecto de las columnas sin avisar. Ahora esos CHECK, valores por defecto y expresiones calculadas se dejan fuera del script, y la vista previa y el informe de la ejecución listan cada uno al principio, con el motivo y el `GRANT VIEW DEFINITION` que permite a DBine leerlos. Los scripts generados y la sincronización de esquemas también lo informan, y una sincronización ya no elimina un CHECK del destino porque el origen no pudo leer el suyo. Ningún motor escribe ya un CHECK o DEFAULT vacío.

## [1.2.0] - 2026-10-27

### Nuevo
- **Ucraniano y polaco:** DBine y la app del teléfono están disponibles en Українська y Polski, con los mensajes del servidor y el asistente de IA incluidos. Elegí el idioma en **Configuración › General › Idioma**, o DBine sigue el del sistema.
- **Indonesio:** DBine y la app del teléfono están disponibles en Bahasa Indonesia, con los mensajes del servidor y el asistente de IA incluidos. Elegí el idioma en **Configuración › General › Idioma**, o DBine sigue el del sistema.

### Mejoras
- **Autocompletado entre bases de datos:** en los motores donde una consulta puede leer otra base de la misma conexión (ClickHouse, MySQL y MariaDB, SQL Server, Snowflake, Databricks, Trino, BigQuery, Oracle y más), el editor ofrece los nombres de las otras bases, sus tablas después de `otra_bd.` y sus columnas después de una tabla o un alias, también en joins entre bases. Las tablas de cada base se leen la primera vez que escribís su nombre con un punto.
- **Motor DuckDB actualizado a 2.0.0:** los archivos DuckDB locales y el DataFrame SQL de los notebooks ahora corren sobre DuckDB 2.0.0 (antes 1.5.5). Los archivos de base nuevos usan el formato de almacenamiento de DuckDB 2.0, que las versiones anteriores de DuckDB no pueden abrir; los archivos existentes se siguen abriendo y los puede leer la versión que los creó.
- **Cifrado de la conexión para SQL Server, PostgreSQL y MySQL:** el formulario de conexión muestra las opciones TLS propias de cada motor, con los nombres que usan sus herramientas: `Encrypt` (Optional, Mandatory, Strict), `TrustServerCertificate`, `ServerCertificate` y `HostNameInCertificate` para SQL Server y Azure SQL; `sslmode` y `sslrootcert` para PostgreSQL y su familia; `ssl-mode` y `ssl-ca` para MySQL y su familia; `ssl`, `ssl-verify-server-cert` y `ssl-ca` para MariaDB y OceanBase. Las conexiones nuevas van cifradas y verifican el certificado del servidor, y cada opción se puede desactivar. Las conexiones guardadas siguen funcionando exactamente igual. Por un túnel SSH, el certificado se verifica contra el nombre del servidor real, y un fallo de certificado indica qué opción cambiar. El `PREFERRED` de MySQL cae a una conexión sin cifrar solo cuando el servidor no ofrece TLS, antes de enviar cualquier contraseña.
- **Cifrado de la conexión para InfluxDB, IoTDB, ksqlDB, TDengine, Phoenix, Arrow Flight SQL y los servicios en la nube:** el formulario de conexión muestra las opciones TLS propias de cada motor: `ssl` y `unsafeSsl` para InfluxDB 1, el esquema de la URL con `skip-verify` (InfluxDB 2) o `tls-no-verify` y `tls-ca` (InfluxDB 3), `use_ssl` y `ca_certs` para IoTDB y TimechoDB, `useTls`, `verifyHost` y `trustStore` para ksqlDB, `useSSL` para TDengine, el esquema de la URL, `hostname_verification` y `truststore` para Phoenix y Avatica, `useEncryption` y `tlsRootCerts` para Flight SQL, y `DisableServerCertificateValidation` para el emulador de Cosmos DB; donde las herramientas de un motor no tienen nombre para omitir la verificación o para el archivo de la CA, `insecure` y `cacert` de curl. Las conexiones nuevas van cifradas y verifican el certificado del servidor, y cada opción se puede desactivar. Las conexiones guardadas siguen funcionando exactamente igual. Por un túnel SSH, el certificado se verifica contra el nombre del servidor real, y un fallo de certificado o un servidor sin TLS se informa como tal. BigQuery, Spanner, DynamoDB, Athena, Snowflake, Databricks y libSQL usan siempre HTTPS verificado, salvo un endpoint de emulador escrito como `http://`.
- **Teclado y lectores de pantalla:** un anillo de foco visible muestra dónde está el teclado; las filas de listas, las tarjetas, los encabezados ordenables y las secciones plegables se alcanzan con Tab y se usan con Enter o Espacio; los menús contextuales funcionan con las flechas; los botones que solo tienen ícono tienen nombres que lee un lector de pantalla; se anuncia el resultado de una consulta (terminada, o fallida con su error); las animaciones se detienen cuando el sistema pide movimiento reducido; y el texto secundario tiene más contraste en ambos temas.

### Correcciones
- **«No verificar» ahora fija el certificado del servidor, seguridad:** una conexión que no verifica el certificado del servidor (`sslmode=require`, `insecure`, `bolt+ssc`, `validate=false`…) acepta el certificado que el servidor presenta la primera vez y, desde entonces, solo ese, en PostgreSQL y su familia, los motores HTTP (ClickHouse, Elasticsearch y OpenSearch, CouchDB, Trino, InfluxDB y más), Neo4j, Neptune, Redis y Cassandra. Si el certificado cambia, la conexión se detiene y muestra las dos huellas, con **Confiar en el certificado nuevo** para un servidor que lo renovó; la pestaña SSL/TLS muestra la huella fijada, con **Olvidar**. Las conexiones guardadas fijan el certificado en su próxima conexión. `verify-ca` y las opciones que solo omiten el nombre de host ahora comprueban toda la cadena contra la CA en lugar de aceptar cualquier certificado. SQL Server, MySQL, MariaDB, MongoDB y SAP HANA mantienen el comportamiento propio de sus clientes.
- **Las bases nuevas aparecen sin reconectar:** una base creada con `CREATE DATABASE` en una consulta (o eliminada, o renombrada) aparece enseguida en el explorador y en las listas de bases, y al abrir una lista de bases (migración, comparar, subset, backups, permisos, el selector de base de la consulta…) se vuelve a leer, así que una creada desde otra herramienta también aparece.
- **Importar desde SQL Server Management Studio:** además de los servidores registrados, DBine ahora importa los servidores que recuerda el diálogo Connect de SSMS (su historial de conexiones), con su login, base y ajustes de cifrado, desde SSMS 18 hasta 20 y las carpetas nuevas de SSMS 21 en adelante. En macOS y Linux, el diálogo de archivos ahora acepta archivos `RegSrvr.xml`, `UserSettings.xml` y `.regsrvr` (y el `dataSources.xml` de DataGrip); antes solo se podían elegir archivos JSON.
- **Importar cadenas de conexión conserva su cifrado:** una cadena pegada o importada ahora trae sus opciones TLS como las del propio motor: `Encrypt` de SQL Server (incluido `Strict`) y `HostNameInCertificate`, el `sslmode` de PostgreSQL tal como está escrito (`require` ya no se convierte en una conexión verificada), el `ssl-mode` de MySQL y el `useSSL` de Connector/J, el `sslMode` de MariaDB, y los esquemas `rediss://`, `bolt+s://` y `bolt+ssc://`. Una cadena sin ajustes TLS se importa como se conectaría su propio cliente. Lo mismo vale para las conexiones importadas desde DBeaver, DataGrip y los demás IDE de JetBrains, DbGate y Azure Data Studio.
- **Errores de certificado en macOS y Windows:** un certificado de servidor en el que el sistema no confía («The certificate was not trusted», «issued by an authority that is not trusted», «The target principal name is incorrect») ahora se reconoce como tal, y el mensaje dice qué opción TLS cambiar en lugar de mostrar un error TLS genérico.
- **ClickHouse 22.8 y anteriores:** conectar ya no falla con `Unknown setting http_write_exception_in_output_format`; DBine deja de pedir ese ajuste a los servidores que no lo tienen.
- **Notebooks, exportación a Python:** una conexión PostgreSQL que verifica el certificado del servidor se exporta con `sslmode=verify-full`, como hace **Copiar cadena de conexión**, en lugar de `require`, que no lo verifica.
- **HashiCorp Vault, seguridad:** la dirección de Vault de una fuente de contraseñas debe ser `https://`; `http://` plano solo se acepta para un Vault en esta computadora, como en los otros gestores de secretos, así que el token de Vault nunca viaja en claro por la red.

## [1.1.0] - 2026-10-21

### Novedades
- **Estado de la sincronización en la nube de un vistazo:** la barra de estado muestra **Sincronizado** con un punto verde, **Por subir** en ámbar mientras un cambio espera para subirse, y rojo ante un error; pasá el mouse para ver la hora de la última sincronización y hacé clic para abrir la configuración de sincronización.
- **Copiar cadena de conexión:** hacé clic derecho en una conexión (o usá el botón de su formulario) y elegí **Copiar cadena de conexión…** para obtenerla como cadena URI, ADO.NET, JDBC, SQLAlchemy, ODBC, libpq, Node.js o Go, las que correspondan a su motor, para cualquier base de datos, con su configuración de TLS e inicio de sesión. La contraseña queda afuera (un marcador `${DB_PASSWORD}`) salvo que actives **Incluir contraseña**; en ese caso se obtiene al copiar y se borra del portapapeles un minuto después si sigue ahí.
- **Paso Enviar un mensaje** en las tareas programadas: publicá cómo salió la ejecución en Microsoft Teams, Slack, Telegram o ntfy, con las variables de la tarea (`{results}` lista lo que hizo cada paso), **Solo si…** como el paso de correo y un botón **Probar**. La URL del webhook y los tokens se guardan en el llavero, nunca en la tarea, y los mensajes viajan solo por HTTPS a los servidores del propio servicio.

### Correcciones
- **Túneles SSH, seguridad:** se actualiza la biblioteca SSH para corregir dos avisos (CVE-2026-111061, una denegación de servicio a través del agente SSH, y CVE-2026-111062).
- **Claves RSA, seguridad:** el inicio de sesión en SSH con una clave RSA y la apertura de las claves de organización de Zoho Vault y Bitwarden ahora se ejecutan en AWS-LC en tiempo constante, así que la fuga por tiempos de la biblioteca RSA (RUSTSEC-2023-0071, «Marvin») ya no se aplica. Las claves RSA firman con SHA-256 o SHA-512; un servidor que solo acepta SHA-1 (`ssh-rsa`) pide una clave Ed25519 o ECDSA, o el agente SSH.
- **TLS, seguridad:** se actualizan la biblioteca TLS y sus verificaciones de certificados en DBine y en 33 drivers (RUSTSEC-2026-0346 a 0349 y 0381 a 0383), de modo que se rechazan los certificados de autoridades en las que ya no se confía. Los drivers actualizados se descargan solos.
- **Motor de IA integrado:** al descomprimir el motor solo se conservan los permisos de lectura, escritura y ejecución de los archivos.
- **Tareas programadas, seguridad:** `{results}` y `{alerts}` solo completan el texto de correos y mensajes. El SQL de un script, el nombre de un archivo o carpeta, una dirección o la ruta de un adjunto ya no los toman, así que un error de la base de datos que repite datos manipulados no puede convertirse en SQL ni en una ruta.
- **Mensajes de las tareas programadas:** `{results}` y `{alerts}` en un mensaje o un correo salen en el idioma de la interfaz de DBine en vez de siempre en español, también cuando la tarea corre con DBine cerrado.
- **Notebooks, resultados grandes de DataFrame SQL:** **Exportación avanzada… › Todas** escribe el resultado completo, no solo las filas que se ven. El SQL de la celda se vuelve a ejecutar en DuckDB sobre los DataFrames que leyó su última ejecución, sin volver a leer archivos ni a consultar las fuentes. Sus gráficos arrancan calculados sobre todo el resultado, y se puede desactivar. Cuando un resultado está cortado, la grilla también avisa que el orden y **Copiar todo** solo abarcan las filas cargadas.

## [1.0.0] - 2026-10-19

### Novedades
- **Tema claro:** un tema claro y una opción **Igual que el sistema** (la predeterminada) que sigue a tu sistema operativo cuando cambia; zoom de la interfaz con Cmd/Ctrl + "+", "−", "0" o la rueda del mouse, y tamaños de fuente separados para el editor y la grilla de resultados, en **Configuración › General**.
- **Modo Vim y tus propios atajos:** modo Vim para el editor SQL (Configuración › General), y cada atajo de teclado se puede cambiar en **Configuración › Atajos de teclado**, con búsqueda, restablecimiento y avisos de conflicto.
- **Editor dividido:** hasta tres grupos de editores uno al lado del otro o uno arriba del otro (el menú de la pestaña, el botón de la barra de pestañas o Cmd/Ctrl+\); arrastrá pestañas entre grupos o sobre la mitad derecha o inferior de un grupo para dividirlo. La disposición se restaura al reiniciar.
- **Inicio de sesión en la nube para bases administradas:** las conexiones PostgreSQL y MySQL a Amazon RDS y Aurora, Google Cloud SQL y AlloyDB, y Azure Database pueden iniciar sesión con **AWS IAM**, **Google Cloud IAM** o **Microsoft Entra ID**. DBine genera un token nuevo desde tu perfil de AWS (SSO incluido), gcloud o az antes de cada conexión, así que se acabaron los tokens que había que pegar y vencían a los 15 minutos, y **Probar** muestra con qué identidad se inició sesión.
- **Contraseñas desde un comando o un gestor de secretos:** cualquier campo secreto de cualquier motor, y la contraseña y la frase secreta de la clave del túnel SSH, pueden tomar su valor de un comando, 1Password, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault o Google Secret Manager. Se obtiene en cada conexión y se guarda solo en memoria, con una opción de clave JSON y una prueba por campo que muestra solo la longitud. Un comando o un gestor de secretos que venga de una importación o de un respaldo restaurado se ejecuta únicamente después de que lo confirmes en esta computadora.
- **KeePass y Zoho Vault como fuentes de contraseñas:** cualquier campo secreto (y el del túnel SSH) puede tomar su valor de una entrada de una base KeePass (.kdbx, que DBine lee por sí mismo; la contraseña maestra sale del llavero, se pide al conectar o viene de un comando; archivo de clave opcional) o de un secreto de Zoho Vault (cualquier centro de datos, descifrado en tu computadora con tu frase secreta maestra), y si querés, el usuario de la conexión sale de la misma entrada.
- **Credenciales de base de datos de Vault, CyberArk y Delinea como fuentes de contraseñas:** las contraseñas de las conexiones pueden venir del motor de secretos de bases de datos de HashiCorp Vault (credenciales generadas para la conexión, renovadas mientras está abierta y revocadas al cerrarla; también inicio de sesión AppRole para Vault), de CyberArk Conjur, del Central Credential Provider de CyberArk (con certificados de cliente) y de Delinea Secret Server.
- **Bitwarden, Bitwarden Secrets Manager y AWS Parameter Store como fuentes de contraseñas:** cualquier campo secreto (y el del túnel SSH) puede tomar su valor de un ítem de Bitwarden, personal o de una organización, en bitwarden.com, bitwarden.eu o tu propio servidor, descifrado en tu computadora con tu contraseña maestra; de un secreto de Bitwarden Secrets Manager (con el token de acceso de una cuenta de máquina); o de un parámetro de AWS Systems Manager (SecureString, String o un elemento de una StringList), con una clave JSON y, si querés, el usuario de la conexión del mismo ítem.
- **Primero las credenciales, y explorá tu gestor de secretos:** el formulario de conexión pide primero las **Credenciales**: guardar la contraseña en el llavero, pedirla cada vez, o tomar el usuario y la contraseña de una línea de comandos o de un gestor de secretos, cuyos campos los reemplazan. Un botón **Examinar** lista las entradas de un gestor y los nombres de sus campos para que los elijas en vez de escribir las referencias; muestra solo nombres, nunca valores.
- **Usuario y contraseña desde la entrada:** con KeePass, Bitwarden, 1Password, Zoho Vault, Delinea o CyberArk CCP como credenciales, tanto el usuario como la contraseña salen de la entrada, y **Configuración avanzada** te deja elegir otros campos de la lista de la propia entrada. Cada fuente pide primero su inicio de sesión y la entrada al final, y Examinar queda listo cuando eso está completo.
- **Cuentas de Zoho Vault, conectadas una sola vez:** un asistente guiado **Conectar Zoho Vault** te lleva por la consola de Zoho, encuentra tu centro de datos y verifica tu frase secreta maestra; después cualquier conexión solo elige la cuenta. Las cuentas quedan en esta computadora y se administran en **Configuración › Gestores de secretos**.
- **Cuentas para cada gestor de secretos:** Bitwarden, Bitwarden Secrets Manager, HashiCorp Vault y OpenBao, Delinea, CyberArk Conjur y Central Credential Provider, 1Password, AWS, Azure Key Vault, Google Secret Manager y KeePass ahora se conectan una sola vez en **Configuración › Gestores de secretos** con un asistente guiado que explica dónde está cada valor, verifica la cuenta en vivo y muestra quién inició sesión; después cualquier conexión solo elige la cuenta, y las credenciales van únicamente al servidor propio de ese gestor.
- **Tenants de Azure para elegir:** el asistente de cuentas de Azure Key Vault lista tus tenants de Microsoft Entra (igual podés escribir uno), y cuando no iniciaste sesión en el tenant elegido muestra el comando `az login --tenant` para copiar.
- **Autocompletado en las celdas DataFrame SQL:** las celdas de arriba se sugieren como tablas (incluso las que todavía no se ejecutaron), con sus columnas después de `nombre.` o de un alias y en la lista del select, y los lectores de archivos de DuckDB después de FROM y JOIN; al elegir un lector, el cursor queda entre sus comillas, donde **Elegir archivo…** abre el diálogo de archivos. La lista de tablas después de FROM ya no se cierra sola mientras el notebook se actualiza.
- **Vista de mapa:** los resultados con columnas de geometría (PostGIS, MySQL, SQL Server, SQLite/SpatiaLite, DuckDB, MongoDB, Elasticsearch y más) o columnas de latitud/longitud se pueden mostrar en un mapa, dibujado en tu máquina sobre un mapamundi integrado que funciona sin conexión. Los mosaicos de mapa en línea con más detalle se pueden activar en **Configuración › Mapas**; el proveedor ve entonces qué zona estás mirando, nunca los datos.
- **Oracle en el mapa:** las columnas SDO_GEOMETRY ahora se muestran en el mapa de resultados, tanto en los datos de una tabla como en tus propias consultas (convertidas en el servidor a WKT con su SRID); esas celdas son de solo lectura en la grilla.
- **Abrir archivos de datos directamente:** los archivos SQLite, DuckDB, CSV, TSV, Parquet, JSON y Excel se abren con doble clic o con «Abrir con DBine», soltándolos sobre la ventana, o con **Archivo › Abrir archivo…** (Cmd/Ctrl+O). Cada uno se abre como una conexión temporal que podés conservar con **Guardar conexión**, y los archivos de datos muestran sus primeras 1.000 filas enseguida.
- **Paleta de comandos:** Cmd/Ctrl+Shift+P encuentra y ejecuta cualquier acción, mostrando solo lo que soporta el motor actual, y **Ir a…** (Cmd/Ctrl+P) salta a conexiones, bases, tablas, vistas, rutinas, consultas guardadas y pestañas abiertas. A cada acción de la paleta también se le puede asignar su propio atajo.
- **Saltar a filas relacionadas:** en los datos de una tabla o en los resultados de una consulta, **Ir a la fila referenciada** (o Cmd/Ctrl+clic) abre la tabla padre filtrada por esa clave foránea, y **Filas que la referencian** abre cada tabla hija filtrada por el valor, claves compuestas incluidas. En todos los motores con claves foráneas.
- **Notebooks SQL:** **Archivo › Nuevo notebook** abre un notebook: celdas SQL que se ejecutan en cualquier conexión, cada una con su resultado y su gráfico debajo, y celdas de texto para explicarlas. Se guarda como un `.ipynb` estándar de Jupyter que GitHub, VS Code y Jupyter muestran, y que también abren Azure Data Studio y los notebooks `%%sql` de Jupyter. El archivo conserva una vista previa de cada resultado (100 filas y 1 MB por celda, en **Configuración › Notebooks**) con su total de filas, hora y duración, nunca datos de conexión ni contraseñas; dentro de un proyecto, los notebooks se guardan sin resultados salvo que lo actives. El asistente puede explicar un resultado, proponer la próxima celda o una versión agrupada de una consulta grande, sin ver las filas. Exporta a HTML o Markdown.
- **Gráficos sobre todo el resultado:** los gráficos se calculan sobre todas las filas del resultado, no solo las cargadas en la grilla. En los motores SQL la agrupación se hace en el servidor, en las celdas DataFrame SQL de un notebook la hace DuckDB, y en los demás motores la hace DBine.
- **Más tipos de gráfico:** histograma, mapa de calor, diagrama de caja (con bigotes y valores atípicos), barras con líneas y KPI (el total de todo el resultado).
- **Varios gráficos por celda de notebook:** una celda de notebook puede mostrar más de un gráfico de su resultado, y recuerda qué fuente usa su mapa.
- **Gráficos en las exportaciones de notebooks:** los notebooks exportados a HTML o Markdown incluyen sus gráficos como imágenes.
- **Cruzá resultados de distintas bases en un notebook:** el resultado de cada celda es un DataFrame con el nombre de la celda (visible y editable en su encabezado), y una celda **DataFrame SQL** consulta y une los DataFrames de arriba como tablas con el SQL de DuckDB y sin conexión, así que una celda puede unir un resultado de PostgreSQL con uno de SQL Server y uno de MongoDB. Se ejecuta en una base DuckDB en memoria que no puede escribir archivos ni acceder a la red, y se descarta al cerrar el notebook; las filas de cada fuente salen de su ejecución en esta sesión o de su consulta ejecutada de nuevo, en solo lectura, en su propia conexión (hasta 1.000.000 de filas por DataFrame, en **Configuración › Notebooks**).
- **Notebooks como informes programados:** una tarea programada puede ejecutar un notebook con DBine cerrado (**Ejecutar notebook**), guardar la copia ejecutada y exportarla a HTML o Markdown, y el siguiente paso **Enviar un mail** puede adjuntarla o enviar la exportación HTML como cuerpo del mail. Nada se pregunta sin supervisión: lee solo los archivos con **Permitir siempre** para ese notebook, se ejecuta en entornos que piden confirmación solo con el permiso explícito del paso, y sus celdas que modifican datos se suman a la aprobación de la tarea. Las exportaciones programadas incluyen tablas; los gráficos se incluyen al exportar desde la app.
- **Archivos locales en celdas DataFrame SQL:** una celda DataFrame SQL puede leer archivos CSV, Parquet, JSON o NDJSON y Excel de tu computadora con `read_csv`, `read_parquet`, `read_json` y `read_xlsx`, incluidos patrones como `data/*.csv` y listas de archivos, archivos geográficos (GeoJSON, Shapefile con sus archivos complementarios, GeoPackage, KML, GPX, FlatGeobuf) con `st_read`, y tablas de archivos de base SQLite y DuckDB con `sqlite_scan` y `read_duckdb`, cada uno como un DataFrame que se puede unir con los resultados de celdas de cualquier motor; las geometrías siguen funcionando con funciones espaciales como `ST_Contains` y se muestran en el mapa. Los archivos solo se leen, nunca se escriben. Antes de leer un archivo por primera vez, el notebook pregunta: permitirlo para esta ejecución, siempre para este notebook en esta computadora (un patrón cubre sus archivos nuevos) o denegarlo; las credenciales, las claves, el historial del shell y los archivos del sistema nunca se leen. Si el contenido de un archivo cambió desde la última ejecución, el resultado lo indica. Configuración › Notebooks lista los archivos que cada notebook puede leer y permite quitarlos. Los mismos lectores también leen archivos de la web (`https://`) y de Amazon S3, Google Cloud Storage y Azure Blob Storage (`s3://`, `gs://`, `az://`, `abfss://`, con patrones como `s3://bucket/2026/*.parquet`), con la misma aprobación, que muestra primero el servidor; DBine los descarga por sí mismo a una carpeta temporal privada que se elimina después de la lectura, usando el inicio de sesión de AWS, Google Cloud o Azure de esta computadora (en Configuración › Notebooks se puede elegir un perfil de AWS), y nunca llega a direcciones locales ni de redes privadas, ni sigue redirecciones a otros servidores, ni acepta credenciales en la URL. Un GeoPackage abierto en otro programa ahora incluye los cambios que todavía están en su `-wal`.
- **Exportar un notebook como notebook de Python:** **Exportar › Notebook de Python (.ipynb)** escribe un notebook de Jupyter que lo reproduce: un motor SQLAlchemy (o el cliente de Python propio del motor, como pymongo o redis) por conexión, cada celda de consulta leída en un DataFrame de pandas con el nombre de la celda, las celdas DataFrame SQL ejecutadas con DuckDB sobre esos DataFrames, y las celdas de texto como Markdown. Las contraseñas y los tokens nunca se exportan: el código los lee de variables de entorno, que la primera celda lista junto con los paquetes a instalar. Los resultados se omiten salvo que elijas incluir las vistas previas.
- **Escribir un DataFrame en una tabla desde un notebook:** una **celda de escritura** (que se agrega debajo de cualquier celda con **Escribir en una tabla…**) escribe todo el resultado de una celda en una tabla de SQL Server, PostgreSQL, MySQL, MariaDB, Oracle, SQLite, DuckDB, MongoDB y más: insertar, actualizar por clave, insertar o actualizar, o reemplazar el contenido de la tabla, con un mapeo de columnas, una vista previa y una tabla nueva si querés. Cada escritura pregunta antes, mostrando dónde, cómo y cuántas filas (las conexiones de producción piden que escribas el nombre de la tabla), se ejecuta en una sola transacción en los motores que las tienen, y las conexiones de solo lectura nunca se escriben. En una tarea programada, una celda de escritura se ejecuta solo cuando el paso autoriza esa celda, tabla y modo.
- **Parámetros de notebook:** un notebook puede definir parámetros (texto, número, fecha, fecha y hora, booleano o una lista fija, con un valor predeterminado) que sus celdas usan como `{{name}}`; al ejecutar se piden los valores que faltan, cada valor llega a la base como dato (enlazado por el driver o entrecomillado con las reglas propias del motor), y cada resultado muestra los valores que lo produjeron.
- **Cinco idiomas más:** DBine y su app para el teléfono ahora hablan alemán, ruso, chino simplificado, japonés y coreano, además de español, inglés, portugués, francés e italiano; el idioma sigue al de tu sistema y solo se carga el que se usa.

### Mejoras
- **Firmada en macOS:** la app de macOS está firmada con un Apple Developer ID y notarizada por Apple, así que se abre sin el aviso de «no se puede verificar al desarrollador» ni ningún comando de terminal. La primera vez que se abre la versión firmada, macOS pregunta una sola vez si DBine puede usar su clave en el llavero: elegí **Permitir siempre**.
- **El inicio de sesión en la nube solo confía en el certificado de la instancia:** las conexiones que inician sesión con AWS IAM, Google Cloud IAM o Microsoft Entra, y las conexiones a Cloud SQL y AlloyDB con el archivo de CA de la instancia, ahora aceptan solo esa CA (o el bundle de RDS de Amazon), no cualquier certificado público, antes de enviar un token o una contraseña. Un servidor que pide la contraseña en texto plano mientras **Confiar en el certificado del servidor** está activado ahora falla en vez de recibirla.
- **Firmada en Windows:** el instalador de Windows y la app están firmados con un certificado de firma de código, así que Windows muestra quién publica DBine en vez de «Editor desconocido».

### Correcciones
- **Monitor del servidor:** sus cifras, tablas y notas están traducidas en todos los motores, en lugar de mostrar algunas etiquetas en español.
- **Asistente de IA y optimizador:** las instrucciones al modelo van en el idioma de la interfaz, así que un modelo local responde en ese idioma en lugar de en español. La línea que cuenta qué se envió al modelo también está traducida.

## [0.1.14] - 2026-10-17

### Nuevo
- **DBine en tu celular (Android):** en **Configuración › Dispositivos**, activá **Permitir celulares** y vinculá la app de DBine del celular escaneando un código QR; después confirmá en la computadora que ambas pantallas muestran el mismo código. Desde el celular ves el Monitor de cada conexión, quién bloquea a quién, las sesiones en curso y el chequeo de salud de una base; en **Actividad**, las consultas que ejecutás en el editor de DBine y las tareas programadas, con su detalle. Si permitís ese celular, terminás una sesión, cancelás una consulta, o ejecutás o cancelás una tarea, siempre con tu huella, rostro o PIN. El celular te avisa cuando termina una consulta larga y cuando una tarea falla o tiene una alerta, también con la app cerrada (cada 15 minutos). Tus conexiones y contraseñas no salen de la computadora: el celular le pide a DBine por un canal cifrado de punta a punta.
- **Acceso del celular desde fuera de tu red:** desactivado por defecto (solo red local). Activalo en **Configuración › Dispositivos › Fuera de la red local**, mediante tu propio servidor de paso o los servidores públicos de iroh, que ven direcciones IP pero nunca el contenido. Cómo está protegido: [dbine.com/es/phone-security](https://dbine.com/es/phone-security).
- **Las consultas largas mantienen la computadora despierta:** mientras una consulta del editor lleva 30 segundos o más ejecutándose, DBine evita que la computadora se suspenda, para que no se corte la conexión.
- **Descarga de la app del celular:** la app del celular (Android) se publica con cada versión y en dbine.com.

## [0.1.13] - 2026-10-16

### Mejoras
- **Apoyar el proyecto:** el botón del corazón (**Apoyar el proyecto**), su recordatorio y **Ayuda › Apoyar el proyecto** abren Buy Me a Coffee.
- **Script con datos de tablas elegidas:** en **Generar script de la base**, con **Datos (INSERT)** tildado podés dejar los datos de todas las tablas o elegir cuáles los llevan, desde una lista con filtro (**Elegir tablas…**), así que el script puede llevar la estructura de toda la base y las filas de unas pocas tablas.
- **Menú de la base:** se quitó **Exportar base…**. Abría el mismo diálogo que **Generar script…**, que sigue y hace todo lo que hacía (estructura, datos y guardar a un archivo).
- **Estadísticas de uso:** cuando el servidor de estadísticas no responde o falla, DBine reintenta hasta por una hora en lugar de descartar el evento. No cambia nada más de lo que se envía, y desactivar las estadísticas frena cualquier reintento pendiente.
- **Descarga para Flatpak:** cada versión ahora incluye un archivo `.flatpak` para Linux (`x86_64` y `aarch64`), que se instala con `flatpak install --user ./dbine-X.Y.Z-x86_64.flatpak`; Flatpak baja solo el runtime de GNOME desde Flathub. No se actualiza solo: instalá el archivo de la versión siguiente de la misma manera.

## [0.1.12] - 2026-10-15

### Nuevo
- **Flathub:** DBine se envió a Flathub como `io.addlayer.dbine`. Esa compilación incluye todos los drivers, DuckDB y el motor del asistente de IA, así que no se descarga nada la primera vez que se usan, y Flatpak la mantiene actualizada en lugar de DBine. Dentro del sandbox, la acción del firewall de Azure explica que no puede usar Azure CLI y ofrece el portal, T-SQL o una terminal, y las tareas programadas se guardan pero no se pueden registrar en el programador del sistema.
- **winget:** DBine se envió a winget en Windows como `AddLayer.DBine` (`winget install AddLayer.DBine` una vez que lo acepten), y cada versión nueva le llega sola. Instalado con winget, DBine se sigue actualizando solo como antes.

### Mejoras
- **Nuevo identificador de la app:** DBine ahora es `io.addlayer.dbine`. En el primer inicio después de actualizar, tus conexiones, contraseñas guardadas, pestañas abiertas, chats del asistente, ajustes y tareas programadas se copian solos; los datos de la versión anterior quedan como estaban, así que volver a ella no pierde nada. En Windows y Linux puede que se vuelva a descargar un modelo de IA grande. En macOS el llavero puede pedir acceso una vez, y es posible que se vuelvan a pedir algunos permisos de privacidad (como el acceso a la red local); en Windows, un anclaje a la barra de tareas hecho con una versión anterior puede necesitar anclarse de nuevo.

## [0.1.11] - 2026-10-13

### Nuevo
- **Ordenar por columna:** hacé clic en el encabezado de una columna en los datos de una tabla para ordenar por ella (otra vez para descendente, una tercera para quitar el orden; Shift+clic suma más columnas). La base ordena la tabla entera donde el motor lo permite, igual que los filtros; en los demás casos, o cuando el servidor rechaza el orden, se ordenan las filas cargadas y un aviso explica por qué. Los resultados de una consulta se pueden ordenar de la misma manera.
- **Controles en la barra de estado:** en una pestaña de consulta, la barra de estado azul ahora muestra la conexión, la base de datos, el modo de transacción (Auto por defecto, o Manual) y el límite de filas. Hacé clic en cualquiera para cambiarlo con un selector rápido, como en VS Code. La barra de herramientas de la consulta ya no pierde botones cuando la barra lateral es ancha: lo que no entra va bajo un menú **…**.
- **Enviar comentarios:** un botón al pie de la barra de actividad abre un formulario corto para reportar un error, sugerir una mejora, pedir un motor nuevo o hacer una pregunta. Se abre GitHub con el issue ya completo y con sus etiquetas, así que solo tenés que apretar «Create». El diagnóstico opcional (versión de DBine, sistema operativo, idioma, y el motor y la versión del servidor de la conexión activa) se muestra antes de incluirlo, y nunca lleva servidores, usuarios, nombres de bases ni consultas.

### Mejoras
- **Texto largo en la grilla:** al detenerte sobre una celda cuyo texto no entra, se muestra completo en un tooltip que respeta los saltos de línea y se puede desplazar y seleccionar.
- **Cuando una conexión falla:** al expandir una conexión primero se conecta y muestra **Conectando…**; el árbol aparece recién cuando el servidor responde (ahí al instante, con lo que DBine recuerda, mientras se actualiza). Si no puede conectar, la conexión muestra un aviso con **Reintentar** y **Ver qué pasa…**, y las pestañas (los datos de una tabla, la ejecución de una consulta y las demás herramientas) muestran el mismo error en lugar de quedar en blanco. Si la conexión se cae más tarde, vuelve al aviso. **Ver qué pasa…** explica la falla (firewall, tiempo de espera, rechazo, nombre del host, certificado o inicio de sesión) y qué revisar, y en Azure SQL puede agregar tu IP al firewall del servidor con tu sesión de Azure CLI, después de confirmar el servidor, la regla y la IP. Funciona en todos los motores; la causa es más precisa en SQL Server, Azure SQL, PostgreSQL y MySQL.

### Correcciones
- **Editor de consultas:** una pestaña cuya conexión todavía no está abierta (por ejemplo, restaurada al iniciar) vuelve a completar nombres de esquemas, tablas y columnas: al pedir el autocompletado se abre la conexión y la lista aparece cuando llegan los nombres. Los snippets ya no se mezclan en la lista después de un punto.
- **Editor de consultas:** la lista de autocompletado y las ayudas se abren hacia arriba cuando escribís en las últimas líneas, en lugar de quedar tapadas por los resultados.
- **Firewall de Azure SQL:** **Agregar mi IP al firewall de Azure** usa solamente la IP pública de esta máquina, consultada por HTTPS y verificada de nuevo antes de crear la regla. Una IP mencionada en el mensaje de error de un servidor se muestra como no verificada y nunca se usa.
- **Reintentar tras una conexión fallida:** si mientras tanto la pestaña pasó a otra conexión o base de datos, al reintentar se vuelve a pedir la confirmación de producción.
- **Renombrar en Snowflake y en Trino, Presto y Starburst:** lo que nombra al objeto renombrado (vistas, vistas materializadas, funciones, procedimientos y, en Snowflake, tareas, streams y tablas dinámicas) se lista para que lo corrijas a mano en lugar de recrearse, así conserva su propietario y su configuración de seguridad. Además, el renombrado se rechaza cuando el código almacenado se partiría en varias sentencias, y reescribir los dependientes ya no cambia con qué identidad corre el código (`DEFINER`, `SQL SECURITY`, `AUTHID`, `EXECUTE AS`).
- **Consultas de los clientes de IA y del asistente:** una consulta corre sin pedir aprobación solo cuando la base la hace cumplir como de solo lectura y no llama más que a funciones propias del motor, sin efectos secundarios. Una que llame a cualquier otra función (una función de usuario, o una que actúe fuera de la transacción, como pausar la replicación o tomar un lock) espera antes tu aprobación.

## [0.1.10] - 2026-10-10

### Nuevo
- **Los drivers se actualizan solos, aparte de la app:** DBine busca en un índice firmado el driver más nuevo compatible con tu versión, lo descarga en segundo plano y vuelve al anterior si algo falla. En Configuración › Drivers hay un botón **Buscar actualizaciones**, el estado de cada driver y **Volver a la anterior**. Un driver puede publicarse solo, sin una versión nueva de la app.
- **Renombrar una base:** **Renombrar…** sobre una base en el explorador. El diálogo muestra las otras sesiones abiertas sobre ella (que el renombrado cierra), si el nombre nuevo ya existe, cuántos objetos se mueven y el script completo. Después, la base predeterminada de la conexión, las pestañas abiertas, las consultas guardadas, las migraciones, los destinos de proyectos y los pasos de tareas programadas siguen el nombre nuevo; las tareas que modifican datos en ella piden aprobación de nuevo. Disponible en SQL Server, Azure SQL, Babelfish, la familia PostgreSQL, MySQL, MariaDB, Snowflake y MongoDB; donde el motor no puede renombrar (o mover) una base, no se ofrece.
- **¿A qué columna corresponde este valor?** En un `INSERT … VALUES`, al poner el cursor sobre un valor aparece un tooltip con su columna (por ejemplo "Columna 14 de 48: Name") y se resalta esa columna en la lista. Sin lista de columnas, usa las de la tabla en orden. Funciona en todos los motores SQL y CQL.
- **Más formas de entrar a SQL Server con Microsoft Entra ID:** interactiva con MFA (en el navegador), integrada (la cuenta de Windows, a través del ADFS federado de la organización), identidad administrada y predeterminada (variables de entorno, identidad administrada, Azure CLI o Azure Developer CLI). El token se conserva mientras DBine está abierto y se renueva antes de que venza.
- **Asignar un login a una base:** **Asignar login…** en Usuarios y permisos crea el usuario de la base para un login del servidor que ya existe (`CREATE USER … FOR LOGIN`, con su schema por defecto), y lista los logins que todavía no tienen usuario ahí. Disponible en SQL Server, Azure SQL Database (ahí el login se escribe), Babelfish y SAP ASE.

### Mejoras
- **Qué trae cada versión:** el aviso de versión nueva muestra sus cambios y los de las versiones intermedias, a partir de la que tenés instalada, en el idioma de la app.
- **Versiones viejas de la app:** de ahora en más, una app anterior a las últimas cinco versiones tiene que actualizarse para descargar drivers nuevos. Los drivers que ya tiene instalados siguen funcionando.
- **Usuarios y permisos:** los cambios se encolan y se aplican juntos. Una barra de **Cambios pendientes** los lista, una sola revisión muestra todo el script y se ejecutan de a uno: los aplicados salen de la lista y, si uno falla, ese y los que siguen quedan pendientes.
- **Las lecturas de los clientes MCP y del asistente de IA las protege la propia base de datos:**
  - **Asegurado por el servidor:** en la familia PostgreSQL, MySQL, MariaDB, SQLite y libSQL, cada lectura corre como una única sentencia dentro de una transacción de solo lectura que el servidor hace cumplir y que DBine siempre revierte.
  - **Aprobación:** en los demás motores, cada lectura pide tu aprobación en DBine, igual que las escrituras. El diálogo de aprobación indica si es una lectura o una escritura, y **Aprobar todo** se aplica por separado a las lecturas y a las escrituras.
  - **Control de escrituras:** DBine sigue revisando cada consulta en busca de escrituras, como una capa extra.

### Correcciones
- **El solo lectura es más estricto:** las consultas de los clientes MCP con nivel Lectura, del asistente de IA y de las conexiones de solo lectura se revisan palabra por palabra, no solo por la primera. Ahora rechazan:
  - una escritura escondida después de una lectura en un lote de SQL Server;
  - un `WITH` que modifica datos;
  - `SELECT … INTO`, `EXEC` y `SET`;
  - funciones que actúan fuera de la consulta, como `set_config`, `dblink_exec` y `xp_cmdshell`.

  Un plan estimado rechaza los scripts que apagarían el modo plan, así que las alternativas de IA de **Optimizar consulta** no pueden ejecutar nada. Las tareas programadas cuyos scripts ahora cuentan como escritura piden aprobación de nuevo.
- **Biblioteca con git:** un repositorio compartido ya no puede hacer que DBine lea, escriba o borre archivos fuera de la carpeta de la Biblioteca.
- **Túneles SSH:** la clave de host de cada servidor se verifica por separado. Una clave aceptada para un salto ya no vale para el servidor siguiente, `known_hosts` se consulta primero y una clave cambiada siempre se rechaza. A los servidores que aceptaste antes se les pregunta una vez más. El puerto local del túnel solo atiende a los programas de tu propio usuario.
- **Backup en la nube:** un archivo de backup modificado por otra persona ya no puede debilitar el cifrado de tu próxima subida.
- **libSQL / Turso:** un `authToken` en una URL pegada se guarda en el llavero del sistema, no en la dirección, el nombre ni el historial de la conexión. Las conexiones guardadas antes se limpian al iniciar DBine.
- **Documentar la base:** los nombres de columna no pueden inyectar HTML en el diccionario de datos en Markdown.
- **Notificaciones de tareas en Windows:** un mensaje de error de la base ya no puede ejecutar comandos a través de la notificación. Las notificaciones en macOS y Linux también reciben el texto como argumentos separados.
- **Túneles SSH:** un servidor cuya clave aceptaste en DBine y que ahora presenta otra distinta se rechaza, en lugar de preguntarte de nuevo. La sección SSH de la conexión lista los servidores aceptados, cada uno con **Olvidar**.
- **SQL contra PostgreSQL:** los valores de texto se escriben como `E'…'` con las barras invertidas escapadas, así que un valor no puede cerrar la cadena antes de tiempo en un servidor con `standard_conforming_strings` apagado. Esto cubre la familia PostgreSQL, CockroachDB y motores similares.
- **Scripts de Snowflake:** una barra invertida dentro de un `"nombre entre comillas"` ya no cambia dónde termina una sentencia.
- **Exportación CSV y TSV:** las celdas de texto y los nombres de columna que empiezan con `=`, `+`, `-`, `@`, un tabulador o un retorno de carro llevan un `'` adelante, para que las planillas no los ejecuten como fórmulas. Los números nunca se modifican. Una opción del diálogo de exportación lo desactiva.
- **Descartar cambios en Proyectos:** un archivo con nombre de patrón (`*`) descarta solo ese archivo.
- **Copiar un subconjunto:** el enmascarado usa una clave aleatoria nueva de 256 bits en cada ejecución.
- **Actualización de drivers:** la app nunca acepta un índice de drivers más viejo que el de su versión, ni siquiera en una instalación nueva, ni uno que dejó de renovarse. Los drivers instalados siguen funcionando en ambos casos.
- La importación de conexiones, el linter y el chequeo de salud ya no se detienen con caracteres acentuados u otros de varios bytes.
- **Solo lectura en SQL Server:** cada consulta corre en una transacción que siempre se revierte. Los backups, las restauraciones, activar o desactivar triggers, las escrituras con punteros de texto, Service Broker y las sentencias de transacción se rechazan cuando vienen después de una lectura en el mismo lote.
- **Solo lectura en PostgreSQL:** se rechazan los nombres escritos con escapes Unicode (`U&"…"`), así que una función prohibida no puede llamarse con otra grafía.
- **Exportación CSV y TSV:** la protección contra fórmulas también se aplica al texto guardado en columnas declaradas como numéricas, algo que SQLite permite.
- **Scripts generados:** los nombres de objetos que vienen del servidor no pueden cerrar un comentario y ejecutarse como código. Esto cubre las correcciones sugeridas por el **Chequeo de salud** y los scripts de usuarios, backups y estructura. Los nombres de ClickHouse con una barra invertida se entrecomillan correctamente.
- **Solo lectura:** se rechaza una sentencia que no empieza con una palabra, como un nombre entre corchetes que ejecuta un procedimiento en SQL Server. También se revisan las palabras dentro de lecturas entre paréntesis.
- **Modificar tabla:** las advertencias del script que abrís como query quedan en su línea de comentario.
- **Las contraseñas y las opciones secretas** se enmascaran en todos los lugares donde se editan, incluidos los pasos de backup de las tareas programadas.
- **Ver dependencias y Renombrar** ya no se detienen en rutinas con nombres entre comillas poco comunes.
- **Solo lectura en SQL Server:** un batch solo puede empezar con `SELECT`, `WITH`, `USE` o `PRINT`. Lo que sigue a `SHOW`, `DESCRIBE` o `PRAGMA` se revisa en todos los motores.
- **Importar conexiones:** una URL JDBC de Oracle con usuario y contraseña los deja fuera del nombre de la conexión: la contraseña va al llavero del sistema.
- **Solo lectura en SQL Server:** un procedimiento cuyo nombre empieza como `print_` o `select1` ya no se toma por una lectura.
- **Importar conexiones:** una contraseña de Oracle con `@` se guarda entera en el llavero del sistema.
- **Túneles SSH en Linux:** el puerto local del túnel solo acepta conexiones abiertas de tu propio usuario.
- **Solo lectura:** se rechaza una consulta con un retorno de carro suelto o un espacio Unicode inusual, porque los motores no coinciden en dónde termina ahí un comentario o una sentencia. También se rechazan las funciones con efectos secundarios: `load_extension` en SQLite, `pg_notify` y las lecturas de replicación lógica en PostgreSQL, y la cancelación de sesiones o consultas en Snowflake.
- **Importar conexiones:** una URL de SQL Server sin host ya no pone su contraseña en el nombre de la conexión.
- **Usuarios y permisos:** la vista previa oculta las contraseñas de PostgreSQL en todas sus formas.
- **Solo lectura:** se rechazan las consultas que toman bloqueos por los que otras sesiones esperan, por ejemplo `pg_advisory_lock`, `GET_LOCK`, `LOCK IN SHARE MODE` y `FOR SHARE`.
- **Snowflake y Cassandra:** los comentarios `//` se leen como los lee el servidor.
- **Las conexiones de solo lectura a SQL Server** rechazan los hints de bloqueo y `WAITFOR`.
- **Las conexiones de solo lectura** rechazan avanzar una secuencia (`NEXT VALUE FOR`, `.NEXTVAL`).
- **Renombrar:**
  - En todos los motores se rechaza un dependiente cuyo código se partiría en más de una sentencia.
  - Los cuerpos de rutinas de Snowflake que contienen `$$` se mantienen enteros.
  - Se rechazan los nombres de OrientDB que DBine no podría entrecomillar de forma segura.
- **Exportación SQL:** los valores de texto se escriben como los lee el motor de origen, así que vuelven exactos al ejecutar el script ahí. Tampoco pueden agregar sentencias en un motor que lee las barras invertidas de otra manera.
  - **PostgreSQL, SQL Server, Oracle, SQLite, DuckDB, Firebird, SAP HANA, Trino, Db2, Teradata, Vertica, Exasol, Netezza y Dremio:** la barra invertida se escribe con la función de caracteres del propio motor, así que no aparece ninguna dentro de un texto. Los scripts de Firebird llevan una fila por `INSERT`, que es lo único que Firebird acepta.
  - **Motores que leen escapes con barra invertida** (MySQL, ClickHouse, Snowflake, Redshift…): las barras invertidas y las comillas se escapan como esos motores las leen.
  - **Orígenes desconocidos** (una grilla de varias bases de datos) reciben una forma segura en cualquier motor. La opción nueva **Destino con textos estándar** escribe sus barras invertidas tal cual.
- **Exportación CSV y TSV:** la protección contra fórmulas también cubre el texto después de una `,` o un `;`, que una planilla con otro separador de listas partiría en su propia celda.
- **Renombrar en Snowflake:** las vistas, funciones y procedimientos que nombran el objeto renombrado se listan para que los corrijas a mano en lugar de recrearlos, así conservan su dueño, `EXECUTE AS` y `SECURE`.

## [0.1.9] - 2026-10-09

### Nuevo
- **Renombrar con impacto:** «Renombrar…» en el explorador cambia el nombre de una tabla, vista, rutina, columna, índice o esquema y, en el mismo script, reescribe las vistas, procedimientos, funciones y triggers que lo usan. Antes de ejecutar muestra qué actualiza el motor solo, qué se reescribe y qué hay que revisar a mano (SQL dinámico, código ilegible), junto con el script completo. Corre en una transacción donde el motor lo permite. Está en todos los motores que pueden renombrar algo; los límites de cada uno están en `docs/engine-support.md`.
- **Modificar una tabla:** «Modificar…» abre el diseñador sobre una tabla existente y arma el `ALTER` del motor. Conserva lo que el diseñador no muestra (CHECKs, opciones de índices, orden de las columnas de la clave) y recrea las vistas y triggers que dependen de la tabla. Renombrar una columna ahí pasa por la revisión de impacto; en conexiones de producción pide escribir el nombre de la tabla antes de ejecutar.
- **Historial por consulta:** la barra de Historial sigue a la pestaña activa, como una línea de tiempo: versiones de la consulta guardada con diferencias y restauración, sus ejecuciones y, en archivos de un proyecto, sus commits de git.
- **Navegación en el editor:** Cmd/Ctrl+clic sobre una tabla, vista o rutina abre su estructura o definición, y «Mostrar en el explorador» la ubica en el árbol. Las tablas y columnas que no existen se marcan antes de ejecutar.
- **Parámetros en las consultas:** `:nombre` y `?` se piden al ejecutar y recuerdan el último valor.
- **Fragmentos de código** por motor (por ejemplo, `sel` + Tab) y menú del botón derecho en el editor.
- **Totales de la selección:** al seleccionar celdas de la grilla se muestran cantidad, suma, promedio, mínimo y máximo.
- **Tareas programadas:** scripts, exportaciones, comparación de esquemas, backups, «Documentar la base» y «Enviar un mail» (SMTP) que corren con DBine cerrado, a través del programador del sistema. Con notificaciones por tarea e historial de ejecuciones. Lo que cambia datos se aprueba explícitamente.
- **Calidad de código:** reglas por motor en el editor y «Ver problemas».
- **Documentar la base:** diccionario de datos en HTML o Markdown, con diagrama, filas estimadas y comentarios de vistas y rutinas. Las filas estimadas y los comentarios salen de la metadata del motor, sin leer tablas ni consumir cuota en los motores en la nube.
- **Constructor visual de consultas.**
- **Copiar un subconjunto de datos**, con enmascarado.
- **Optimizar consulta:** reescrituras, índices sugeridos, alternativas de la IA y comparación medida. Las alternativas de la IA se validan contra el plan estimado de la base antes de mostrarse.
- **Chequeo de salud** de una base, en todos los motores, con revisiones propias en SQL Server, la familia PostgreSQL, la familia MySQL, Oracle, SAP HANA, Firebird, ClickHouse, Snowflake, BigQuery, Databricks y los perfiles ODBC.
- **Buscar en la base:** nombres de objetos, código de vistas y rutinas, y nombres de columnas (con su tabla y tipo).
- **Generar datos de prueba** para una tabla.
- **Propiedades de la base** y opciones avanzadas al crear una base, en pestañas y por motor, con vista previa del script.
- **Vista JSON en árbol** de los resultados, con edición, y «Agregar fila» / «Agregar documento» en la pestaña Datos y en la grilla.
- **Nueva marca:** el logotipo con el halo.

### Mejoras
- El color de la conexión se ve como una franja en el borde de la fila, y el punto solo indica el estado (verde conectada, rojo desconectada).

### Correcciones
- La comparación de esquemas ya no se cancela por lecturas del explorador, y SQL Server se reconecta.
- Las filas de conexión sin color quedan alineadas con las que tienen.
- Arrastrar tablas al constructor de consultas funciona en macOS.
- Propiedades de SQL Server: los nombres de archivo largos no desbordan el diálogo, y la pestaña «Opciones ANSI y de seguridad» está traducida.
- El editor ya no marca como desconocidas las columnas de una subconsulta con alias.

### Ya disponible
- **Ejecutar una consulta en varias bases a la vez:** se elige una o varias bases de una conexión, y los resultados se unen con una columna que indica la base de cada fila. Llegó en la 0.1.4. Ver `docs/multi-database-queries.md`.

## [0.1.8] - 2026-10-06

### Nuevo
- **PostgreSQL detrás de gateways que solo aceptan el protocolo simple:** la conexión tiene una opción nueva, «Protocolo de consultas»: Automático o Solo protocolo simple. Sirve para los gateways que rechazan el protocolo extendido con el error 0A000. En ese modo, lo que necesita el protocolo extendido avisa con un mensaje claro en lugar de fallar.

### Correcciones
- **«Nueva query» con la pestaña «Nueva conexión» abierta** fallaba con «FOREIGN KEY constraint failed». Ahora la query se abre en la última conexión que tenías abierta, o te pide elegir una base en el explorador.

## [0.1.7] - 2026-10-05

### Correcciones
- **Comparar datos con columnas identity:** sincronizar filas hacia una tabla de SQL Server con una columna `IDENTITY` fallaba con «Cannot insert explicit value for identity column». Ahora DBine activa `IDENTITY_INSERT` solo mientras inserta esas filas.
- En PostgreSQL, después de copiar filas con sus ids, la secuencia avanza para que el próximo insert no choque con un id copiado.

## [0.1.6] - 2026-10-04

### Nuevo
- **Procesos en el Monitor:** junto al panel, la pestaña «Procesos» lista las sesiones y las consultas en curso del servidor, con filtros. Desde ahí se cancela una consulta o se termina una sesión. Disponible en todos los motores que lo exponen: SQL Server, PostgreSQL, MySQL, Oracle, MongoDB, Redis y la mayoría de los demás.
- **Autenticación de Windows en SQL Server:** con el usuario actual (SSPI en Windows, Kerberos en macOS y Linux) o con usuario y contraseña de dominio, también desde Mac y Linux.
- **Kerberos en MongoDB.**

### Mejoras
- En ODBC, los atributos extra de la conexión reemplazan a los de la plantilla.
- El asistente de IA tiene su propio ícono y ya no se confunde con «Formatear».
- Las consultas que hace el asistente se muestran traducidas en todos los idiomas.
- La telemetría anónima cuenta también el uso del asistente, el servidor MCP, las sincronizaciones, las migraciones y las consultas en varias bases. Nunca nombres, consultas ni datos; se desactiva en Configuración › General.

## [0.1.5] - 2026-10-03

### Nuevo
- **El asistente de IA lee tu base, con tu aprobación:** con un modelo local puede consultar la estructura y el uso de índices de la conexión (por ejemplo, «analizá los índices y decime cuál sobra»). Antes de leer filas o ejecutar una consulta te muestra el SQL exacto y la base, con Aprobar o Rechazar. Nunca modifica datos ni estructura.

### Mejoras
- «Detener» corta la respuesta del asistente en cualquier momento y «Nueva conversación» siempre está disponible.
- La opción «estructura» del chat ya no hace falta: el asistente pide los detalles cuando los necesita.
- El cursor de texto aparece donde se puede seleccionar o escribir.

## [0.1.4] - 2026-10-03

### Nuevo
- **Proyectos:** repositorios Git de SQL vinculados a tus conexiones, desde el segundo ícono de la barra lateral. Árbol de archivos, base activa o entornos (dev/qa/prod) sin credenciales en el repo, cambios con diff, commit, pull y push. Cada base muestra en el Explorador los proyectos vinculados.
- **Ejecutar una consulta en varias bases a la vez:** la misma consulta en varias bases de una conexión, con los resultados juntos y una columna que indica la base.
- **DBine se actualiza solo:** descarga la versión nueva, verifica su firma y se reinicia (pregunta antes si hay tareas en segundo plano). La 0.1.4 se instala a mano por última vez. En Linux funciona con la AppImage; con .deb/.rpm sigue ofreciendo la descarga.
- **Deshabilitar y habilitar índices** desde el explorador y la pestaña Índices, en los motores que lo permiten (SQL Server, MySQL, MariaDB, TiDB, Oracle, Firebird, CockroachDB, MongoDB…).
- **Selección de celdas en la grilla:** un bloque (arrastrando, Shift+clic o Shift+flechas) para copiarlo, o celdas y filas salteadas con Cmd/Ctrl+clic.
- **Comparación de esquemas:** puede eliminar un elemento a la izquierda, a la derecha o en ambos lados, y antes de ejecutar muestra qué depende de él.

### Mejoras
- **Asistente de IA:** recomienda un modelo integrado más grande según la memoria de tu equipo, conoce las particularidades de cada dialecto, reintenta si se niega a responder y guarda el historial de conversaciones en un panel.
- El panel de Tareas tiene «Quitar terminadas» arriba y se cierra con Escape o con un clic afuera.
- Azure SQL Database (también Hyperscale): conectado a master, lista todas las bases del servidor.
- CockroachDB: los índices se ven como BTREE y GIN, igual que en PostgreSQL.
- El texto del chat de IA se puede seleccionar y copiar.

### Correcciones
- La barra de la consulta ya no se desarma al abrir el panel de IA.
- La sincronización de esquemas de libSQL ya no falla por una sentencia `PRAGMA` que el servidor rechaza.

## [0.1.3] - 2026-10-02

### Nuevo
- **Varias ventanas** en la misma instancia: «Nueva ventana» desde el Dock, la barra de tareas, Archivo › Nueva ventana o Cmd/Ctrl+Shift+N. Conexiones, consultas guardadas y configuración se comparten entre ventanas.
- **Tareas en segundo plano:** las operaciones largas (sincronizar esquemas o datos, backups, generar scripts, importar, exportar, clonar tablas, eliminar objetos) pueden seguir en segundo plano. El panel de Tareas muestra progreso, tiempo transcurrido, tiempo restante estimado y Cancelar, y avisa al terminar. Al cerrar la aplicación con tareas en curso, pide confirmación antes de cancelarlas.
- **Uso de índices** en todos los motores que lo informan: claves PK y FK en las columnas, carpeta Índices, porcentaje de lecturas por índice con color según seeks y scans, y eliminar un índice desde el explorador.
- **«Ver dependencias…»:** qué depende de una tabla, columna, vista o rutina.

### Mejoras
- La sincronización de datos aplica cada lado en una sola transacción.
- Autocompletado de SQL después de «esquema.» y «tabla.».
- La comparación de esquemas sincroniza comentarios, tiene flechas reversibles y lista redimensionable.

### Correcciones
- La sincronización de esquemas elimina las claves foráneas duplicadas una por una, y en SQL Server cambia el índice clustered de una tabla de forma segura.

## [0.1.2] - 2026-10-01

### Nuevo
- **Ejecución de scripts como en la herramienta de cada motor:** sentencia por sentencia, con `GO` / `GO N`, `DELIMITER`, `/` y `SET TERM`. Opción «Seguir si hay un error», mensajes en vivo ordenados, errores con código y línea, y ejecución de la sentencia en el cursor.
- **Transacciones Auto/Manual** con Confirmar y Deshacer, y confirmación antes de un UPDATE o DELETE sin WHERE.
- **Esquemas:** crear y eliminar esquemas con propietario y permisos; los esquemas vacíos aparecen en el explorador.
- **Aviso de versión nueva:** DBine avisa cuando hay una versión nueva, al abrir y desde Ayuda › Buscar actualizaciones….
- Reordenar conexiones y carpetas arrastrando.
- Eliminar filas desde la grilla de datos y guardar los cambios con Cmd/Ctrl+S.

### Mejoras
- Cancelar una consulta mantiene la sesión.

### Correcciones
- El driver de Solr se republicó (comparte código con el de Elasticsearch).

## [0.1.1] - 2026-09-30

### Nuevo
- **Telemetría anónima**, activada por defecto, con un aviso la primera vez. Se desactiva en Configuración o con `DO_NOT_TRACK` / `DBINE_TELEMETRY=0`.
- PostgreSQL: opciones de identidad (columnas identity) al diseñar tablas.

### Mejoras
- Oracle: las definiciones incluyen los índices.
- Pestaña de definición completa, con mensajes de error de migración más claros.
- La comparación de esquemas conserva las filas y sincroniza con un solo paso.
- La pestaña de comparación de datos recuerda tus selecciones.
- Drivers de PostgreSQL y Oracle actualizados a 0.1.2.

## [0.1.0] - 2026-09-30

### Nuevo
- Primera versión de DBine, con instaladores para Windows, macOS (Apple Silicon e Intel) y Linux. Los drivers de cada motor, salvo SQLite, se descargan la primera vez que te conectás.
