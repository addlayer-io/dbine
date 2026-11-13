# Server jobs

Many database servers run work of their own on a schedule: SQL Server Agent
jobs, Oracle's `DBMS_SCHEDULER` jobs, Snowflake tasks, Databricks jobs,
pg_cron, MySQL events… DBine lists them next to its own
[scheduled tasks](scheduled-tasks.md), shows what each one runs and how its
runs went, and lets you run, stop, enable or disable them.

## Where it is

The alarm clock icon in the activity bar opens **Scheduled**. Its **On the
servers** section has one group per connection whose engine has jobs.

- A group reads its server's jobs when you open it, and again every minute
  while it stays open. Opening **Scheduled** never connects to servers whose
  group is closed.
- A red number on a group counts the jobs whose last run failed.
- Jobs that come with the engine (Oracle's maintenance jobs, SQL Server's
  policy purge, TimescaleDB's built-in policies…) are marked as system jobs and
  hidden by default. A toggle on the connection's group shows them. Which jobs
  count as system is listed in `engine-support.md`.
- Each job shows the result of its last run, its schedule (or **On demand**)
  and its next run. Disabled jobs are dimmed.
- When the server has no jobs, or only some are visible, the group says why:
  pg_cron installed in another database, a login that only sees its own jobs,
  the event scheduler turned off…
- **Server jobs** on a connection's menu in the explorer, or in the command
  palette, opens the section on that connection.

Right-click on a job: **Open**, **Run now**, **Stop** (while it runs),
**Enable** / **Disable** and **View script**. Each item appears only where the
engine offers it for that job.

## A job's tab

- **Schedule**, **Next run**, **Last run** (result, start and duration), owner
  and description.
- **Steps**: what the job runs, in order, with the database or warehouse each
  step runs on (Agent steps, a Databricks job's tasks, a chain's steps, the
  statement of a pg_cron job or an event).
- **Properties**: the rest the engine reports about the job.
- **History**: the runs, newest first, with their result, what started them
  and the message. Click a run to see each step's result and the whole error.
  **More runs** loads up to 500.
- **Run now**, **Stop**, **Enable** / **Disable** and **View script**, which
  opens the script that recreates the job in a new query.

The tab reads the job again every 30 seconds while it's shown, and right after
each action.

## Changing a job

**Run now**, **Stop**, **Enable** and **Disable** ask first, naming the job and
the connection. **Run now** returns as soon as the server takes the run; the
run's progress shows in the history.

On a **read-only** connection you can see jobs, their steps and their history,
but not run, stop, enable or disable them: DBine doesn't offer it, and the
connection refuses it if asked.

What each engine offers is in [engine support](engine-support.md#server-jobs).

## Managing the scheduler

Beyond the jobs, DBine can create, edit and delete every kind of object the
engine's scheduler has: SQL Server's shared schedules, operators, alerts,
proxies and categories; Oracle's programs, schedules, chains, windows and
destinations; Snowflake's alerts; Elasticsearch's lifecycle policies… The jobs
come first. Which engines offer it, which kinds each one manages and what it
cannot do is in [engine support](engine-support.md#managing-the-scheduler).

### The tree

Under **On the servers**, a connection whose engine has a scheduler shows a
tree of its kinds, with the jobs first. Open a kind to see its objects. Objects
that come with the engine are marked as system objects: they open read only
and **Delete** is not offered on them. Each object's menu offers only what the
engine lets this user do with that object (for example, not editing another
owner's schedule), plus the kind's own actions.

### The designer

Creating an object from its kind's menu, or opening or editing one from its own, opens a designer tab (**Create …** for a new one).
The form comes from the driver, so it follows the engine: sections or tabs,
fields, and lists such as a job's steps and schedules, which can be added to,
removed from and reordered. Required fields are marked; a field that doesn't
pass is shown in red and the tab says some need review. A tab, field or list
that doesn't apply to what is chosen (a step type, a schedule type, the
scheduler a PostgreSQL job runs in) is not shown, and what it holds is not sent.

The tab's buttons:

- **Reload** reads the object again; if there are unsaved changes it asks
  before discarding them. Closing the tab asks too.
- **View script** shows the script that **Save** would run, without running it.
- **Open in a query** opens that script in a new query tab.
- **Save**.
- **Delete**, on an existing object that can be deleted.
- The kind's actions (below), as buttons or under **More actions**.

An object the user cannot change (the server's permissions, a system object, a
read-only connection) opens read only, with a note saying why.

### Save and delete ask first

**Save** and **Delete** show the script that will run on the connection and wait
for a yes: "Create / Save the changes to … on …? This script runs:". A delete
says it cannot be undone. If there is nothing to change, the server returns no
script and nothing is run.

- On a **production** connection the dialog also says so.
- Some changes have a cost the user should accept: the engine can only
  recreate the object (its run history, counters or state go), or the change
  cannot be done in one transaction and a failure can leave it half changed.
  The script marks each of these with a warning comment (a line starting with
  `⚠`). The dialog lists them under **Before continuing, keep in mind** and its
  button reads **Continue** instead of the usual yes.

The same dialog, with the script, comes before every action.

### Secrets

Passwords, tokens and other secret fields are never read back from the server.
A secret field opens empty: leave it empty to keep the current value, or type a
new one. In the script shown, and in the one **Open in a query** opens,
the secret values you typed appear as `********`: only **Save** writes the real
ones. If you run an opened script, replace them first. Connection strings and
credentials the engine hands back are masked in scripts too.

### Actions

A kind can have actions besides create, edit and delete: open or close an
Oracle window, run a program or a chain now, refresh a materialized view, reset
an alert's count, acknowledge a watch, take a backup now… Each action is
offered only on the objects where it applies at that moment, is shown as a
script and confirmed like a save, and is refused on a read-only connection.
After it runs, the tree and the open tab are read again.

Two more are offered on a job, only where the engine allows them:

- **Start at step…** asks which step and runs the job from there (SQL Server
  Agent, Oracle chains, Databricks tasks, Snowflake graphs, Elasticsearch
  watches…).
- **Clear history** deletes the job's run history after a confirmation that
  says it cannot be undone.

They are in the job's menu and under **More actions** in the job's tab and
designer. Which engines offer them, and the permission each needs, is in the
engine support table.

### Actions that ask for values

Some actions open a form first. Their name ends in "…" in menus and buttons,
and the form is built and checked like the designer's: fields that show by
what you pick, choices the server lists, required fields and limits. **OK**
puts the values in the action's script, which is shown and confirmed as for
any action, and **Cancel** there takes you back to the form. If the engine
refuses the values, or the action fails, the error shows in the form and what
you typed stays there for another try. On a read-only connection the form and
the script open but the run is refused. The labels below are the ones in the
code; the app shows them in the interface language.

| Engine | Action | What it asks for |
|---|---|---|
| BigQuery | **Ejecutar para fechas pasadas** | A period (first and last day, in UTC, both included) or a single run time. The script is a `startManualRuns` call. Days that haven't ended and future times are refused before the request. The runs are billed, and the script warns about what happens to the destination table. |
| Databricks | **Ejecutar con parámetros** | The job's parameters, each filled with its default, or the task-level parameters of its kinds of task when the job has none. Only what you changed is sent. It also picks the tasks to run and a full refresh of its pipelines, which the script warns about. The script is a `run-now` call. |
| TDengine | **Recalcular un rango de tiempo** | A start and an optional end, today so far by default. The script is `RECALCULATE STREAM … FROM … [TO …]`, and the results replace the rows the stream already wrote to its target table. Offered on enabled streams that aren't `PERIOD`-triggered, on 3.3.7 and later. |
| Oracle | **Cambiar un paso de la cadena en curso** | The step of a running chain and the change: its state, `SKIP`, `PAUSE`, `PAUSE_BEFORE` or taking its pause off. It calls `ALTER_RUNNING_CHAIN`. |
| Oracle | **Detener un paso en curso** | The running step to stop (`STOP_JOB`); `force` is offered only with `MANAGE SCHEDULER`. |
| Couchbase | **Unir los backups de un rango de fechas** | A start and an end date. The default leaves out the newest day, and it asks for at least two backups in the range. |
| Snowflake | **Ejecutar con otra configuración** | A JSON object for `EXECUTE TASK [GRAPH] … USING CONFIG`. Snowflake merges it into the task's own configuration for that run only. |
| Elasticsearch (8.7+) | **Iniciar desde…** | A time, **Desde (UTC)**, never in the future; the default is now minus the transform's sync delay. It sends `POST _transform/<id>/_start?from=<time>`. Offered on a stopped continuous transform that hasn't completed a checkpoint (new, or just reset). |
| Elasticsearch | **Reconocer acciones…** | The watch's actions waiting for an acknowledgement, all picked by default. It sends `PUT _watcher/watch/<id>/_ack/<a>,<b>`. |
| OpenSearch, Open Distro | **Reconocer alertas…** | The monitor's active alerts, labelled by trigger, bucket keys or documents, severity and start time. It sends `POST _plugins/_alerting/monitors/<id>/_acknowledge/alerts` (`_opendistro` on Open Distro). |

On Oracle, changing the state of a step, stopping one or taking a pause off
makes the chain evaluate its rules again, so other steps may start or the
chain may end and stop the steps still running: the script warns. Both
actions are offered on a chain that is running now, where the login manages
it.

On Snowflake the configuration is combined with the task's, not substituted
for it: fields you leave out keep their values, and a field you delete from
the editor isn't removed. The editor starts from the current configuration.
If the graph is already running, the new run starts when it ends. A task
graph with unpublished changes runs its published version, and one that was
never published is published first; the form and the script warn about it,
and so does **Ejecutar ahora**.

On Elasticsearch, a transform started from a time processes in its first
checkpoint only the groups (pivot) or keys (latest) with documents whose sync
field is at or after it; the rest reach the destination index when they
change, and after that it runs like any continuous transform. Elasticsearch
refuses `from` on batch transforms and ignores it once a checkpoint exists:
reset the transform first to start an existing one from a date. If a first
checkpoint was interrupted, the documents it already wrote stay, and the
script warns. **Reconocer acciones…** throttles only the picked actions until
the condition stops holding; the others keep running, and **Reconocer** still
acknowledges them all. On OpenSearch, acknowledged alerts stop running their
trigger's actions until they complete. If the Alerting plugin doesn't answer
within 30 seconds, DBine reports it instead of waiting.

### Scheduler log

Engines that keep a log for the scheduler as a whole (SQL Server Agent's error
log, Oracle's job and window logs, MySQL's scheduler lines, Snowflake's task
history…) show it in a **Scheduler log** tab. It reads what the user is allowed
to read, and says so when the server has nothing to show.

### Read-only connections

On a read-only connection you can open every form and view every script, and
open a script in a query. You cannot save, delete, run an action, start at a
step or clear a history: DBine doesn't offer them, and the connection refuses
them if asked.
