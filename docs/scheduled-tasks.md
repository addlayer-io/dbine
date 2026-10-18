# Scheduled tasks

A task runs on its own, at the time you choose, **with DBine closed**: it runs
a script, exports a query, compares two schemas, makes a backup, documents a
database, runs a notebook or sends an email, and notifies you with a system
notification.

## Where it is

The alarm clock icon in the activity bar opens **Scheduled tasks**.

- **+** creates a task and opens it in its own tab.
- Right-click on a task: **Open**, **Run now**, **Enable** / **Disable** and
  **Delete**. When you delete a task, it stops running and its history is
  erased.
- The list shows the next run, the result of the last one and, when it
  applies, **Not scheduled in the system** or **There are unapproved
  changes**.

## The task

A task has:

- a **name**;
- **Active**: if it is disabled, it is not scheduled in the system;
- **When**;
- **Notify**;
- a list of **Steps**, which run in order.

### When

The time is local, in `HH:MM` format.

- **Every day** at `HH:MM`.
- **Some days** of the week at `HH:MM`.
- **Once a month**: on day 1 to 28 of each month at `HH:MM`.
- **Every few minutes**: minimum 5.

### The steps

Each step has an optional name and what to do if it fails:

- **If it fails, stop the task** (the usual choice): the following steps do
  not run.
- **If it fails, continue with the next one.**

The run ends in one of three states:

- **OK**: all the steps finished fine.
- **With errors**: a step failed and the others went on.
- **Failed**: the task stopped at a step that failed, or could not start.

Steps can be moved up, moved down and removed.

#### Only if…

In the configuration of **any** step, the task engine understands an **Only
if…** condition:

- **Always** (the usual choice).
- **A previous step raised an alert:** the step runs only if some previous
  step raised a warning. Today it is raised by the **schema comparison** when
  it finds differences.
- **A previous step failed:** the step runs only if some previous step failed
  and the task went on.

When the condition is not met, the step **does not run** and is left in the
history as **OK**, with the summary "Not run: no previous step had alerts"
(or "…failed"). For a step to run after a failure, the step that fails must
be set to **If it fails, continue with the next one**: if it stops the task,
the following ones are never evaluated.

Today the interface offers **Only if…** in the **Send an email** step.

## Step types

### Run a script

Runs on a connection › database. The script is split into statements the same
way as in the editor ([`script-execution.md`](script-execution.md)).

- **Each statement is committed**, regardless of the connection's autocommit
  option.
- **Continue with the next statement if one fails**: optional. If any
  statement fails, the step fails anyway, and the error is left in the
  history.
- Produces `statements`, `affected` and `errors`.

### Export to file

Writes the result of a query to a folder, with the **File name** you choose
(the extension is added automatically). Formats: CSV, CSV with semicolon, CSV
for Excel, TSV, Excel (.xlsx), JSON, JSON Lines, XML and SQL (INSERT). The
options are those of the export dialog (for example, **Column names in the
first row**).

- **It always runs read-only**, even if the connection allows writing.
- If it fails or is cancelled, the half-written file is deleted.
- Produces `file` and `rows`.

### Compare schemas

Compares a **Reference database** with a **Database to compare**, with the
options **Ignore case**, **Match without the schema**, **Ignore comments** and
**Include drops** (the ones in [`schema-compare.md`](schema-compare.md)).

