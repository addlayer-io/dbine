#!/bin/sh
# Cargo runner for macOS development builds (see .cargo/config.toml).
#
# Development binaries are signed ad hoc, and each build gets a new identity,
# so the keychain asks again for every saved password after every rebuild
# ("Always Allow" only lasts until the next build). Signing the app binary
# with a stable local identity and a fixed identifier keeps the keychain's
# trust across rebuilds.
#
# One-time setup on a machine: a code-signing certificate named "DBine Dev"
# in the login keychain (self-signed is enough). Without it, or for any
# binary other than the app (tests, examples), this just runs the binary.

bin="$1"
if [ "$(basename "$bin")" = "dbine" ] && security find-identity -p codesigning 2>/dev/null | grep -q '"DBine Dev"'; then
  codesign -f -s "DBine Dev" --identifier com.addlayer.dbine "$bin" >/dev/null 2>&1 \
    || echo "dev-sign-run: no se pudo firmar $bin con «DBine Dev»; sigue sin firmar" >&2
fi
exec "$@"
