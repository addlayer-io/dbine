# Contributing to DBine

Thanks for your interest in DBine. Bug reports, engine requests, fixes and
translations are all welcome.

## Before you start

- **Bugs, ideas and questions:** open an issue with the matching template
  (bug, enhancement, driver request, question). For a bug, include the
  DBine version, your OS and the database engine and version.
- **Security issues:** don't open a public issue. Report them privately from
  the repository's **Security** tab (**Report a vulnerability**), with the
  details and a way to reproduce it.
- **Larger changes:** open an issue first so we can agree on the approach
  before you spend time on it.

## Building

The requirements and commands are in the README:
[Build](README.md#build). In short:

```bash
npm install --prefix web
cargo install tauri-cli --version "^2"
cargo tauri dev          # the app with hot reload
cargo test --workspace   # Rust tests
cd web && npx vue-tsc --noEmit && npx vite build
```

## How the code is organized

- `crates/dbine-driver`: the driver contract every engine implements.
- `crates/drivers/<engine>`: one crate per engine or protocol family. How to
  write one: [`docs/drivers.md`](docs/drivers.md).
- `crates/dbine-core`: local state and secrets.
- `src-tauri`: the desktop app's commands.
- `web/`: the Vue 3 interface.

[`AGENTS.md`](AGENTS.md) has the full conventions.

## Rules a pull request must follow

- **Every feature works on every engine.** A new feature enters through the
  driver contract and is implemented in every driver crate. If an engine
  can't support it, list it with the reason in
  [`docs/engine-support.md`](docs/engine-support.md).
- **Every text in all UI languages.** Spanish is the source; the interface
  also ships in English, Portuguese, French, Italian, German, Russian,
  Simplified Chinese, Japanese and Korean. `python3 scripts/i18n/check.py`
  must pass.
- **Secrets never go to disk, logs or telemetry.** Passwords and tokens live
  in the system keychain or in memory only.
- **Commits** start with `feat:`, `fix:` or `perf:`, in English.
- **User-visible changes** add a line to `CHANGELOG.md` under
  `## [Unreleased]`.
- **Tests:** add tests for what you change. Engine-specific behavior is
  tested against real servers in Docker where an image exists.

## License

DBine is MIT licensed. By contributing, you agree that your contributions are
licensed under the same terms.
