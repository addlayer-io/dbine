# Flathub: what the owner does

How the build works is in [`docs/flathub.md`](../docs/flathub.md). This is
the checklist for getting `io.addlayer.dbine` onto Flathub and keeping it
updated.

## Before the first submission

1. **Release a version that contains this folder.** The manifest installs
   `flatpak/io.addlayer.dbine.desktop` from the tagged source, so the tag
   Flathub builds must include `flatpak/`. Ship the identifier rename
   (`io.addlayer.dbine`) in that release or an earlier one: the
   single-instance D-Bus name comes from it, and the sandbox only lets the
   app own names under its Flatpak id.
2. **Screenshots.** Replace the three placeholder URLs in
   `io.addlayer.dbine.metainfo.xml` (`TODO(owner)`) with real ones: PNG,
   16:9, at least 1248×702, hosted at a stable URL (dbine.com or the
   repository's raw files). Flathub's build fails if they can't be
   downloaded.
3. **Render the files** for that tag: Actions › flathub › Run workflow, the
   tag, `dry_run` on. The artifact has the manifest, the metainfo,
   `cargo-sources.json`, `node-sources.json` and `flathub.json`.

## First submission

New apps go through a PR to [flathub/flathub](https://github.com/flathub/flathub)
against its `new-pr` branch (not `master`):

```sh
gh repo fork flathub/flathub --clone --default-branch-only=false
cd flathub
git switch -c add-io.addlayer.dbine origin/new-pr
cp <artifact>/* .          # the five files, at the repository root
git add . && git commit -m "Add io.addlayer.dbine"
git push -u origin add-io.addlayer.dbine
gh pr create --repo flathub/flathub --base new-pr --title "Add io.addlayer.dbine"
```

In the PR description:

- Say you are the developer (AddLayer) and link https://github.com/addlayer-io/dbine.
- Justify the two permissions the linter flags, which need an exception:
  - `--filesystem=home`: file databases (SQLite, DuckDB, libSQL) write their
    journal/WAL files next to the database, which the document portal
    can't provide; also `~/.ssh` for SSH tunnels (keys, `known_hosts`), git
    projects, and importing connections from other tools' settings files.
  - `--socket=ssh-auth`: SSH tunnels and git remotes authenticate with the
    user's SSH agent.
- Mention that DuckDB's library is the official prebuilt release (pinned
  SHA-256) and llama.cpp is built from source.

Reviewers comment on the PR; a comment `bot, build` starts a test build.
When it is merged, Flathub creates `flathub/io.addlayer.dbine` and gives the
submitter write access (two-factor authentication must be on in GitHub).

## Domain verification

The id `io.addlayer.dbine` maps to the domain **addlayer.io**. After the app
is published, log in at https://flathub.org with the GitHub account, open
the app in the developer portal and start the verification: it shows a
token. Publish it as plain text at

    https://addlayer.io/.well-known/org.flathub.VerifiedApps.txt

(one token per line if more apps are added later) and press Verify. The
app then shows the verified badge with AddLayer as developer.

## Updates: FLATHUB_TOKEN

After the first submission, each release reaches Flathub by itself:
`.github/workflows/flathub.yml` opens a PR on `flathub/io.addlayer.dbine`
with the new tag. It needs the repository secret **`FLATHUB_TOKEN`**
(Settings › Secrets and variables › Actions):

- a fine-grained personal access token of the account with write access to
  `flathub/io.addlayer.dbine` (resource owner: flathub, repository:
  io.addlayer.dbine), with **Contents: read and write** and **Pull
  requests: read and write**. If Flathub's organization doesn't allow
  fine-grained tokens, a classic token with the `public_repo` scope.

Without the secret, the workflow still renders and lints and keeps the
files as an artifact, with a notice that no PR was opened.

Flathub builds the PR; merging it publishes the update to users (Flatpak
updates the app; DBine doesn't check for updates in this build).

Optional: to have Google Drive and OneDrive sync in the Flathub build, set
the repository **variables** `DBINE_GOOGLE_CLIENT_ID`,
`DBINE_GOOGLE_CLIENT_SECRET` and `DBINE_MICROSOFT_CLIENT_ID`. They end up in
the public manifest (they are also inside every published binary); without
them the Flathub build has no Google Drive or OneDrive client configured
(folder sync works), like any build without them (`docs/sync.md`).
