# Backups

Right-click a database › **Backups…** opens a tab with everything about that
database's backups: the history, making a new one and restoring. On engines
whose backups cover the whole server (Redis, Elasticsearch snapshots…), the
option is also in the connection's menu.

The tab has two parts.

## DBine copies

They work for **all engines**. A copy is a script with the database's
structure and, if requested, its data, saved to a local file.

- **New copy:** you choose the file and whether it includes the data. By
  default it goes to `Documents/DBine/Backups/<connection>/<database>-<date>.sql`.
  - The script drops and recreates each object (`DROP … IF EXISTS` and
    `CREATE`). The data follows (`INSERT` in batches, in the engine's
    syntax) and, at the end, the indexes and foreign keys.
  - It is the same as **Generate script…** with all objects checked.
  - Progress is shown and it can be cancelled. If it is cancelled or fails,
    the half-written file is deleted.
- **History:** date, whether it has data, number of objects and rows, size and
  file path. It stays in this machine's local state: it doesn't travel with
  sync.
- **Restore:** runs the script on the chosen database, which can be the same
  or another. It is the same as **Run file…**.
  - It asks for confirmation, because it replaces the copy's objects that
    already exist in that database.
  - You can continue even if a statement fails; errors are shown at the end.
- **Delete:** removes the copy from the list and, if chosen, also deletes the
  file.

## Server backups

These are made by the engine itself (`BACKUP DATABASE` in SQL Server,
`BACKUP … TO Disk(…)` in ClickHouse, Elasticsearch snapshots…), where they
exist:

- **History:** what the server has on record (date, type, database, size,
  location and status), on engines that report it.
- **Back up, Restore and Delete:** each engine asks for its options (type,
  destination, compression, repository…). With those options DBine builds the
  **script in the engine's language**, shows it and runs it only on clicking
  **Run**. The script can be copied to run elsewhere.
- The files stay where the server leaves them, not on this machine. The tab
  says what each engine needs (for example, a configured backup disk in
  ClickHouse or a registered repository in Elasticsearch).
- These scripts aren't saved in the query history.

**Read-only** connections show the history, but don't offer making backups or
restoring.

What each engine supports is in
[`engine-support.md`](engine-support.md#backups).

## Contract

- `Driver::backup()` says what the engine offers (`BackupSpec`): the options
  of a backup and of a restore, and whether it can restore, delete and list
  the history. It also says whether backups cover the whole server, which
  database the scripts run on and a note for the tab. `None`: only DBine
  copies.
- `Session::backups(database)` reads the server's history.
- `Driver::backup_script(action)` writes the script of a backup, a restore or
  a delete.

DBine copies don't go through the driver: they use script generation
(`generate_script`) and file execution (`run_script_file`).
