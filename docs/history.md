# Query history

Everything executed from the editor goes into the history: the view with the
clock in the activity bar. It has two views: **This tab**, the active tab's
history, and **All runs**, the machine's complete history. The chosen view is
remembered.

## This tab

Shows the timeline of the tab you're on, newest to oldest and grouped by day.
It follows the active tab: when you switch tabs it reloads with the other
one's history.

- In a **saved query**:
  - **Saved**: the versions of its text, with the lines added and removed
    relative to the previous version.
  - **Run**: each time it was run, with duration, rows and a red mark if it
    failed.
- In a **project file**:
  - **Run**: what was run from that file.
  - The git **commits** that touched it (message, short hash and author),
    following renames: if the file had another name in that commit, it says
    so.
- Other tabs (table data, diagrams, etc.) have no timeline.
- The buttons at the top filter by type (Versions, Runs, Commits).

A **click** on a version or a commit opens the comparison with the editor's
current text (the then-text on the left, the now-text on the right), with
**Restore this version**:

- In a saved query, the current text is first saved as a version and then
  replaced by the restored one, which is also saved. Nothing is lost: the
  previous text stays in the timeline and the editor can undo it.
- In a file, the restored text stays in the editor as an unsaved change; it
  is written to disk with ⌘S.

A run opens as usual: **double-click** or **Enter** opens it in a new query.
With **right-click** you can also compare its text with the current one or
copy it; on a commit, copy its hash.

### When a version is saved

- On manual save (⌘S), on running the query and on closing its tab, as long
  as the text has changed.
- While typing, autosave leaves at most one version per minute.
- The text the query had before editing is also kept, so you can always go
  back to the state before a change.
- All versions from the last 7 days are kept; from there up to 90 days, one
  per day (the last of each day); older ones are deleted. At most 300
  versions per query.
- Versions stay on this machine only, like the history: they don't travel
  with cloud sync. They are deleted when the query is deleted.

## All runs

- Runs, newest to oldest, **grouped by the server** where they ran: the
  connection's host or, in file engines, the file name.
- Each run shows the time, the database, the number of rows (returned or
  affected, when the engine reports it), the duration and the text. Failed
  ones carry a red mark with the error.
- Search filters by text, server, database or connection name.

### What you can do

- **Double-click** or **Enter**: opens the text in a new query of that
  connection and database.
- **Right-click**: open in a new query, copy the SQL or delete the entry. On
  a server: delete its whole history.
- The view's trash can deletes the whole history.

## What is stored

- What is run from the editor, including execution plans. Reads the app does
  on its own are not stored: loading a table's data, the explorer, the
  Profiler or the Monitor.
- The last 20,000 runs are kept; older ones are deleted automatically.
- It stays on this machine only, in the local state file. It doesn't travel
  with cloud sync.
- The text is stored as it was run, even if it contained a password (for
  example, a `CREATE USER … PASSWORD`). In those cases it's advisable to
  delete the entry.
- The connection name is stored with the entry, so it stays readable if the
  connection is renamed or deleted. To reopen it, the connection must exist.

## Commands

- `execute_query` with `record: true` stores the run.
- `history_list { search?, before?, limit? }` returns the entries, newest to
  oldest. `before` is the id of the last one on the previous page.
- `history_delete { ids }`: `null` deletes everything.
- `history_of { queryId?, projectId?, filePath?, limit? }`: the runs of a
  saved query or of a project file. `execute_query` tags them with
  `query_id`, or with `project_id` and `file_path`.
- `save_query { query, checkpoint? }`: with `checkpoint` the text always
  stays as a version; without it, at most one per minute.
- `query_versions { queryId }` (without the text), `query_version { id }`
  (with the text) and `query_version_checkpoint { queryId }`, which saves the
  current text as a version if it changed.
- `project_file_log { id, path, limit? }`: a file's commits
  (`git log --follow`); empty if there is no git or nothing is committed.
  `project_file_at { id, commit, path }`: the file at that commit.
