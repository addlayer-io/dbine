# Projects

A project is a folder on this machine with a git repository of scripts:
queries, migrations, reports. DBine shows its files, opens them in the
editor, runs them on the database you choose and works with git (changes,
commit, pull, push) without leaving the application. They are opened from the
activity bar, with the **Projects** button, the second one after the
Explorer.

The connection remains the center: a project has no connections of its own,
it only points to one of yours.

## Difference from queries and the Library

| | Queries | Library | Projects |
|---|---|---|---|
| Where the text lives | in DBine's local state | in DBine's local state | in files in a folder |
| It is saved | as you type | as you type | with ⌘S |
| History | no | optional, in its own git repository | the repository's |
| For | what you work on in a database | scripts of an engine for any database | the code the team versions and reviews |

## Linking a project

- **Link project…** (the **+** in the view): choose a folder.
  - If it is a git repository, its root is linked. If you chose a
    subfolder, DBine warns that it will link the repository root.
  - If it is not a repository, one can be initialized right there.
  - The same folder cannot be linked twice.
- **Clone repository…**: the remote's URL, the folder to clone into (by
  default `DBine/Proyectos` in your personal folder), and optionally a name and
  a branch. Cloning runs as a background task and can be cancelled while it
  downloads.
- From the Explorer: right-click the **Projects** node of a database ›
  **Link project…**. That database becomes the project's database.

**Unlink** only makes DBine stop showing it: the folder and its files are not
touched. If the folder was moved, **Relocate…** finds it again.

Projects belong to this machine: they are not synced to the cloud nor included
in the backup, because their paths only make sense here.

## The active database

Each project has an **active database**: the connection and database where its
files run. It is chosen in the project's "Active database" row.

The project's file tabs follow the active database: if you change it, they all
move to the new one. There are two exceptions:

- A tab where you chose another database in its own selector stops following
  the project. The **Project database** link in its header brings it back.
- A tab that is running, or has an open transaction, stays on the previous
  database until it finishes, and DBine says so.

If the chosen connection was deleted, the project shows "Connection not
available" and its files cannot be run until another is chosen.

### From the Explorer

Each database in the Explorer has a **Projects** node with the projects that
use it, with their branch and the number of uncommitted changes. A click on a
project opens it in the Projects view and makes that database its active
database (if the project has environments, it activates the environment that
points to that database, or asks which one to assign it to).

## Environments and `.dbine.json`

A repository can declare environments (for example `dev`, `qa` and `prod`) in
a `.dbine.json` file at its root. It is shared with the team like any other
file in the repository:

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

| Field | What it is |
|---|---|
| `name` | Suggested name for the project. |
| `engine` | The scripts' engine (the driver id). When choosing a database, that engine's connections are suggested. |
| `environments[].name` | Name of the environment: letters, numbers, `_`, `.` or `-`, up to 40. Not repeated. |
| `environments[].engine` | That environment's engine, if it differs from the root's. |
| `environments[].confirm_run` | Asks for confirmation before each run in that environment. |
| `environments[].description` | A text to recognize it. |
| `default_environment` | The active environment when the project is linked. |

The file **has only names**. Each person decides, on their machine, which
connection and database to use for each environment; that choice does not go
to the repository. If the file brings fields that look like credentials
(`password`, `user`, `host`, `url`, `connection_string`…), DBine ignores them
and says so.

With environments, the "Active database" row shows each one with its database
(or "Unassigned"). A click activates an environment; the pencil changes its
database. Each file's header shows the environment in use, and in one with
`confirm_run` each run asks first: "You are about to run on “prod”
(connection › database)".

**Define environments…** (project menu) edits the file from DBine. If the file
is not valid JSON, the project uses a direct database and shows the error. If
the active environment was removed from the file, DBine asks to choose
another.

## Files

The **Files** section shows the folder as a tree that loads each level when
opened:

