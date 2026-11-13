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
