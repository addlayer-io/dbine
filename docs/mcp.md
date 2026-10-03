# Servidor MCP

DBine puede actuar como servidor MCP (Model Context Protocol) local. Con él,
asistentes como Claude Code o Codex pueden ver tus conexiones, recorrer la
estructura de las bases y, donde lo habilites, hacer consultas de solo
lectura o, aprobando cada uno, cambios. Funciona solo mientras DBine está
abierto.

## Cómo se activa

1. Abrí **Configuración › MCP** y prendé **Servidor MCP**. Viene apagado.
2. Revisá el **puerto** (por defecto `27517`). Si otro programa lo usa, DBine
   lo avisa ahí mismo: elegí otro y guardalo. DBine no cambia de puerto por su
   cuenta.
3. En **Clientes**, creá uno por asistente ("Claude Code", "Codex"…). DBine
   muestra su token **una sola vez**, junto con la configuración lista para
   pegar.

## Niveles de acceso

Cada conexión tiene un nivel, que decide qué puede hacer un cliente con ella:

| Nivel | Qué permite |
|---|---|
| Deshabilitado | Nada: los clientes no ven la conexión. |
| Esquema | Listar bases y objetos, y ver columnas, claves, índices y claves foráneas. Ningún dato. |
| Lectura | Además: filas de muestra (hasta 100), consultas de solo lectura (hasta 500 filas, 30 s por defecto) y planes de ejecución estimados. |
| Escritura | Además: cambios en datos y estructura con `execute`. Cada uno se aprueba en DBine antes de ejecutarse (ver [Aprobaciones](#aprobaciones)). |

El **nivel predeterminado** se elige en Configuración › MCP y vale para las
conexiones sin nivel propio. De fábrica es **Esquema**. En el formulario de
cada conexión, **Acceso por MCP** permite usar el predeterminado o fijar otro.

Dos topes se aplican siempre, sea cual sea el nivel elegido:

- una conexión con la etiqueta `prod` (en mayúsculas o minúsculas) nunca pasa
  de **Lectura**;
- una conexión configurada como de solo lectura nunca pasa de **Lectura**.

## Herramientas

| Herramienta | Nivel | Qué hace |
|---|---|---|
| `list_connections` | Esquema | Nombre, motor y nivel de cada conexión visible. Nunca muestra hosts, usuarios ni contraseñas. |
| `list_databases` | Esquema | Las bases de una conexión. |
| `list_objects` | Esquema | Tablas, vistas, colecciones y demás objetos de una base. |
| `describe_object` | Esquema | Columnas, clave primaria, claves foráneas e índices. |
| `index_usage` | Esquema | Los índices de una tabla y cuánto se usan: lecturas, escrituras, porcentaje de las lecturas, sin uso y deshabilitados. |
| `sample_rows` | Lectura | Las primeras filas de una tabla o colección. |
| `run_query` | Lectura | Una consulta de solo lectura en el lenguaje del motor. |
| `explain` | Lectura | El plan estimado de una consulta, en los motores que tienen planes. |
| `execute` | Escritura | Código que cambia datos o estructura, en el lenguaje del motor. Espera la aprobación del usuario. |

Las consultas corren siempre en una sesión de solo lectura propia del
servidor MCP. En los motores SQL, una sentencia que modifica datos o
estructura se rechaza antes de llegar al servidor; los demás motores usan su
propio modo de solo lectura. Eso vale también en las conexiones con nivel
**Escritura**: para cambiar algo, el asistente tiene que usar `execute`.

## Aprobaciones

Cada vez que un asistente pide `execute`, DBine abre una ventana (aunque
esté minimizado u oculto) con el cliente, la conexión, la base y el código
exacto, y no ejecuta nada hasta que respondas:

- **Aprobar**: se ejecuta solo ese pedido, en una sesión propia del servidor
  MCP y con el mismo límite de tiempo que las consultas (30 s por defecto).
  El asistente recibe «aprobado y ejecutado» con las filas afectadas o las
  que devolvió.
- **Rechazar**: no se ejecuta; el asistente recibe «rechazado por el
  usuario».
- **Aprobar todo**: se ejecuta este pedido y los siguientes de ese cliente,
  sin preguntar, hasta que cierres DBine, lo quites o revoques el cliente. El
  riesgo es tuyo. Mientras está activo, Configuración › MCP lo muestra junto
  al cliente («Aprueba todo hasta cerrar DBine») con el botón **Quitar**.

Si no respondés en **2 minutos**, el pedido se rechaza y el asistente recibe
«sin respuesta: rechazado». Si llegan varios a la vez, se muestran de a uno,
con la cantidad que queda esperando. Cada pedido y su resultado (aprobado,
rechazado o sin respuesta) quedan en la actividad.

## Conectar Claude Code

Con `--scope user` queda disponible en todos los proyectos, no solo en la
carpeta desde donde se corre el comando.

```sh
claude mcp add --scope user --transport http dbine http://127.0.0.1:27517/mcp --header "Authorization: Bearer <token>"
```

## Conectar Codex

En `~/.codex/config.toml`:

```toml
[mcp_servers.dbine]
url = "http://127.0.0.1:27517/mcp"
http_headers = { "Authorization" = "Bearer <token>" }
```

## Cursor

En `~/.cursor/mcp.json` (o `.cursor/mcp.json` dentro de un proyecto):

```json
{
  "mcpServers": {
    "dbine": {
      "url": "http://127.0.0.1:27517/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

## Claude Desktop

Claude Desktop solo arranca servidores locales por comando, así que usa
`mcp-remote` como puente (necesita Node). En
`~/Library/Application Support/Claude/claude_desktop_config.json` (en Windows,
`%APPDATA%\Claude\claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "dbine": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:27517/mcp", "--header", "Authorization:${DBINE_AUTH}"],
      "env": { "DBINE_AUTH": "Bearer <token>" }
    }
  }
}
```

Después, reiniciar Claude Desktop.

## VS Code (Copilot)

```sh
code --add-mcp '{"name":"dbine","type":"http","url":"http://127.0.0.1:27517/mcp","headers":{"Authorization":"Bearer <token>"}}'
```

## Windsurf

En `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "dbine": {
      "serverUrl": "http://127.0.0.1:27517/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

## ChatGPT

No se puede conectar. ChatGPT llama a los servidores MCP desde los servidores
de OpenAI, así que necesita una URL pública con HTTPS, y el de DBine escucha
solo en esta máquina (`127.0.0.1`) a propósito. Exponerlo a internet con un
túnel dejaría las bases al alcance de cualquiera que tenga la URL y el token:
no lo recomendamos. Para usar un modelo de OpenAI con DBine, usar Codex
(arriba).

## Otros clientes

Los que aceptan un bloque `mcpServers` con servidores HTTP:

```json
{
  "mcpServers": {
    "dbine": {
      "type": "http",
      "url": "http://127.0.0.1:27517/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

En todos los casos, `<token>` es el que DBine muestra una sola vez al crear el
cliente, y el puerto es el que figura en Configuración → Servidor MCP.

## Actividad

Cada llamada queda registrada en esta máquina con la hora, el cliente, la
conexión, la herramienta, un resumen (la consulta, recortada) y el resultado
con la cantidad de filas. Se guardan las últimas 10.000 y se ven en
Configuración › MCP, filtrando por cliente o por conexión.

## Seguridad

- **Solo local.** El servidor escucha en `127.0.0.1`, nunca en la red. Rechaza
  los pedidos que vienen de una página del navegador (encabezado `Origin`) y
  los que llegan con otro nombre de host.
- **Token por cliente.** Cada pedido lleva `Authorization: Bearer <token>`.
  Cada cliente se revoca por separado y deja de funcionar en el acto.
- **Solo la huella.** DBine guarda el SHA-256 del token, nunca el token. Si lo
  perdés, revocá el cliente y creá otro.
- **Topes.** Las conexiones `prod` y las de solo lectura nunca pasan de
  Lectura, y cada escritura necesita tu aprobación en DBine.
- **Sin credenciales.** Ninguna respuesta ni el registro de actividad
  incluyen contraseñas, tokens ni otros secretos, y `list_connections` no
  muestra hosts ni usuarios.
- **Los resultados de las consultas le llegan al modelo.** Lo que devuelven
  `sample_rows` y `run_query` se envía al asistente y, según cómo funcione, al
  proveedor del modelo. Habilitá **Lectura** solo donde eso esté bien.
- Todo esto es de esta máquina: la configuración del servidor, los clientes y
  la actividad no viajan en la copia en la nube. El nivel de cada conexión sí,
  con el resto de la conexión.