- Git marks: **M** modified, **A**/**U** new, **D** deleted (the file is still
  shown struck through), **!** in conflict. A folder with changes inside shows
  a dot. What `.gitignore` excludes is shown dimmed.
- A click opens the file in a preview tab; a double click pins it.
- Context menu: New file…, New folder…, Rename… (also F2 or Enter), Delete…,
  View changes, Discard changes…, Show in Finder/Explorer and Copy relative
  path.

Script files (`.sql`, `.cql`, `.js`, `.json`, `.txt`, `.redis`, `.cypher`,
`.flux`, `.ksql`, `.n1ql`, `.psql`) open in the query editor, with everything
usual: run, plans, format, transactions, "Run on several databases…" and the
AI assistant. If the extension is not the database engine's (a `.js` on
PostgreSQL), a notice appears, but it can still be run. Other text files (a
`README.md`, a `.yml`) are edited without the Run button. Binary files or
files over 5 MB are not opened.

## Saving

Unlike queries, a file is **not saved by itself**: it is saved with ⌘S or with
the **Save** button in the header. While it has changes, its tab shows a dot.

- When saving, the file's line endings (LF or CRLF) and BOM are kept.
- If the file changed on disk since you opened it (another editor, a `git
  pull` in a terminal, another DBine window), DBine does not overwrite it: it
  shows "The file changed outside DBine." with **Reload** (discards your
  changes) or **Overwrite** (saves yours).
- A tab without changes of its own reloads by itself when the file changes on
  disk.
- If the file was deleted, the tab says so, with **Save again** or **Close**.
- Closing a tab with changes asks: Save, Don't save or Cancel. The same when
  closing the window or quitting DBine, for all the unsaved files in all the
  windows.

## Changes and git

The **Changes (N)** section lists what changed since the last commit. A click
on a file opens its changes side by side (the last commit on the left, the
current file on the right). Each row has "Open file" and "Discard changes".

- **Commit**: write the message (⌘↵ also commits). All the changes are
  included, also new and deleted files. If git does not have your name and
  email yet, DBine asks for them once.
- **Pull**: brings the remote's changes. It respects your git configuration
  (merge or rebase). If you have unsaved project files, it first offers to
  save them.
- **Push**: pushes your commits. If the branch does not exist on the remote
  yet, it creates it.
- **Sync**: Pull and, if it goes well, Push.
- **Fetch** (project menu) and **Refresh** (view header) query the remote
  without changing anything.

Pull, Push, Sync, Fetch and Clone run as background tasks (the Tasks panel
shows them) and can be cancelled while they talk to the remote. There is only
one git operation at a time per project.

The project's row shows the branch, the commits to push (↑) and to pull (↓),
and the number of uncommitted changes. The Projects button in the activity bar
shows the total changes of all the projects.

### Conflicts

If a Pull collides with your changes, DBine **leaves the repository halfway
through the merge or rebase** (they are your files: nothing is discarded on its
own) and shows the **Conflicts** section with each file:

- **Open**: the file with the conflict markers, to edit by hand.
- **Use mine** or **Use the remote's**: keeps one of the two versions.
- **Mark resolved**: after editing it.

With everything resolved, **Continue** finishes the merge or rebase; **Abort**
returns the repository to how it was before. An operation left half-done
outside DBine is detected all the same.

### Credentials

Git uses your own credentials: the credential helper you have configured or
your SSH agent. DBine never asks for or stores them. If the remote rejects
access, the message says what is missing to configure. If the SSH server is
not trusted yet, you have to connect once from a terminal.

## When something is missing

| Situation | What DBine shows |
|---|---|
| git is not installed | Files work; the git sections ask to install it (git-scm.com). |
| The folder is missing | "The folder was not found.", with Relocate… and Unlink. |
| The folder lost its `.git` | Initialize or Unlink. |
| Detached HEAD | The branch says "Detached HEAD (abc1234)"; Commit, Pull and Push are disabled. |
| No remote | Pull, Push and Sync disabled; "Add remote…" in the menu. |

## Security

- All file operations go through DBine, which verifies that the path stays
  inside the project folder: nothing can be read or written outside it, or
  inside `.git`.
- Symbolic links are not followed outside the folder, and deleting one deletes
  the link, not its target.
- The repository never has passwords: `.dbine.json` carries only names, and
  the relation between environments and connections stays on this machine.

## Limits

- There is no branch switching or creation, partial staging, history or
  blame; for that, git from a terminal keeps working alongside DBine.
- Up to 5000 changes are shown.
- Files over 5 MB are not opened; differences are not shown for binary files
  or files over 2 MB.
