# Cambios

## [0.1.10] - 2026-10-09

### Nuevo
- **Los drivers se actualizan solos, aparte de la app:** DBine busca en un índice firmado el driver más nuevo compatible con tu versión, lo descarga en segundo plano y vuelve al anterior si algo falla. En Configuración › Drivers hay un botón **Buscar actualizaciones**, el estado de cada driver y **Volver a la anterior**. Un driver puede publicarse solo, sin una versión nueva de la app.
- **Renombrar una base:** **Renombrar…** sobre una base en el explorador. El diálogo muestra las otras sesiones abiertas sobre ella (que el renombrado cierra), si el nombre nuevo ya existe, cuántos objetos se mueven y el script completo. Después, la base predeterminada de la conexión, las pestañas abiertas, las consultas guardadas, las migraciones, los destinos de proyectos y los pasos de tareas programadas siguen el nombre nuevo; las tareas que modifican datos en ella piden aprobación de nuevo. Disponible en SQL Server, Azure SQL, Babelfish, la familia PostgreSQL, MySQL, MariaDB, Snowflake y MongoDB; donde el motor no puede renombrar (o mover) una base, no se ofrece.
- **¿A qué columna corresponde este valor?** En un `INSERT … VALUES`, al poner el cursor sobre un valor aparece un tooltip con su columna (por ejemplo "Columna 14 de 48: Name") y se resalta esa columna en la lista. Sin lista de columnas, usa las de la tabla en orden. Funciona en todos los motores SQL y CQL.
- **Más formas de entrar a SQL Server con Microsoft Entra ID:** interactiva con MFA (en el navegador), integrada (la cuenta de Windows, a través del ADFS federado de la organización), identidad administrada y predeterminada (variables de entorno, identidad administrada, Azure CLI o Azure Developer CLI). El token se conserva mientras DBine está abierto y se renueva antes de que venza.

### Mejoras
- **Qué trae cada versión:** el aviso de versión nueva muestra sus cambios y los de las versiones intermedias, a partir de la que tenés instalada, en el idioma de la app.
- **Versiones viejas de la app:** de ahora en más, una app anterior a las últimas cinco versiones tiene que actualizarse para descargar drivers nuevos. Los drivers que ya tiene instalados siguen funcionando.

### Correcciones
- **El solo lectura es más estricto:** las consultas de los clientes MCP con nivel Lectura, del asistente de IA y de las conexiones de solo lectura se revisan palabra por palabra, no solo por la primera. Ahora rechazan:
  - una escritura escondida después de una lectura en un lote de SQL Server;
  - un `WITH` que modifica datos;
  - `SELECT … INTO`, `EXEC` y `SET`;
  - funciones que actúan fuera de la consulta, como `set_config`, `dblink_exec` y `xp_cmdshell`.

  Un plan estimado rechaza los scripts que apagarían el modo plan, así que las alternativas de IA de **Optimizar consulta** no pueden ejecutar nada. Las tareas programadas cuyos scripts ahora cuentan como escritura piden aprobación de nuevo.
- **Biblioteca con git:** un repositorio compartido ya no puede hacer que DBine lea, escriba o borre archivos fuera de la carpeta de la Biblioteca.
- **Exportación SQL:** los valores de texto se escapan como los lee el motor de origen, así que un valor guardado no puede agregar sentencias a un script `INSERT` para MySQL, ClickHouse, BigQuery, Hive, Spark o Databricks.
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
- **Exportación SQL:** desde MySQL, ClickHouse y otros motores que leen las barras invertidas, las comillas se escriben como `''`, así que el script se lee igual en cualquier destino.
- **Modificar tabla:** las advertencias del script que abrís como query quedan en su línea de comentario.
- **Las contraseñas y las opciones secretas** se enmascaran en todos los lugares donde se editan, incluidos los pasos de backup de las tareas programadas.
- **Ver dependencias y Renombrar** ya no se detienen en rutinas con nombres entre comillas poco comunes.

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
