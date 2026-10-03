# Varias ventanas

DBine puede tener varias ventanas abiertas a la vez. Todas son la misma
instancia de la app: comparten las conexiones, las queries guardadas, la
configuración, la biblioteca y el servidor MCP. Lo que se cambia en una
ventana aparece en las demás.

## Dónde está "Nueva ventana"

| Sistema | Cómo se abre |
|---|---|
| macOS | Clic derecho (o mantener apretado) en el ícono de DBine en el Dock › **Nueva ventana**. También en el menú **Archivo › Nueva ventana** o con ⌘⇧N. |
| Windows | Clic derecho en el ícono de DBine en la barra de tareas › **Nueva ventana**. También con Ctrl+Shift+N, o abriendo DBine otra vez desde el menú Inicio o un acceso directo. |
| Linux | Clic derecho en el ícono del lanzador › **Nueva ventana** (en los escritorios que muestran las acciones del lanzador, con los paquetes `.deb` y `.rpm`). También con Ctrl+Shift+N, o abriendo DBine otra vez. |

En macOS, el menú del Dock también lista las ventanas abiertas, y un clic en
el ícono con todas las ventanas minimizadas trae de vuelta la última que se
usó.

## Qué guarda cada ventana

- **La primera ventana conserva tus pestañas.** Es la que restaura las
  pestañas y la conversación del asistente de IA al abrir DBine, y la que las
  guarda.
- **Las ventanas nuevas arrancan vacías,** sin pestañas ni conversación. Lo
  que se abre en ellas no se restaura la próxima vez.
- Si se cierra la primera ventana y quedan otras, la última que se usó pasa a
  ser la principal: desde ese momento sus pestañas son las que se guardan.
- Cada ventana recuerda su posición y su tamaño.

## Cerrar una ventana o salir

- **Cerrar una ventana** (el botón de cerrar) cierra solo esa ventana. Si
  tiene tareas corriendo en segundo plano (una exportación, una
  sincronización…), pregunta solo por esas antes de cancelarlas. Las tareas de
  las otras ventanas siguen.
- **Cerrar la última ventana** cierra DBine, también en macOS.
- **Salir** (⌘Q, **Salir** en el menú o en el Dock, o el cierre del sistema)
  cierra todas las ventanas. Si hay tareas corriendo, pregunta una sola vez con
  las de todas las ventanas.

## Actualizaciones

- **La búsqueda al abrir DBine** la hace una sola ventana: la primera que
  arranca.
- **El aviso de una versión nueva aparece en una sola ventana:** la que la
  encontró, o la ventana donde se eligió **Ayuda › Buscar actualizaciones…**.
  Si se busca desde otra ventana mientras se descarga, avisa en qué ventana
  está la descarga.
- **Reiniciar para terminar** pregunta una sola vez por las tareas de todas
  las ventanas, como Salir. Detalle: [`actualizaciones.md`](actualizaciones.md).
