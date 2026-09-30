# Biblioteca de scripts

Scripts reutilizables, como "Reindexar una tabla", "Sesiones bloqueantes" o
"Espacio por tabla". No pertenecen a una base: son de un **motor**. Se abren
con la ⭐ de la barra de actividad.

## Diferencia con las queries

| | Queries | Biblioteca |
|---|---|---|
| Pertenecen a | una conexión y una base (nodo "Queries" del explorador) | uno o más motores |
| Al abrirlas | se abre esa misma query | se crea una **copia** en una query nueva de la base elegida; la biblioteca no cambia |
| Para | lo que se trabaja en esa base | lo que un DBA usa en cualquier base de ese motor |

## Motores

Cada script declara para qué motores es:

- uno o más drivers (SQL Server, PostgreSQL, MongoDB…);
- o **"Cualquier motor SQL"** (`*sql`).

Con una pestaña activa, la vista muestra primero los scripts que sirven para su
motor. Los demás quedan en "Otros motores", de modo que un script de MongoDB no
se mezcla con uno de SQL Server.

Un script también sirve para los motores que comparten dialecto: uno escrito
para PostgreSQL aparece en CockroachDB o YugabyteDB. Esto no aplica al dialecto
genérico `standard`.

## Parámetros

`{{nombre}}` marca un valor que se pide al abrir el script. Por ejemplo, en
`ALTER INDEX ALL ON {{tabla}} REBUILD;` DBine pregunta la tabla y sugiere las
de la base de destino. Se reemplaza el texto tal cual, sin escaparlo.

## Uso

- **Abrir:** doble clic en el script (o seleccionarlo y Enter; un clic solo lo selecciona). Se abre en una query nueva de la base de la
  pestaña activa, o de la que se elija si el script no sirve para ella.
- **Agregar a la query abierta:** desde el menú contextual o el diálogo; lo
  suma al final.
- **Guardar:** la ⭐ en la barra de la query guarda la selección, o toda la
  query si no hay selección. También está el "+" de la vista.
- **Organizar:** carpetas anidadas (`Mantenimiento/Índices`), búsqueda,
  duplicar, editar y borrar.
  - Las carpetas se crean con el botón de carpeta de la vista, o con "Nueva
    subcarpeta…" en el menú de una carpeta. Existen aunque estén vacías: se
    guardan en la preferencia `library.folders`, que se sincroniza.
  - Se arrastran scripts y carpetas a otra carpeta, o a la raíz soltándolos en
    el fondo de la lista.
  - Renombrar una carpeta mueve todo lo que tiene adentro. Borrarla sube sus
    scripts y subcarpetas un nivel: nunca borra scripts.
- **Importar:** archivos o una carpeta entera de scripts (`.sql`, `.js`,
  `.json`, `.cql`, `.redis`, `.flux`, `.cypher`, `.txt`…). Las subcarpetas pasan a ser carpetas de la biblioteca, y un
  comentario en la primera línea queda como descripción. Si se importa de nuevo
  la misma carpeta, se actualizan los scripts en lugar de duplicarse.
- **Exportar:** toda la biblioteca, o un script, como archivos en carpetas.
  La extensión sale del lenguaje de los motores del script. Así sirve para
  todos los motores, sin una lista:

  | Lenguaje | Extensión |
  |---|---|
  | SQL, PartiQL (DynamoDB), SQL de Cosmos DB | `.sql` |
  | Shell de MongoDB | `.js` |
  | Documentos JSON (Elasticsearch, Solr, CouchDB…) | `.json` |
  | CQL | `.cql` |
  | Comandos de Redis | `.redis` |
  | Flux | `.flux` |
  | Cypher | `.cypher` |

  La descripción va como comentario en la primera línea, con la sintaxis del
  lenguaje: `--` o `//`. JSON y Redis no tienen comentarios, así que ahí no
  se agrega.

La biblioteca vive en el estado local (la tabla `library`) y viaja con la
[sincronización en la nube](sincronizacion.md).

## Git

El botón de git de la vista guarda la biblioteca en un repositorio (GitHub,
GitLab, Azure DevOps…), como respaldo o para compartirla con el equipo.

- **Vincular:** URL del repo y rama (`main` si se deja vacía). DBine clona el
  repo en su carpeta de datos (`library-git/`), trae los scripts que ya tenga
  (no borra ninguno de la biblioteca) y sube los de la biblioteca.
- **En el repo:** cada script es un archivo en su carpeta
  (`Mantenimiento/Índices/Reindexar.sql`), con la misma extensión que al
  exportar y el texto tal cual. Lo que un archivo no
  dice (id, motores, descripción, carpetas vacías) va en
  `.dbine/library.json`. Los demás archivos del repo (un README) no se tocan;
  un archivo agregado a mano en el repo entra como script nuevo, y su
  extensión decide el motor:
  - `.sql`: cualquier motor SQL;
  - `.js`: MongoDB;
  - `.cql`: Cassandra;
  - `.redis`: Redis;
  - `.cypher`: Neo4j;
  - `.flux`: InfluxDB;
  - `.json`: todos los motores.
- **Ventana de git:** los cambios de la biblioteca sin confirmar, cuántos
  commits hay para subir y para traer, y los botones **Commit**, **Pull**,
  **Push** y **Sincronizar** (commit + pull + push). Un pull trae scripts
  nuevos, cambios y borrados.
- **Conflictos:** si un script cambió en el repo y en la biblioteca a la vez,
  el pull se detiene sin tocar nada y ofrece **Usar la versión del repo**
  (descarta los cambios locales de la biblioteca) o **Subir la mía**
  (reemplaza el repo con `--force-with-lease`).
- **Credenciales:** DBine usa el git instalado en la máquina con las
  credenciales del usuario (SSH, el administrador de credenciales de git). No
  guarda contraseñas ni tokens, y git nunca pide una: si falta, el error lo
  dice.
- **Con la sincronización en la nube:** gana el último cambio. Un pull deja la
  biblioteca como el repo, y restaurar un respaldo de la nube la deja como el
  respaldo; el próximo commit sube esa versión al repo. El vínculo al repo es
  de cada máquina: no viaja en el respaldo.
- **Desvincular:** borra la copia local y el vínculo; no borra scripts, ni en
  DBine ni en el repo.

| Comando | args | Devuelve |
|---|---|---|
| `library_git_status` | `{ fetch }` | estado: remoto, rama, cambios, commits para subir y traer |
| `library_git_link` | `{ remote, branch }` | `{ applied, pushed }` |
| `library_git_unlink` | — | `void` |
| `library_git_commit` | `{ message }` | `void` |
| `library_git_pull` | — | `{ applied, conflicts }` |
| `library_git_push` | — | `void` |
| `library_git_sync` | `{ message }` | `{ applied, conflicts }` |
| `library_git_resolve` | `{ keep: 'remote' \| 'local' }` | `applied` |

## Comandos

| Comando | args | Devuelve |
|---|---|---|
| `list_library` | — | `LibraryScript[]` |
| `save_library_script` | `{ script }` (`id` vacío = nuevo) | `LibraryScript` |
| `delete_library_script` | `{ id }` | `void` |
| `import_library_files` | `{ paths, engines, folder }` | `{ imported, skipped }` |
| `export_library` | `{ dir, ids }` (`ids` vacío = todos) | cantidad exportada |

`LibraryScript` es `{ id, name, folder, description, engines, text, updated_at }`.
