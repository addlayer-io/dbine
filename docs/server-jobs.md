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
