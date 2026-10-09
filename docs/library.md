# Script library

Reusable scripts, such as "Reindex a table", "Blocking sessions" or "Space
per table". They don't belong to a database: they belong to an **engine**.
They open from the ⭐ in the activity bar.

## Difference from queries

| | Queries | Library |
|---|---|---|
| Belong to | a connection and a database ("Queries" node in the explorer) | one or more engines |
| When opened | that same query opens | a **copy** is created in a new query of the chosen database; the library doesn't change |
| For | what you work on in that database | what a DBA uses in any database of that engine |

## Engines

Each script declares which engines it is for:

- one or more drivers (SQL Server, PostgreSQL, MongoDB…);
- or **"Any SQL engine"** (`*sql`).

With an active tab, the view first shows the scripts that fit its engine.
The rest go under "Other engines", so a MongoDB script isn't mixed with a SQL
Server one.

A script also works for engines that share a dialect: one written for
PostgreSQL shows up in CockroachDB or YugabyteDB. This doesn't apply to the
generic `standard` dialect.

## Parameters

`{{name}}` marks a value that is asked for when the script is opened. For
example, in `ALTER INDEX ALL ON {{table}} REBUILD;` DBine asks for the table
and suggests those of the target database. The text is replaced as is,
without escaping.

## Usage

- **Open:** double-click the script (or select it and press Enter; a single
  click only selects it). It opens in a new query of the active tab's
  database, or of the one you choose if the script doesn't fit it.
- **Add to the open query:** from the context menu or the dialog; it appends
  it at the end.
- **Save:** the ⭐ in the query bar saves the selection, or the whole query
  if there's no selection. There's also the "+" in the view.
- **Organize:** nested folders (`Maintenance/Indexes`), search, duplicate,
  edit and delete.
  - Folders are created with the view's folder button, or with "New
    subfolder…" in a folder's menu. They exist even when empty: they are
    stored in the `library.folders` preference, which is synced.
  - Drag scripts and folders onto another folder, or to the root by dropping
    them on the list background.
  - Renaming a folder moves everything inside it. Deleting it moves its
    scripts and subfolders up one level: it never deletes scripts.
- **Import:** files or a whole folder of scripts (`.sql`, `.js`, `.json`,
  `.cql`, `.redis`, `.flux`, `.cypher`, `.txt`…). Subfolders become library
  folders, and a comment on the first line becomes the description. If the
  same folder is imported again, the scripts are updated instead of
  duplicated.
- **Export:** the whole library, or one script, as files in folders. The
  extension comes from the language of the script's engines. This works for
  all engines, without a list:

  | Language | Extension |
  |---|---|
  | SQL, PartiQL (DynamoDB), Cosmos DB SQL | `.sql` |
  | MongoDB shell | `.js` |
  | JSON documents (Elasticsearch, Solr, CouchDB…) | `.json` |
  | CQL | `.cql` |
  | Redis commands | `.redis` |
  | Flux | `.flux` |
  | Cypher | `.cypher` |

  The description goes as a comment on the first line, with the language's
  syntax: `--` or `//`. JSON and Redis have no comments, so none is added
  there.

The library lives in the local state (the `library` table) and travels with
[cloud sync](sync.md).

## Git

The view's git button saves the library to a repository (GitHub, GitLab,
Azure DevOps…), as a backup or to share it with the team.

- **Link:** repo URL and branch (`main` if left empty). DBine clones the repo
  into its data folder (`library-git/`), brings in the scripts it already has
  (it doesn't delete any from the library) and pushes the library's.
- **In the repo:** each script is a file in its folder
  (`Maintenance/Indexes/Reindex.sql`), with the same extension as when
  exporting and the text as is. What a file doesn't say (id, engines,
  description, empty folders) goes in `.dbine/library.json`. The repo's other
  files (a README) are left alone; a file added by hand to the repo comes in
  as a new script, and its extension decides the engine:
  - `.sql`: any SQL engine;
  - `.js`: MongoDB;
  - `.cql`: Cassandra;
  - `.redis`: Redis;
  - `.cypher`: Neo4j;
  - `.flux`: InfluxDB;
  - `.json`: all engines.
- **Git window:** the library's uncommitted changes, how many commits there
  are to push and to pull, and the buttons **Commit**, **Pull**, **Push** and
  **Sync** (commit + pull + push). A pull brings new scripts, changes and
  deletions.
- **Conflicts:** if a script changed in the repo and in the library at the
  same time, the pull stops without touching anything and offers **Use the
  repo version** (discards the library's local changes) or **Push mine**
  (replaces the repo with `--force-with-lease`).
- **Credentials:** DBine uses the git installed on the machine with the
  user's credentials (SSH, git's credential manager). It doesn't store
  passwords or tokens, and git never asks for one: if it's missing, the error
  says so.
- **With cloud sync:** the last change wins. A pull leaves the library as the
  repo, and restoring a cloud backup leaves it as the backup; the next commit
  pushes that version to the repo. The repo link is per machine: it doesn't
  travel in the backup.
- **Unlink:** deletes the local copy and the link; it doesn't delete scripts,
  neither in DBine nor in the repo.

| Command | args | Returns |
|---|---|---|
| `library_git_status` | `{ fetch }` | status: remote, branch, changes, commits to push and to pull |
| `library_git_link` | `{ remote, branch }` | `{ applied, pushed }` |
| `library_git_unlink` | — | `void` |
| `library_git_commit` | `{ message }` | `void` |
| `library_git_pull` | — | `{ applied, conflicts }` |
| `library_git_push` | — | `void` |
| `library_git_sync` | `{ message }` | `{ applied, conflicts }` |
| `library_git_resolve` | `{ keep: 'remote' \| 'local' }` | `applied` |

## Commands

| Command | args | Returns |
|---|---|---|
| `list_library` | — | `LibraryScript[]` |
| `save_library_script` | `{ script }` (empty `id` = new) | `LibraryScript` |
| `delete_library_script` | `{ id }` | `void` |
| `import_library_files` | `{ paths, engines, folder }` | `{ imported, skipped }` |
| `export_library` | `{ dir, ids }` (empty `ids` = all) | number exported |

`LibraryScript` is `{ id, name, folder, description, engines, text, updated_at }`.
