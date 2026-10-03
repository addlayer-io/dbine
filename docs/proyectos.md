# Proyectos

Un proyecto es una carpeta de esta máquina con un repositorio git de
scripts: consultas, migraciones, reportes. DBine muestra sus archivos, los
abre en el editor, los ejecuta sobre la base que elijas y trabaja con git
(cambios, commit, pull, push) sin salir de la aplicación. Se abren desde la
barra de actividad, con el botón **Proyectos**, el segundo después del
Explorador.

La conexión sigue siendo el centro: un proyecto no tiene conexiones propias,
solo apunta a una de las tuyas.

## Diferencia con las queries y la Biblioteca

| | Queries | Biblioteca | Proyectos |
|---|---|---|---|
| Dónde vive el texto | en el estado local de DBine | en el estado local de DBine | en archivos de una carpeta |
| Se guarda | al escribir | al escribir | con ⌘S |
| Historial | no | opcional, en un repositorio git propio | el del repositorio |
| Para | lo que se trabaja en una base | scripts de un motor para cualquier base | el código que el equipo versiona y revisa |

## Vincular un proyecto

- **Vincular proyecto…** (el **+** de la vista): elegí una carpeta.
  - Si es un repositorio git, se vincula su raíz. Si elegiste una subcarpeta,
    DBine avisa que va a vincular la raíz del repositorio.
  - Si no es un repositorio, se puede inicializar uno ahí mismo.
  - La misma carpeta no se puede vincular dos veces.
- **Clonar repositorio…**: la URL del remoto, la carpeta donde clonarlo (por
  defecto `DBine/Proyectos` en tu carpeta personal), y opcionalmente un nombre
  y una rama. La clonación corre como tarea en segundo plano y se puede
  cancelar mientras descarga.
- Desde el Explorador: clic derecho en el nodo **Proyectos** de una base ›
  **Vincular proyecto…**. Esa base pasa a ser la base del proyecto.

**Desvincular** solo hace que DBine deje de mostrarlo: la carpeta y sus
archivos no se tocan. Si la carpeta se movió, **Reubicar…** la vuelve a
encontrar.

Los proyectos son de esta máquina: no se sincronizan con la nube ni entran en
el backup, porque sus rutas solo tienen sentido aquí.

## La base activa

Cada proyecto tiene una **base activa**: la conexión y la base donde se
ejecutan sus archivos. Se elige en la fila "Base activa" del proyecto.

Las pestañas de archivos del proyecto siguen a la base activa: si la cambiás,
todas pasan a la nueva. Hay dos excepciones:

- Una pestaña en la que elegiste otra base en su propio selector deja de
  seguir al proyecto. El enlace **Base del proyecto** de su encabezado la
  devuelve.
- Una pestaña que está ejecutando, o que tiene una transacción abierta, se
  queda en la base anterior hasta terminar, y DBine lo avisa.

Si la conexión elegida se eliminó, el proyecto muestra "Conexión no
disponible" y sus archivos no se pueden ejecutar hasta elegir otra.

### Desde el Explorador

Cada base del Explorador tiene un nodo **Proyectos** con los proyectos que
la usan, con su rama y la cantidad de cambios sin confirmar. Un clic en un
proyecto lo abre en la vista Proyectos y hace que esa base sea su base activa
(si el proyecto tiene entornos, activa el entorno que apunta a esa base, o
pregunta a cuál asignarla).

## Entornos y `.dbine.json`

Un repositorio puede declarar entornos (por ejemplo `dev`, `qa` y `prod`) en
un archivo `.dbine.json` en su raíz. Se comparte con el equipo como cualquier
otro archivo del repositorio:

```json
{
  "version": 1,
  "name": "Ventas",
  "engine": "postgres",
  "environments": [
    { "name": "dev" },
    { "name": "qa" },
    { "name": "prod", "confirm_run": true, "description": "Producción" }
  ],
  "default_environment": "dev"
}
```

