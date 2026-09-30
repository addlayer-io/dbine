# Túneles SSH

Una conexión puede llegar a su servidor a través de un servidor SSH (un
bastión), y opcionalmente pasando antes por otros bastiones. Sirve para bases
que solo se alcanzan desde adentro de una red.

## Cómo se usa

En el formulario de la conexión, el panel **Túnel SSH**:

- **Servidor SSH**, **puerto** (22 si se deja vacío) y **usuario SSH**.
- **Autenticación**:
  - **Contraseña**.
  - **Clave privada**: el archivo (OpenSSH o PEM; `~` vale) y su frase, si
    tiene.
  - **Agente SSH**: las claves cargadas en el agente (`ssh-agent`, 1Password,
    el agente de OpenSSH de Windows).
- **Bastiones intermedios** (opcional): servidores SSH por los que hay que
  pasar antes, en orden, como `usuario@bastion1:22, bastion2`. Si no dicen
  usuario o puerto, se usan el usuario SSH y el 22. Usan la misma
  autenticación.

El servidor de la base y su puerto se escriben como siempre, como los ve el
último servidor SSH (por ejemplo, `db.interno:5432`).

El panel aparece en los motores que se conectan por red. No aparece en los
que usan un archivo local (SQLite, DuckDB, etc.).

## Verificación del servidor

La primera vez que DBine se conecta a un servidor SSH muestra la huella de su
clave (`SHA256:…`) y pregunta si confiar en él. Hay que compararla con la
que dé el administrador del servidor. La huella aceptada se guarda en la
conexión, y en el formulario se ve «N servidores SSH verificados», con la
opción de olvidarlos.

- Un servidor que ya está en `~/.ssh/known_hosts` se acepta sin preguntar.
- Si la clave de un servidor no coincide con la de `known_hosts`, la conexión
  se rechaza: puede ser otro servidor haciéndose pasar por él.
- Con bastiones, se verifica cada servidor por separado.

## Qué se guarda y dónde

- La configuración va en las opciones de la conexión (`ssh.enabled`,
  `ssh.host`, `ssh.port`, `ssh.user`, `ssh.auth`, `ssh.key_path`,
  `ssh.jump`, `ssh.trusted`).
- La contraseña SSH y la frase de la clave van al llavero del sistema, nunca
  al archivo de estado ni a los logs. Se guardan aunque la conexión no guarde
  la contraseña de la base: la opción «Guardar contraseña» se refiere a la
  base.
- Al importar conexiones de otras herramientas, el túnel SSH viene con
  ellas, con su contraseña o frase si la herramienta la guardaba.

## Cómo funciona

- DBine abre un puerto local (en `127.0.0.1`, elegido por el sistema) y lo
  reenvía por SSH al servidor de la base (`crates/dbine-tunnel`, con
  `russh`).
- El driver se conecta a ese puerto local como si fuera el servidor, así que
  cualquier motor que se conecte por red funciona con túnel sin saber nada de
  SSH. No hay cambios en los drivers.
- **Un túnel por conexión:** todas las sesiones de la conexión (explorador,
  pestañas, exportaciones, Profiler) usan el mismo túnel.
  - Se abre la primera vez que hace falta.
  - Si la sesión SSH se corta, se vuelve a abrir en la conexión siguiente.
  - Se cierra al desconectar o al editar la conexión.
- Se mandan keepalives cada 30 s para que un firewall no corte el túnel
  inactivo.

## Particularidades

- **TLS:** con túnel, el driver se conecta a `127.0.0.1`. Si la base exige
  TLS y verifica que el certificado corresponda al nombre del servidor, esa
  verificación falla. En ese caso hay que activar «Confiar en el certificado
  del servidor» o, si el driver lo permite, indicar el nombre esperado.
- **Direcciones en URL:** en los motores que usan una URL como servidor (por
  ejemplo, `https://es.interno:9200`), el túnel reemplaza solo el servidor y
  el puerto de la URL.
- Los motores que se conectan a servicios en la nube por HTTPS con un nombre
  fijo (BigQuery, Snowflake, Cosmos DB, DynamoDB…) no suelen necesitar
  túnel. Si se configura uno, pasan las mismas consideraciones de TLS.
