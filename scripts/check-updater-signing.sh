#!/usr/bin/env bash
# Before a release build: can it sign the updater packages? Checks, without
# printing any value, that
# - the updater public key in src-tauri/tauri.conf.json isn't the placeholder;
# - TAURI_SIGNING_PRIVATE_KEY (from the environment or the repo's .env) is an
#   absolute path to a readable file, or the key's content;
# - TAURI_SIGNING_PRIVATE_KEY_PASSWORD is set (empty is fine): without it
#   the Tauri CLI asks for it interactively and the build hangs.
# docs/actualizaciones.md.
set -euo pipefail
cd "$(dirname "$0")/.."

# .env fills in what the environment doesn't set (in the Linux container the
# key's path is exported over the host's one in .env).
if [ -f .env ]; then
  env_key="${TAURI_SIGNING_PRIVATE_KEY-}"
  set -a; . ./.env; set +a
  if [ -n "$env_key" ]; then TAURI_SIGNING_PRIVATE_KEY="$env_key"; fi
fi

fail() { echo "error: $*" >&2; exit 1; }

python3 - <<'PY' || exit 1
import json, sys
key = json.load(open("src-tauri/tauri.conf.json"))["plugins"]["updater"]["pubkey"].strip()
if not key or key == "DBINE_UPDATER_PUBKEY_PLACEHOLDER":
    sys.exit("error: src-tauri/tauri.conf.json still has the placeholder updater pubkey")
PY

key="${TAURI_SIGNING_PRIVATE_KEY:-}"
[ -n "$key" ] || fail "TAURI_SIGNING_PRIVATE_KEY isn't set (an absolute path to the key, or its content)"
case "$key" in
  /*) [ -r "$key" ] || fail "TAURI_SIGNING_PRIVATE_KEY points to a file that doesn't exist or can't be read" ;;
  "~"*|./*|../*) fail "TAURI_SIGNING_PRIVATE_KEY must be an absolute path (the Tauri CLI doesn't expand ~ or relative paths)" ;;
  *) [ "${#key}" -gt 100 ] || fail "TAURI_SIGNING_PRIVATE_KEY is neither an absolute path nor a key's content" ;;
esac
[ -n "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD+x}" ] || fail "TAURI_SIGNING_PRIVATE_KEY_PASSWORD isn't set (set it, empty if the key has no password)"
echo "updater signing: ready"