- If they are equal, the step finishes fine and writes nothing.
- If they differ, it **saves in a `.sql` file the sync script** that would
  leave the database to compare like the reference one, and raises a warning
  (see [Notify](#notify)).
- **The script is never run.** Reviewing it and applying it is a person's
  decision.
- Without **Include drops**, the objects that exist only in the database to
  compare are not dropped: the script mentions them in a comment.
- Produces `differences` and, if there were differences, `file`.

### Backup

- **Engine backup:** the engine's own backup, with the same options as the
  **Backups** tab ([`backups.md`](backups.md)). It runs the engine's script on
  the server; the files end up where the server leaves them.
- **DBine copy:** a script with the structure and, if **With data** is
  checked, the rows, like the one in the **Backups** tab. It is saved in a
  **Folder**, with the **File name** you choose.

Engines without backups of their own offer only the DBine copy, so any engine
can be backed up.

The **secret** options of an engine backup (for example, an encryption
password) **are stored separately, in the system keychain**, never in the
task. When you reopen the task the field is empty and says **Stored
separately, in the keychain**; if you leave it empty, the password that was
already there is kept.

The copy produces `file` and `rows`.

### Document the database

Writes the data dictionary of a connection › database to a **Folder**, with
the **File name** you choose (the extension is added automatically: `.html`
or `.md`). It has the same options as **Document the database…** in the
explorer ([`database-docs.md`](database-docs.md)): the **Format** (HTML or
Markdown), the **Schemas** (empty, all) and what to include (tables, views,
routines, triggers, other objects, source code, indexes, foreign keys,
dependencies and diagram).

- It reads the database in a **read-only session**, so it does not ask for
  approval.
- What the engine does not have or cannot be read does not make the step
  fail: it is left as a note in the document and in the history.
- Produces `file` and `tables`.
- If the task does not send its texts, the document comes out in Spanish.

### Run a notebook

Runs a saved notebook ([`notebooks.md`](notebooks.md)) from top to bottom,
as **Run all** does in its tab, and stops at the first cell that fails. With
**Send an email** after it, a notebook becomes a periodic report.

- **Notebook:** the `.ipynb` file, standalone or inside a project (a
  project's notebook uses that project's environments, as in the app).
- **Cells:** the step's editor reads the notebook and lists where each SQL
  cell runs on this machine. A connection name the file uses that no local
  connection has, or that several share, is chosen here; the step stores
  only that choice (the connection's id), never hosts or credentials.
- **Parameters:** the values for the parameters the notebook declares,
  with [variables](#variables). The step refuses a value for a parameter
  the notebook doesn't declare.
- **Executed notebook:** a copy with this run's results: **Don't save it**,
  **Next to the original** or **In a folder**, named **File name** (default
  `{notebook}-{datetime}`, `{notebook}` being the notebook's name without
  the extension). It keeps the preview limits of **Settings › Notebooks**
  and the notebook's own **Don't save result** / **Save without results**,
  as the app's save and export do. The original is never written: a copy
  that would have its name fails the step.
- **Export:** **HTML** or **Markdown**, as the notebook's export in the app,
  to the **Folder** chosen (empty: where the copy goes, else next to the
  notebook). **Charts are not in a scheduled export:** the app's window
  draws them and a task has none; where a cell has a chart, the export says
  *"Los gráficos se incluyen al exportar desde la app."* Tables, text and
  messages are all there.
- Produces `file` (the export, else the copy), `html` (the HTML export),
  `markdown` (the Markdown export), `notebook` (the copy) and `cells` (the
  cells that ran). The next **Send an email** attaches `{steps.N.file}` or
  `{steps.N.notebook}`, and can put `{steps.N.html}` as its HTML body.
- If a cell fails, the copy and the export are still written (with the
  error under that cell; the cells after it have no result, so nothing old
  passes as new) and the step fails with the cell and the reason.

How cells run:

- **Connection cells** run on one session per connection and database for
  the whole run (a transaction a cell opens stays open for the next ones,
  as in the tab; one still open at the end is rolled back when the session
  closes), split as the editor's Auto mode does, each statement committed.
  The connection's **read-only** setting holds.
- **DataFrame SQL cells** run as in the app: the same DuckDB lockdown, the
  same refused statements, the same row cap per DataFrame (**Settings ›
  Notebooks**) and the same always-blocked folders. Their sources are the
  cells above, with the rows of their run in this task when complete, else
  their query run again read-only.
- **Credentials** come from the connections, as in the app: the keychain or
  the connection's secret manager. Nothing new is stored. A secret-source
  definition not yet confirmed on this computer fails the cell with the
  usual message (open DBine and connect once to confirm it).

Nobody is there to answer, so the step **never asks**:

- **Files:** a DataFrame SQL cell reads only the files this notebook may
  **always** read on this machine (**Permitir siempre** in the app, see
  [Approving the files a notebook reads](notebooks.md#approving-the-files-a-notebook-reads)).
  Any other file fails the cell with the list of files to approve from the
  app, and nothing is read. The step's editor lists those files beforehand,
  with **Open the notebook** to run that cell in the app and allow them.
- **Confirmations:** in the app, an environment with confirmation
  (`confirm_run`) asks before running, and connections tagged as production
  are marked PROD. A task can't confirm, so each of those connections and
  databases that the notebook uses needs the opt-in **Permitir ejecutar en
  <target> sin confirmar** in the step; without it, the cell fails. A
  DataFrame SQL cell that loads a source from one of them needs it too.
- **Changes:** cells whose SQL changes data or structure (on connections
  that aren't read-only) join the task's approval, like **Run a script**:
  saving shows each one with its cell, and the task doesn't run them until
  approved. The approval covers the notebook as it is: editing one of those
  cells in the notebook voids it (see [Changes are approved](#changes-are-approved)).
- **Write cells** (cells that write a DataFrame to a table) run only when
  the step authorizes each one, as the editor shows it: the cell, its target
  (`Ventas prod › ventas › dbo.resumen`, with "(nueva)" when it creates the
  table) and its mode with its keys (`upsert: id`). A change in any of them
  voids the authorization; an unauthorized write cell fails the step at that
  cell and writes nothing. An authorized one writes as the app does (the
  same plan, token and transaction; see
  [notebooks.md](notebooks.md#writing-a-dataframe-to-a-table)) without the
  confirmation dialog, which the authorization stands for; a target that is
  production or asks before running needs the step's opt-in for it as
  well, its DataFrame reads only files with "Permitir siempre", and a
  read-only connection is refused.
- The opt-ins and authorizations are stored in the task (this machine's
  state); the MCP server and the assistant have no tools for scheduled
  tasks, and the phone app can only run a saved task, never edit it.

### Send an email

Sends an email with the server configured in
[**Settings › Mail**](#settings--mail).

- **To** and **CC**: addresses separated by commas, semicolons or lines.
- **Subject** and **Body**.
- **HTML body** (optional): an HTML file whose content goes as the email's
  HTML body, with **Body** as the plain-text version for clients that don't
  show HTML. `{steps.1.html}` is the HTML export of a **Run a notebook**
  step 1. Up to 10 MB; a missing file fails the step.
- **Attachments**: file paths. With variables, `{steps.1.file}` attaches the
  file that step 1 produced (the schema comparison, an export, a backup, the
  documentation, a notebook's export or copy). If an attachment does not exist or is not a file, the step
  fails. **Attachments can add up to 20 MB.**
- **Only if…**: see [above](#only-if). With **A previous step failed**, the
  step that fails must continue with the next one.

All the fields accept [variables](#variables). The step fails, with the
reason in the history, if there is no server configured, if an address is not
valid or if the server rejects the email.

**The history does not store the text of the email**: only the recipients and
the names of the attachments. It produces `recipients` (the number of
recipients, including CC).

An email does not change the database, so the step does not ask for approval.
Be careful with what you attach: the file leaves this machine.

## Variables

In file names, folders and scripts you can use:

| Variable | Value |
|---|---|
| `{task}` | the task name |
| `{date}` | `YYYY-MM-DD` |
| `{time}` | `HHMMSS` |
| `{datetime}` | `YYYY-MM-DD_HHMMSS` |
| `{year}`, `{month}`, `{day}` | the parts of the date |
| `{steps.N.key}` | what step number `N` produced |

The default name of a file is `{task}-{datetime}`. The characters that some
system does not accept in a name (`/ \ : * ? " < > |`) are replaced with `_`.

What each step produces:

| Step | Keys |
|---|---|
| Run a script | `statements`, `affected`, `errors` |
| Export to file | `file`, `rows` |
| Compare schemas | `differences`, `file` |
| DBine copy | `file`, `rows` |
| Document the database | `file`, `tables` |
| Run a notebook | `file`, `html`, `markdown`, `notebook`, `cells` |
| Send an email | `recipients` |

Example: a step 1 that exports and a step 2 that saves a copy as
`copy-{steps.1.rows}` generates `copy-1204.sql` if step 1 exported 1204 rows.
A variable that does not exist is left as is, not replaced.

## Notify

- **If it fails or finds differences** (default): notifies when the run did
  not finish **OK**, or when a step raised a warning (the comparison that
  found differences).
- **Always.**
- **Never.**

It is a system notification, because with DBine closed there is no window to
show it in: Notification Center on macOS, a toast on Windows and
`notify-send` on Linux. If the system cannot show it, it stays in the log and
nothing more.

## Changes are approved

A task with **Run a script** steps, or notebook cells, that **change data or
structure** runs on its own, with nobody watching. That is why it has to be
approved.

- When saving, DBine shows **This task changes data or structure**, with each
  step (and, for a notebook, each cell), its connection › database and the
  first statement it writes (`DELETE`, `ALTER`…). **Approve and save**
  leaves it ready.
- In engines that do not use SQL, a read cannot be told apart from a write:
  **any script counts** as a change.
- Connections tagged `prod`, `production`, `producción` or `prd` are marked
  as **PROD**.
- If the connection, the database or the script of one of those steps (or
  the notebook file or the text of one of those cells) is edited afterwards, the task **does not run them** until it is approved
  again. The list shows **There are unapproved changes** and the run fails
  saying that the task must be opened and saved again. Changing the time, the
  name or the other steps does not require approving again.
- A **read-only** connection stays read-only in the task and needs no
  approval.
- Exporting, comparing, making backups, documenting the database and sending
  emails do not ask for approval: they do not change the database (the
  comparison writes a script to a file, it does not run it).

## How it runs with DBine closed

When saving, DBine registers the task in the system scheduler. At the given
time, the system starts DBine in a **windowless mode**: it loads the task,
runs it and exits. In that mode nothing else of the app starts (no MCP
server, no updater, no telemetry, no cloud sync).

The system entry carries **only the task id**, never connections or
passwords.

| System | What is registered | Conditions |
|---|---|---|
| macOS | A LaunchAgent in `~/Library/LaunchAgents` | Runs while the user is signed in. DBine has to be in **Applications**: it refuses to schedule from the disk image or from a temporary copy that macOS made. |
| Windows | Task Scheduler, under the `DBine` folder | "Only when the user is logged on". If the computer was off, it runs when it is turned on. |
| Linux | A systemd user timer; if there is no systemd, a `crontab` line | With cron, the keychain may not be accessible: a task that needs saved passwords may fail. |

- On startup, the app **registers again** the active tasks. That covers DBine
  having been moved to another folder or updated.
- On macOS, entries registered under the old identifier's label
  (`com.addlayer.dbine.task.<id>`) move to `io.addlayer.dbine.task.<id>` on
  that same startup, once the state was copied: the new one is registered
  before the old one is removed, so a task never runs twice. From then on
  tasks run from the newest app ([identifier-migration.md](identifier-migration.md)).
- If the system rejects the task, it is saved anyway and the list says **Not
  scheduled in the system**, with the reason.
- In a development build, tasks are not registered.

### Passwords

Tasks use the connections' **saved passwords**. A connection that does not
save its password **cannot run unattended**.

On macOS, after an update the system may ask for keychain access again. As
there is nobody to answer, the run fails after 45 seconds with a message that
asks you to open DBine once and choose **Always Allow**. This will stop
happening once versions are signed with a stable Developer ID.

## Run now

**Run now** runs the task inside the app, in the background, and records it in
the history just like a scheduled run. With unsaved changes in the tab, the
button is disabled (**Save your changes before running it**).

## History

Each task keeps its **last 200 runs**. For each one you can see:

- the state (**Running**, **OK**, **With errors**, **Failed**);
- whether it was **scheduled** or **manual**;
- the summary of each step, for example how many rows were exported and to
  which file;
- the server messages (`PRINT`, warnings) and the errors.

**Results, data rows and passwords are not stored.**

## Settings › Mail

The server used by the **Send an email** steps. It is in **Settings › Mail**
and is stored **only on this machine**: tasks run here, so it is not synced.

- **SMTP server** and **Port** (587 by default).
- **Security:** **STARTTLS (port 587)**, **SSL/TLS (port 465)** or
  **Unencrypted**. Unencrypted, the user, the password and the emails travel
  readable over the network: use it only with a server on your local network.
- **User** and **Password**: empty if the server does not require signing in.
  **The password is stored in the system keychain**, never in the state file
  or the logs. If you leave it empty, the stored one is kept (**Saved**);
  without a user, none is stored.
- **Sender address** (required) and **Sender name**.
- **Send a test email:** sends an email to the recipient you type, with what
  is in the form (saved or not). If it fails, it says what: connection
  refused or not answering, secure connection failure (the security does not
  match the port), user or password rejected, or the server's rejection.

Each SMTP command waits up to 30 seconds and the whole send, 120.

## Only on this machine

Tasks point to this machine's connections and folders. **They are not part of
the backup or of cloud sync**, and restoring a backup does not delete them.
The same goes for the mail server and its password.

What each engine supports is in
[`engine-support.md`](engine-support.md#scheduled-tasks).

## Contract

Tasks add no methods to the drivers: each step type uses what already exists.

- **Run a script:** the query's script runner.
- **Export:** `Session::execute` with the result exporter, in a read-only
  session.
- **Compare schemas:** the schema loading and `Driver::sync_script`.
- **Engine backup:** `Driver::backup()` and `Driver::backup_script()`.
  **DBine copy:** `list_objects` and script generation.
- **Document the database:** the same as the feature
  ([`database-docs.md`](database-docs.md)), in a read-only session.
- **Run a notebook:** the notebook's own paths, run without a window
  (`src-tauri/src/notebook/headless.rs`): connection cells through the
  scheduled scripts' runner on a session per connection and database,
  DataFrame SQL cells through `notebook::local::run_local` (the function
  behind the app's `notebook_run_local`), the export through
  `notebook::export`. What the tab does in TypeScript (resolving targets,
  turning a run into a cell's result) is done again there, the same way.
  Command: `scheduled_notebook_plan` (what the step's editor shows; it runs
  nothing). Notebook parameters come in through a hook there
  (`executable_sources`); write cells through `write_cell` (what the editor
  shows and authorizes) and `write_unattended` (`notebook::write`'s prepare
  and run).
- **Send an email:** it does not use the driver; it sends over SMTP with the
  server from **Settings › Mail**. Commands: `mail_settings_get`,
  `mail_settings_save` and `mail_test`.

The model (`ScheduledTask`, `Step`, `TaskRun`) is in
`crates/dbine-core/src/tasks.rs`. Each step type is a function in
`src-tauri/src/tasks/steps.rs` that receives its configuration as JSON, so a
new type changes neither the model nor the tables.
