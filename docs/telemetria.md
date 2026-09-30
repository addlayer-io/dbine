# Telemetría

DBine envía datos de uso **anónimos**. Sirven para saber cuánto se usa la
herramienta, qué motores de base de datos conviene priorizar y en qué sistemas
operativos corre.

## Cómo se desactiva

- Viene **activada por defecto**. En el primer arranque, un aviso explica qué
  se envía y qué no, y cómo desactivarla.
- Se desactiva en **Configuración › General › Compartir datos de uso
  anónimos**. Desde ese momento no se envía nada más.
- Es una preferencia más: viaja con la sincronización a las otras máquinas del
  mismo usuario.
- Para desactivarla en toda una máquina (instalaciones administradas), basta
  con la variable de entorno `DO_NOT_TRACK=1` o `DBINE_TELEMETRY=0`. Con
  cualquiera de las dos no sale ningún evento, diga lo que diga la
  configuración.
- Quien la había rechazado en una versión anterior (cuando la app preguntaba)
  la sigue teniendo desactivada.

## Qué se envía

Tres eventos, nada más:

| Evento | Cuándo | Datos propios |
|---|---|---|
| `app_started` | una vez por ejecución de la app | — |
| `connection_opened` | la primera vez que se conecta a cada motor en una ejecución | `engine`: el id del driver (`postgres`, `sqlserver`, `redis`…) |
| `module_opened` | la primera vez que se muestra cada módulo en una ejecución | `module`: el tipo de pestaña (`query`, `object`, `designer`, `diagram`, `monitor`, `profiler`, `migration`, `connection`, `compare`, `dataCompare`, `security`, `backups`) |

Con `connection_opened` y `module_opened` se ve qué motores y qué módulos se
usan de verdad, y cuáles no aparecen nunca.

Cada evento lleva además:

- la versión de DBine;
- el sistema operativo y su versión (`macOS 26.0`, `Windows 10.0.26100`,
  `ubuntu 24.04`);
- el idioma de la app;
- si es una compilación de desarrollo (esos eventos se ven aparte y no se
  mezclan con los de la app publicada);
- un id de sesión al azar, que cambia en cada ejecución y después de 4 horas sin
  uso.

El país lo deduce el servicio a partir de la conexión; **la IP no se guarda**.

## Qué no se envía nunca

- Nombres de conexiones, servidores, puertos, usuarios, bases, tablas ni
  schemas.
- Consultas, resultados ni ningún dato de las bases.
- Contraseñas ni secretos.
- Ningún identificador del usuario, de la máquina ni de la instalación.

La lista está fijada en el código de la app, no en la interfaz:
`src-tauri/src/commands/telemetry.rs` descarta cualquier evento que no sea uno
de los tres de arriba, cualquier motor que no sea un driver conocido y
cualquier módulo que no esté en su lista.

## Dónde se guardan

En [Aptabase](https://aptabase.com), un servicio de analítica de código abierto
pensado para la privacidad, en su región de Estados Unidos. Si no hay conexión
a internet, el evento se descarta: no se guarda para después.
