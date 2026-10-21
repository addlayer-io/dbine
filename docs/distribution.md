# Distribution channels

Every release is published on GitHub, and from there four channels pick it
up. Each channel is a workflow that runs after the release; none of them
builds the app again except Flathub, which builds from source on its own
infrastructure.

| Channel | Package | Workflow | What it publishes | Secret |
|---|---|---|---|---|
| GitHub Releases | `vX.Y.Z` release | `release.yml` | The installers, signatures and `latest.json` for the in-app updater. | none (GITHUB_TOKEN) |
| Flathub | `io.addlayer.dbine` | `flathub.yml` | A PR on `flathub/io.addlayer.dbine`; Flathub builds and publishes on merge. | `FLATHUB_TOKEN` |
| winget | `AddLayer.DBine` | `winget.yml` | A PR on `microsoft/winget-pkgs` pointing to the NSIS setup. | `WINGET_TOKEN` |
| AUR | `dbine-bin` | `aur.yml` | A push to `aur.archlinux.org/dbine-bin.git` (the release's `.deb`, repackaged). | `AUR_SSH_PRIVATE_KEY`, `AUR_USERNAME`, `AUR_EMAIL` |

## How the channel workflows start

Flathub, winget and the AUR share the same triggers:

- **After `release`:** a release that the `release` workflow publishes uses
  GITHUB_TOKEN, and GitHub starts no other workflow from that event. The
  channels listen to `workflow_run` instead and go on when `release` ended
  well on a `v*` tag pushed to the repository.
- **After a local release:** a release published with a personal token
  (`gh release create`) starts them through the `release: published`
  event. Pre-releases are skipped.
- **By hand:** Actions › *channel* › Run workflow, with the tag. `dry_run`
  is on by default: the run does everything except publishing and keeps
  the files as an artifact.

A missing secret never fails a run: the files are rendered, checked and kept
as an artifact, and the publish step ends with a notice that says which
secret is missing.

## Dry runs

```sh
gh workflow run winget.yml  --repo addlayer-io/dbine -f tag=v0.1.11            # dry_run defaults to true
gh workflow run aur.yml     --repo addlayer-io/dbine -f tag=v0.1.11
gh workflow run flathub.yml --repo addlayer-io/dbine -f tag=v0.1.11
gh run download --repo addlayer-io/dbine -n winget-v0.1.11   # or aur-v0.1.11, flathub-v0.1.11
```

To publish from a manual run, add `-f dry_run=false`.

## GitHub Releases

Automatic: pushing a `vX.Y.Z` tag builds every platform and publishes the
release (`release.yml`, [`updates.md`](updates.md)). The assets the
channels use:

| Asset | Used by |
|---|---|
| `DBine_X.Y.Z_x64-setup.exe` (NSIS) | winget, the Windows updater |
| `DBine_X.Y.Z_x64_en-US.msi` | direct download |
| `DBine_X.Y.Z_amd64.deb` | the AUR package |
| `DBine_X.Y.Z_amd64.AppImage`, `DBine-X.Y.Z-1.x86_64.rpm` | direct download, the Linux updater (AppImage) |

## Flathub

See [`flathub.md`](flathub.md) and the first-submission checklist in
[`flatpak/README.md`](../flatpak/README.md). Secret: `FLATHUB_TOKEN`, a
fine-grained token with Contents and Pull requests (read and write) on
`flathub/io.addlayer.dbine`.

## winget

The package is `AddLayer.DBine` (publisher AddLayer, Apache-2.0, homepage
https://dbine.com, moniker `dbine`). Its installer is the release's NSIS
setup: `InstallerType: nullsoft`, `Scope: user`, because Tauri's NSIS
installer is per-user by default (`tauri.conf.json` doesn't set
`bundle.windows.nsis.installMode`) and installs into
`%LocalAppData%\DBine`. winget can then upgrade it, and so can DBine's own
updater: both run the same setup, which replaces the installed version.

### What the workflow does

- **Tool:** [Komac](https://github.com/russellbanks/Komac) 2.16.0, the
  release binary for Linux, verified by its SHA-256 in `winget.yml`.
- **State:** it asks `microsoft/winget-pkgs` for
  `manifests/a/AddLayer/DBine`. Missing: first submission. Present without
  this version: update. Version already there: nothing to do.
- **First submission:** `komac new` keeps asking questions (install modes,
  and more) even with every field given as a flag, so it can't run
  unattended. Instead, `packaging/winget/render.py` writes the three
  manifests (version, installer, defaultLocale) from `komac analyze` of the
  setup (SHA-256, ProductCode, the Apps & Features entry, the install
  location) and from the Flathub metainfo (summary and description), with
  the tags, the license and the release notes URL; then `komac submit`
  opens the PR. Nothing is manual beyond the one-time steps below.
- **Later versions:** `komac update AddLayer.DBine --version X.Y.Z --urls
  <setup> --release-notes-url <release>`. It carries over the published
  manifests, including any change winget's moderators made, and only
  changes the version, the installer and its hash.
- **Checks before sending:** the manifests are validated against winget's
  JSON schemas 1.12.0 (from `microsoft/winget-cli` at a pinned commit),
  the installer URL and architecture are checked, and `komac submit
  --dry-run` parses them. `winget validate` only exists on Windows; the
  winget-pkgs pipeline runs it, with an install test in a sandbox, on the
  PR.
- **Re-runs:** if a PR for the same version is already open, it isn't
  opened again.

### Secret: `WINGET_TOKEN`

A classic personal access token with the `public_repo` scope, from the
account that will appear as the author of the PRs. Komac forks
`microsoft/winget-pkgs` into that account the first time, pushes a branch
there and opens the PR from it.

### Owner's one-time steps

1. Pick the GitHub account that will submit (your own, or a bot account
   you control).
2. Create the token: GitHub › Settings › Developer settings › Personal
   access tokens › Tokens (classic) › Generate new token (classic). Scope:
   `public_repo` only. Choose an expiration and note it: with an expired
   token the PR step fails with an authentication error, and the secret
   needs a new token.
3. Store it: `gh secret set WINGET_TOKEN --repo addlayer-io/dbine` (paste
   the token when asked).
4. First submission: `gh workflow run winget.yml --repo addlayer-io/dbine
   -f tag=v<latest> -f dry_run=false`, or wait for the next release.
5. On that first PR, the Microsoft CLA bot asks for the contributor
   license agreement: reply with `@microsoft-github-policy-service agree`
   (or `@microsoft-github-policy-service agree company="AddLayer"`). Only
   once per account.
6. Follow the PR until a moderator merges it. New packages are reviewed by
   hand; the installers are not signed yet, which winget accepts, but a
   Defender or SmartScreen flag in the validation pipeline is answered on
   the PR. From then on, every release opens its PR by itself.

## AUR

The package is `dbine-bin` (x86_64; provides and conflicts with `dbine`).
It repackages the release's `.deb`: makepkg downloads it, the PKGBUILD
extracts `data.tar.*` and installs

- `/usr/bin/dbine`;
- `/usr/share/applications/dbine.desktop`;
- the icons under `/usr/share/icons/hicolor/<size>/apps/dbine.png` (32,
  128, 256 and 512; the bundler names the 256 folder `256x256@2`, the
  PKGBUILD puts it back in `256x256`);
- `/usr/share/licenses/dbine-bin/LICENSE`, when the repository has a
  `LICENSE` file (see below).

The `.deb` was chosen over the AppImage because the AppImage updates
itself, which would fight pacman. With the `.deb`'s binary, DBine's
updater finds no `APPIMAGE` variable and only offers the release page
([`updates.md`](updates.md)); AUR users update with their AUR helper.

**Dependencies:** `depends` lists the Arch packages that own the shared
libraries `/usr/bin/dbine` links (`webkit2gtk-4.1`, `gtk3`, `libsoup3`,
`glib2`, `gdk-pixbuf2`, `cairo`, `dbus`, `zlib`, `libgcc`, `glibc`) plus
`hicolor-icon-theme`. `render.py` reads the `.deb`'s `Depends` field
(`libwebkit2gtk-4.1-0, libgtk-3-0`), maps it to Arch names and fails if the
PKGBUILD misses one, so a new library in a future `.deb` is caught.
`optdepends`: a Secret Service provider (`org.freedesktop.secrets`, e.g.
gnome-keyring or KeePassXC) for the key that encrypts saved passwords,
`git` for projects and the script library, `unixodbc` for ODBC.

### Files

| File | What it is |
|---|---|
| `packaging/aur/PKGBUILD.in` | The PKGBUILD template (`@VERSION@`, `@PKGREL@`, `@DEB_SHA256@`, ...). |
| `packaging/aur/render.py` | Renders the PKGBUILD for one tag from the downloaded `.deb` (its SHA-256 and Depends). |
| `packaging/aur/check.sh` | In an Arch container: `.SRCINFO` (`makepkg --printsrcinfo`), `namcap` on the PKGBUILD and the package, build, install, and `ldd /usr/bin/dbine`. |
| `.github/workflows/aur.yml` | Runs both in `archlinux:base-devel`, keeps the artifact and pushes. |

Every run builds and checks the package before pushing; a namcap error or a
library `ldd` can't resolve stops it. The push uses plain `git` over SSH
with aur.archlinux.org's Ed25519 host key pinned in the workflow (its
fingerprint `SHA256:RFzBCUItH9LZS0cKB5UE6ceAYhBD5C8GeOBip8Z11+4` is the one
published on the AUR's home page). To republish the same version after a
packaging fix, run the workflow by hand with a higher `pkgrel`.

### Secrets

- `AUR_SSH_PRIVATE_KEY`: the private half of an SSH key registered on the
  AUR account.
- `AUR_USERNAME`, `AUR_EMAIL`: the commit identity, also the PKGBUILD's
  `# Maintainer:` line (the email is public in the AUR's git history).

### Owner's one-time steps

1. Create an AUR account: https://aur.archlinux.org/register (user name,
   email). `dbine-bin` and `dbine` are free today.
2. Make a key just for this:
   `ssh-keygen -t ed25519 -N '' -C 'dbine aur workflow' -f aur_dbine`.
3. On the AUR: My Account › SSH Public Key, paste the content of
   `aur_dbine.pub`, confirm with your password, Update.
4. Store the secrets:
   ```sh
   gh secret set AUR_SSH_PRIVATE_KEY --repo addlayer-io/dbine < aur_dbine
   gh secret set AUR_USERNAME --repo addlayer-io/dbine --body '<aur user name>'
   gh secret set AUR_EMAIL --repo addlayer-io/dbine --body '<email>'
   ```
   Then delete `aur_dbine` from disk.
5. The package ships the repository's `LICENSE` (Apache 2.0) and `NOTICE`
   files, as Apache 2.0 requires.
6. First push: `gh workflow run aur.yml --repo addlayer-io/dbine
   -f tag=v<latest> -f dry_run=false`, or wait for the next release. The
   push creates https://aur.archlinux.org/packages/dbine-bin. There is no
   review.

## Checking locally

```sh
# winget: the manifests of a release (needs komac and PyYAML)
komac analyze --hash DBine_0.1.11_x64-setup.exe > analysis.yaml
python3 packaging/winget/render.py --tag v0.1.11 --analysis analysis.yaml \
  --release-date 2026-10-13 --out /tmp/winget

# AUR: render, then build and check in an Arch container
python3 packaging/aur/render.py --tag v0.1.11 --deb DBine_0.1.11_amd64.deb \
  --maintainer 'Name <email>' --out /tmp/aur
docker run --rm --name dbine-test-aur --platform linux/amd64 \
  -v "$PWD/packaging:/src:ro" -v /tmp/aur:/out archlinux:base-devel \
  bash -c 'sed -i "s/^#DisableSandbox/DisableSandbox/" /etc/pacman.conf; /src/aur/check.sh /out'
```

On an Apple Silicon Mac the Arch image runs emulated, and pacman's sandbox
has to be disabled (the `sed` above); the workflow does the same.
