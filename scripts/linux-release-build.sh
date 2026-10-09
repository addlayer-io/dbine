#!/usr/bin/env bash
# DBine's Linux release build (drivers, .deb, .rpm and the signed AppImage),
# inside the x86_64 Docker build container, with the release worktree
# mounted at /src and the updater key mounted read-only:
#
#   docker run --rm --platform linux/amd64 \
#     -v "$PWD:/src" \
#     -v "$HOME/.tauri/dbine-updater.key:/run/secrets/dbine-updater.key:ro" \
#     <build image> bash /src/scripts/linux-release-build.sh
#
# The key's password and the cloud client IDs come from /src/.env (never
# printed). Output: linux-out/ (installers, the AppImage and its .sig).
# docs/updates.md, .claude/agents/release.md.
set -euo pipefail
cd /src
# Kerberos (GSSAPI) headers for SQL Server's and MongoDB's Windows / Kerberos
# authentication (libgssapi-sys runs bindgen over them). Images built before
# it was needed don't have them.
if ! pkg-config --exists mit-krb5-gssapi; then
  apt-get update -qq && apt-get install -y -qq --no-install-recommends libkrb5-dev
fi
export DBINE_DRIVER_JOBS="${DBINE_DRIVER_JOBS:-1}"
python3 scripts/build-driver-hosts.py x86_64-unknown-linux-gnu driver-hosts-linux \
  "https://github.com/addlayer-io/dbine/releases/download/drivers" \
  driver-index-linux/index-x86_64-unknown-linux-gnu.json
echo DRIVERS-DONE
export DBINE_PLUGIN_CATALOG=/src/driver-hosts-linux/plugins.json
set -a; . ./.env; set +a
# The host's path doesn't exist in here: the key is mounted at this one.
export TAURI_SIGNING_PRIVATE_KEY=/run/secrets/dbine-updater.key
scripts/check-updater-signing.sh
npm ci --prefix web
cargo tauri build --features plugins --config src-tauri/tauri.updater.conf.json
mkdir -p linux-out
B=target/release/bundle
cp "$B"/deb/*.deb "$B"/rpm/*.rpm "$B"/appimage/*.AppImage "$B"/appimage/*.AppImage.sig linux-out/
echo APP-DONE
