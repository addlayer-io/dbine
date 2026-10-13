# Feedback

The button at the bottom of the activity bar (the speech bubble under
Settings) opens **Send feedback**. The user picks what kind of feedback it is,
writes a title and a description, and presses **Continue on GitHub**. DBine
opens the repository's issue form in the browser with everything filled in;
the user only presses **Create** there.

DBine sends nothing itself and holds no GitHub token: the issue is created by
the user's own GitHub account, in the browser. A GitHub account is needed.

## Kinds and labels

Each kind opens its own [issue form](https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/syntax-for-issue-forms)
in `.github/ISSUE_TEMPLATE/`. The form applies the labels, even when the
reporter has no triage rights on the repository. The URL never carries
labels, assignees or anything else that could change how the issue is filed.

| In the app | Form | Labels |
|---|---|---|
| Reportar un error | `bug.yml` | `bug`, `from-app` |
| Sugerir una mejora | `enhancement.yml` | `enhancement`, `from-app` |
| Pedir un motor nuevo | `driver-request.yml` | `driver-request`, `from-app` |
| Hacer una pregunta | `question.yml` | `question`, `from-app` |

`config.yml` keeps blank issues available for everything else.

Issue forms only apply labels that already exist in the repository:
`driver-request` and `from-app` have to be created once. The forms take
effect once they are on `main`.

## How the form is prefilled

Issue forms fill their fields by `id` from the query string:

```
https://github.com/addlayer-io/dbine/issues/new?template=bug.yml&title=…&description=…&steps=…&diagnostics=…
```

| Field id | Forms | From the dialog |
|---|---|---|
| `title` | all | the title; for a new engine, "Support for <engine>" |
| `engine` | `driver-request` | the engine's name |
| `description` | all | the description |
| `steps` | `bug` | the steps to reproduce (optional) |
| `diagnostics` | all | the diagnostics, when the checkbox is on |

The field ids are a contract with the app: renaming one in a form leaves that
field empty.

The URL is built in `web/src/composables/feedbackUrl.ts`:

- every value is URL-encoded; control characters are removed and the title is
  one line of at most 120 characters;
- the whole URL stays under 7,500 characters (GitHub refuses much longer
  ones). A longer description is cut, with a "…(truncated)" note at the end,
  and then the steps if needed; the dialog warns about it before the user
  continues, so the rest can be added on GitHub.

The backend (`src-tauri/src/commands/feedback.rs`) only opens the URL, and
only when it is one of these four forms of `addlayer-io/dbine`: the web side
can't open other links through it.

## Diagnostics

Optional, with a checkbox: on by default for errors, off for the other kinds.
The dialog shows the exact text that goes in the issue:

```
DBine: 0.1.10
OS: macOS 15.1 (aarch64)
Language: es
Engine: postgres
Server: PostgreSQL 16.4
```

- DBine's version, the OS name and version, the CPU architecture and the UI
  language;
- when the active tab's connection is connected: its engine id and the first
  line of the server's version. Not for new engine requests.

Never: host names, users, database names, connection names, queries,
passwords or any other connection setting. The diagnostics type has no field
for them, and the unit tests check that they don't leak.

## Privacy

The issue is **public** on GitHub, under the user's account. The dialog says
so before continuing: don't include private data. Nothing is sent until the
user presses **Create** on GitHub, and they can edit everything there first.

## Tests

```
cd web && node --test src/composables/feedbackUrl.test.mjs
cargo test -p dbine --lib feedback
```

The first covers the URL builder (encoding, the form per kind, truncation and
the length cap, diagnostics without connection data); the second, the
backend's check of which URLs it opens.
