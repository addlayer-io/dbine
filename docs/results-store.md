# Results store

Today the query editor keeps every result in the window's memory. It loses
them all when DBine closes or crashes, and a result can't be much bigger
than what that memory holds. The results store keeps them on disk instead,
encrypted, so that:

- you close DBine (or it crashes), open it again and every editor shows its
  results as you left them;
- a result can be far larger than the window's memory (phase 2);
- a stored file is unreadable on another machine or by another user of
  this one.

It works the same for every engine: rows go into it at the one point every
driver goes through (`QueryOutcome::begin_result` / `push_row`).

## Phases

1. **Store, persistence and restore** (first PR). Editor results go to
   the store as well as to the window. A restored editor shows its result
   tabs again, without connecting. How many rows a run fetches doesn't
   change.
2. **The grid pages from the store.** Sort, filter, totals and copy work
   over the whole stored result. "Rows kept per result" goes up and accepts
   "all". The first page shows while the rest keeps arriving, and it can be
   cancelled.
3. **Export and charts from the stored result** (what you saw), with
   running the query again still offered.
4. **"Open in a notebook"** from a result tab's menu. Cell 1 is the run's
   query, already holding that result's rows; cell 2 is
   `select * from <its name>`, ready to edit. The path and the key never
   appear in a cell.

## The file

- `<app data>/results/results.duckdb`, plus its `.wal`. Temporary spill
  files go in `<app data>/results/tmp/`.
- It is opened through the DuckDB driver, like DataFrame SQL cells, on a
  DuckDB instance of its own (`isolated`). Until DuckDB has been downloaded,
  or if the store can't open, results stay in memory, as before.
- The file is created by the first stored result. Reading, deleting and
  the status never create it, so a DBine that has never stored a result
  doesn't download DuckDB at start.
