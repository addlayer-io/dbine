# DBine: guide for agents

DBine is a desktop multi-engine database manager: Tauri 2 with a Rust core and
a Vue 3 + Element Plus UI.

## Structure

- `crates/dbine-driver`: the driver contract (the `Driver` and `Session`
  traits, `DriverInfo`, the result model, SQL helpers, `ReadOnlySession`). It
  is lightweight and does not depend on any database client.
- `crates/drivers/<engine>`: one crate per engine or protocol family. How to
  write one: `docs/drivers.md`.
- `crates/dbine-drivers`: the driver registry, with one cargo feature per
  crate.
- `crates/dbine-core`: the local state in SQLite (connections and saved
  queries) and the secrets in the system keychain.
- `src-tauri`: the Tauri commands. Each one takes a single `args` and uses
  `rename_all = "camelCase"`. Errors are returned as `CommandError`
  `{kind, message}`.
- `web/`: Vue 3 + Pinia + Element Plus, with SCSS (no Tailwind). The workbench
  imitates VS Code and the style tokens are in `web/src/styles/global.scss`.

## Main rule: every feature is for every engine

A new feature has to work on **all** drivers: execution plans, data editing,
export, autocompletion, cancellation, read-only mode, etc. There are only two
exceptions:

- **The engine doesn't have the capability.** For example, Redis has no
  execution plans.
- **There is no reasonable way to offer it** with the available protocol or
  client.

Each engine may have its own particularities in how it implements the feature,
but a feature is not shipped "only for SQL Server" or "only for the SQL
engines".

To comply:

1. The feature enters through the contract (`crates/dbine-driver`) with a
   default method that returns `Error::Unsupported` and, if the UI needs it, a
   capability in `Driver` (like `supports_explain`). The UI shows the feature
   only where it is supported.
2. It is implemented in **every** crate under `crates/drivers/`, in the same
   batch of work.
3. The engines left without the feature are listed in
   `docs/engine-support.md`, with the reason. "There was no time" is not a
   valid reason; it stays as an explicit pending item.
4. It is tested against real servers (`dbine-test-*` containers) on the
   engines that have a Docker image or an emulator.

## Conventions

- UI texts in the five locales (Spanish is the source of the UI strings);
  documentation, markdown, agent instructions and the changelog in English;
  code, comments and commits in English.
- Commits prefixed with `feat:`, `fix:` or `perf:`.
- Brand: "AddLayer".
- Rust types are copied by hand into `web/src/api/types.ts`, with the fields
  in snake_case.
- Passwords and secret fields never go into the state file or the logs.

## Changelog

`CHANGELOG.md` tells whoever uses DBine what each version brings: it is what
the update notice and the release notes show. It is in **English** and is the
source. Every change the user notices adds a line under `## [Unreleased]`, in
the same commit or in its own, using the format of the `changelog` agent
(`.claude/agents/changelog.md`).

The translations are in `changelog/CHANGELOG.{es,pt,fr,it}.md`: the same
published versions, without an `Unreleased` section. When preparing a release,
`python3 scripts/changelog.py release <version>` turns `Unreleased` into the
new version in the English file; then the `translator` agent adds that version
to each translation and `python3 scripts/changelog.py check` has to pass (it
fails if a translation is missing a version). The release workflow fails if
the tag has no section. The app shows the notes in the interface language by
reading the tag's translation, and in English if it can't.

## Release gate: tests and security

A release goes out only when **both** pass, on the release commit:

1. **Tests:** `cargo test --workspace` (no failures), `cd web && npx vue-tsc
   --noEmit` and `npx vite build`, on a clean worktree of that commit.
2. **Security:** the `security` agent (`.claude/agents/security.md`) runs the
   `claude-security` scan on what the release adds (since the last release
   users have) and returns PASS: a verified report with **zero findings of
   any severity**.

The scan starts at the same time as the tests: they run in parallel, not one
after the other. Any finding blocks the tag: the owning session fixes it, and
tests and scan run again on the new commit. Nobody downgrades or waives a
finding except the owner.

## Shared files

To modify these files you have to coordinate with the other sessions: announce
the change and wait for confirmation.

- `Cargo.toml` (root), `Cargo.lock`, `crates/dbine-drivers/Cargo.toml` and `crates/dbine-drivers/src/lib.rs`
- `crates/dbine-driver/src/*` (the contract)
- `src-tauri/src/lib.rs`, `src-tauri/capabilities/`, `src-tauri/tauri.conf.json`
- `web/package.json`, `web/src/main.ts`, `web/src/App.vue`, `web/src/api/*`
- `CHANGELOG.md` (each session adds its lines, in English, to `Unreleased`;
  re-read the file right before editing it) and `changelog/*` (only when
  publishing, written by the `translator` agent)

Each driver exclusively owns its folder `crates/drivers/<engine>/`.

## Git and Docker with parallel sessions

- Never use `git checkout -- <path>`, `git reset --hard`, `git clean` or `git stash`.
- Test containers: only `dbine-test-*`. All other containers belong to other
  projects and are not touched.

## Component preview (development only)

`web/dev-preview.html?view=plan|chart|results|export` mounts components with sample data
(`web/src/preview/samples.ts`) in a regular browser, without Tauri or a
database. It is for viewing them and taking screenshots with headless
Chromium while the app window is not visible. It is not part of the build:
`vite build` only bundles `index.html`.

## Commands

- Dev: `npm install --prefix web && cargo tauri dev`
- Tests: `cargo test --workspace`
- Build: `cargo tauri build`