| Campo | Qué es |
|---|---|
| `name` | Nombre sugerido para el proyecto. |
| `engine` | El motor de los scripts (el id del driver). Al elegir una base, se proponen las conexiones de ese motor. |
| `environments[].name` | Nombre del entorno: letras, números, `_`, `.` o `-`, hasta 40. No se repite. |
| `environments[].engine` | El motor de ese entorno, si difiere del de la raíz. |
| `environments[].confirm_run` | Pide confirmación antes de cada ejecución en ese entorno. |
| `environments[].description` | Un texto para reconocerlo. |
| `default_environment` | El entorno activo al vincular el proyecto. |

El archivo **solo tiene nombres**. Cada persona decide, en su máquina, qué
conexión y base usa para cada entorno; esa elección no va al repositorio. Si
el archivo trae campos que parecen credenciales (`password`, `user`, `host`,
`url`, `connection_string`…), DBine los ignora y lo avisa.

Con entornos, la fila "Base activa" muestra cada uno con su base (o "Sin
asignar"). Un clic activa un entorno; el lápiz cambia su base. El encabezado
de cada archivo muestra el entorno en uso, y en uno con `confirm_run` cada
ejecución pregunta primero: "Vas a ejecutar en «prod» (conexión › base)".

**Definir entornos…** (menú del proyecto) edita el archivo desde DBine. Si el
archivo no es JSON válido, el proyecto usa una base directa y muestra el error.
Si el entorno activo se borró del archivo, DBine pide elegir otro.

## Archivos

La sección **Archivos** muestra la carpeta como un árbol que carga cada nivel
al abrirlo:

- Marcas de git: **M** modificado, **A**/**U** nuevo, **D** eliminado (el
  archivo se sigue mostrando tachado), **!** en conflicto. Una carpeta con
  cambios adentro muestra un punto. Lo que `.gitignore` excluye se ve atenuado.
- Un clic abre el archivo en una pestaña de vista previa; doble clic la fija.
- Menú contextual: Nuevo archivo…, Nueva carpeta…, Renombrar… (también F2 o
  Enter), Eliminar…, Ver cambios, Descartar cambios…, Mostrar en
  Finder/Explorador y Copiar ruta relativa.

Los archivos de script (`.sql`, `.cql`, `.js`, `.json`, `.txt`, `.redis`,
`.cypher`, `.flux`, `.ksql`, `.n1ql`, `.psql`) se abren en el editor de
consultas, con todo lo de siempre: ejecutar, planes, formatear, transacciones,
"Ejecutar en varias bases…" y el asistente de IA. Si la extensión no es la del
motor de la base (un `.js` en PostgreSQL), aparece un aviso, pero se puede
ejecutar igual. Otros archivos de texto (un `README.md`, un `.yml`) se editan
sin el botón Ejecutar. Los archivos binarios o de más de 5 MB no se abren.

## Guardar

A diferencia de las queries, un archivo **no se guarda solo**: se guarda con
⌘S o con el botón **Guardar** del encabezado. Mientras tiene cambios, su
pestaña muestra un punto.

- Al guardar se mantienen los finales de línea (LF o CRLF) y el BOM del archivo.
- Si el archivo cambió en el disco desde que lo abriste (otro editor, un
  `git pull` en una terminal, otra ventana de DBine), DBine no lo pisa: muestra
  "El archivo cambió fuera de DBine" con **Recargar** (descarta tus cambios) o
  **Sobrescribir** (guarda los tuyos).
- Una pestaña sin cambios propios se recarga sola cuando el archivo cambia en
  el disco.
- Si el archivo se borró, la pestaña lo avisa, con **Guardar de nuevo** o
  **Cerrar**.
- Cerrar una pestaña con cambios pregunta: Guardar, No guardar o Cancelar.
  Lo mismo al cerrar la ventana o salir de DBine, para todos los archivos sin
  guardar de todas las ventanas.

## Cambios y git

La sección **Cambios (N)** lista lo que cambió desde el último commit. Un clic
en un archivo abre sus cambios lado a lado (el último commit a la izquierda,
el archivo actual a la derecha). Cada fila tiene "Abrir archivo" y "Descartar
cambios".

- **Confirmar**: escribí el mensaje (⌘↵ también confirma). Se incluyen todos
  los cambios, también los archivos nuevos y eliminados. Si git todavía no
  tiene tu nombre y email, DBine los pide una vez.
- **Pull**: trae los cambios del remoto. Respeta tu configuración de git
  (merge o rebase). Si tenés archivos del proyecto sin guardar, primero
  ofrece guardarlos.
- **Push**: sube tus commits. Si la rama todavía no existe en el remoto, la
  crea.
- **Sincronizar**: Pull y, si sale bien, Push.
- **Fetch** (menú del proyecto) y **Actualizar** (encabezado de la vista)
  consultan el remoto sin cambiar nada.

Pull, Push, Sincronizar, Fetch y Clonar corren como tareas en segundo plano
(el panel de Tareas las muestra) y se pueden cancelar mientras hablan con el
remoto. Hay una sola operación de git a la vez por proyecto.

La fila del proyecto muestra la rama, los commits por subir (↑) y por traer
(↓), y la cantidad de cambios sin confirmar. El botón Proyectos de la barra de
actividad muestra el total de cambios de todos los proyectos.

### Conflictos

Si un Pull choca con tus cambios, DBine **deja el repositorio a mitad del
merge o del rebase** (son tus archivos: nada se descarta solo) y muestra la
sección **Conflictos** con cada archivo:

- **Abrir**: el archivo con las marcas de conflicto, para editarlo a mano.
- **Usar la mía** o **Usar la del remoto**: se queda con una de las dos
  versiones.
- **Marcar resuelto**: después de editarlo.

Con todo resuelto, **Continuar** termina el merge o el rebase; **Abortar**
vuelve el repositorio a como estaba antes. Una operación que quedó a medias
fuera de DBine se detecta igual.

### Credenciales

Git usa tus propias credenciales: el credential helper que tengas configurado
o tu agente SSH. DBine nunca las pide ni las guarda. Si el remoto rechaza el
acceso, el mensaje dice qué falta configurar. Si el servidor SSH todavía no es
de confianza, hay que conectarse una vez desde una terminal.

## Cuando algo falta

| Situación | Qué muestra DBine |
|---|---|
| git no está instalado | Los archivos funcionan; las secciones de git piden instalarlo (git-scm.com). |
| La carpeta no está | "No se encuentra la carpeta", con Reubicar… y Desvincular. |
| La carpeta perdió su `.git` | Inicializar o Desvincular. |
| HEAD desacoplado | La rama dice "HEAD desacoplado (abc1234)"; Confirmar, Pull y Push quedan desactivados. |
| Sin remoto | Pull, Push y Sincronizar desactivados; "Agregar remoto…" en el menú. |

## Seguridad

- Todas las operaciones con archivos pasan por DBine, que verifica que la ruta
  quede dentro de la carpeta del proyecto: no se puede leer ni escribir fuera
  de ella, ni dentro de `.git`.
- Los enlaces simbólicos no se siguen fuera de la carpeta, y borrar uno borra
  el enlace, no su destino.
- El repositorio nunca tiene contraseñas: `.dbine.json` solo lleva nombres, y
  la relación entre entornos y conexiones queda en esta máquina.

## Límites

- No hay cambio ni creación de ramas, staging parcial, historial ni blame;
  para eso, git desde una terminal sigue funcionando junto a DBine.
- Se muestran hasta 5000 cambios.
- No se abren archivos de más de 5 MB; no se muestran diferencias de archivos
  binarios o de más de 2 MB.