- Before anything is written, the session is set up and then checked:
  - no extension installs or loads by itself, and unsigned ones are
    refused;
  - `httpfs` is loaded (installed from DuckDB's signed extension repository
    the first time). DuckDB's built-in crypto only reads encrypted files:
    writing them securely takes the OpenSSL one `httpfs` brings. Without
    it, `ATTACH … ENCRYPTION_KEY` is refused unless
    `force_mbedtls_unsafe`, which the store never sets and checks is off;
  - the file is attached with `ENCRYPTION_KEY` (AES-256-GCM);
  - `temp_file_encryption = true`;
  - a `memory_limit`;
  - external access off once the file is attached, so `httpfs` can't reach
    the network;
  - `lock_configuration = true`.

  If the check fails, the store isn't used. If `httpfs` can't be fetched
  (no network the first time), the store tries again five minutes later.

  This setup and its check are in `src-tauri/src/local_duckdb.rs`, shared
  with the scratch database a [data compare](data-compare.md) of big tables
  uses (its own folder, file, alias and key; it doesn't compact).

### Verified on DuckDB 2.0.0 (the version DBine downloads)

| What | Result |
|---|---|
| Database file and WAL after 3 M rows | no plaintext |
| Same file attached with another key, or with none | refused |
| Spilled temp files, `temp_file_encryption` at its default (`false`) | **plaintext** |
| Spilled temp files, `temp_file_encryption = true` | no plaintext |
| Writing an encrypted file without `httpfs` loaded | refused |

That's why the store always loads `httpfs` and turns temp-file encryption
on, and checks both.

## The key

- It is 256 random bits from the OS generator, created the first time the
  store opens.
- It is kept in DBine's secrets vault as `results-store-key`. The vault is
  sealed by the key in the OS keychain (macOS Keychain, Windows Credential
  Manager, Secret Service), so the key belongs to this machine **and**
  this OS user.
- It never goes in the state file, the logs, the SQL history or any SQL
  that can be seen.
- **Moved file:** a copy of `results.duckdb` on another machine doesn't
  open, because the key didn't travel with it.
- **Key missing or wrong** (vault reset, keychain access denied): the old
  file is deleted and a new store starts. The store is a cache, never the
  only copy of anything.
- **No vault** (no keychain on some Linux setups): nothing is stored, and
  results stay in memory as before. Settings says so.
- **Clearing renews the key:** "Clear now", or turning keeping off,
  deletes the files and the key too, so a copy left somewhere (a backup
  of the folder) can't be read any more. The next store gets a new key. A
  run already going when it's cleared isn't stored when it ends.
- **Read only from the vault:** the key is never adopted from an older
  keychain item of that name, which another program of the user could
  have planted.
- **Why not a key derived from the machine:** machine IDs (the hardware
  UUID, `MachineGuid`, `/etc/machine-id`) aren't secret. Any program or
  user of the machine can read them, they turn up in inventories and
  backups, they're the same for every OS user, and they change when the
  system is reinstalled or a VM is cloned.

## Layout

- `runs`: one row per editor run:
  - id, the editor tab's id;
  - connection id, database and language;
  - plan mode and the script;
  - when it ran, and whether it's pinned;
  - for a multi-database run, its labels and summary;
  - the line offset and run base;
  - the outcome's metadata as JSON: messages, log, errors, elapsed time,
    plans, the transaction state, and per result set its columns, total
    rows, truncation and rows affected;
  - `complete`.
- One table per result set, `r<run>_<n>`:
  - `_row BIGINT` keeps the order;
  - each result column is a `VARCHAR` with the grid value as JSON text,
    exactly what the driver produced. Nothing is mapped to DuckDB types
    when it's written, so after paging or a restart every value is
    byte-identical to what the grid showed.
  - Typed reads (sort, filter, sums) cast on the fly, using the
    notebook's type inference as the hint.

## Writing

- **Phase 1:** a stored result holds exactly what the editor shows, so it
  is written after the run, in the background. `execute_query` gets
  `store` (the run's id, its editor tab and what the editor keeps of it)
  and hands a copy of the outcome to the store once it's back. The answer
  doesn't wait for the write, the driver contract doesn't change, and every
  engine, including driver hosts released before the store, is covered.
  - Outcomes the editor puts together itself (a run on several databases)
    or changes (saved edits) go through `results_save`.
  - A run again in place replaces its stored copy.
- **Phase 2:** rows past what the window holds go to the store as they
  arrive, through `QueryOutcome::sink` and a bounded channel to a bulk
  load, without changing what the editor's live progress shows.
- `runs.complete` turns true after the last table. A run cut short by a
  crash is removed the next time the store opens, together with any table
  of no run.
- A write error never fails the run: the editor gets its result as today,
  and that run isn't stored.
- **Connections an organization manages** (DBine Enterprise, `ent-…`) are
  never stored, so their rows don't outlive signing out or losing access.
  The backend checks the connection that ran, not what the UI says.

## Reading

- `results_runs(tab)` returns the runs of a restored editor, without their
  rows. The editor shows their tabs at once, with the newest one active.
- `results_outcome(run)` returns a run's outcome with its rows. A restored
  tab loads it when it's shown, without connecting to the database.
- Phase 2 adds `results_page(run, result, offset, limit, sort, filter)`.

## Lifecycle

- **Closing results:** closing a result tab, trimming past "Results each
  editor keeps" or closing an editor removes those runs and their tables.
- **Disk cap:** a setting, 2 GB by default. Past it, the oldest unpinned
  results go first. A run that wouldn't fit alone stops storing and is
  marked partial.
- **At start:** runs whose editor tab no longer exists (closed during a
  crash) are removed.
- **Nothing stored, nothing on disk:** when the last stored result closes,
  the store's session closes and its files (the database, the WAL, temp
  files) are deleted. A later run creates a new file.
- **Compaction:** DuckDB reuses the space of dropped tables but doesn't
  shrink the file. So when the store opens with at least 64 MB free and
  more free than used, it copies what's stored into a new file, encrypted
  with the same key, and that file replaces the old one. This happens
  before the configuration is locked, the only time another file can be
  attached. If DBine crashes midway, the old file is still whole, and the
  next open deletes the half-written one first.
- **Settings › General:**
  - **"Keep results when closing DBine"** (`query.keepResultsOnClose`, on
    by default). Off, runs aren't stored and what's stored is deleted.
  - **"Space for saved results"** (`query.resultsSpace`, 2 GB by default),
    which shows the space they take now.
  - **"Clear now"** deletes them.
  - Without a system keychain, the setting says so and stays off.

## Security

The store puts the data of your queries on disk. It is encrypted at rest
with a key that never leaves this machine and user, but it is still new
data at rest. This page, `security-model.md` and the privacy policy say
so, and the change goes through the security scan before a release.
