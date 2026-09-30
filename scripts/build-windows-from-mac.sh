#!/usr/bin/env bash
# Build DBine's Windows executable (x86_64, MSVC) from macOS with cargo-xwin.
# Output: target/x86_64-pc-windows-msvc/release/dbine.exe (no installer: the
# .msi / -setup.exe are built on Windows or by the release workflow).
#
# Needs: brew install llvm · cargo install cargo-xwin
#        rustup target add x86_64-pc-windows-msvc
set -euo pipefail
cd "$(dirname "$0")/.."

for tool in cargo-xwin; do
  command -v "$tool" >/dev/null || { echo "falta $tool (ver el encabezado de este script)"; exit 1; }
done
LLVM="$(brew --prefix llvm)/bin"
[ -x "$LLVM/clang-cl" ] || { echo "falta LLVM: brew install llvm"; exit 1; }

# cargo-xwin links with rustup's rust-lld as `lld-link`; called from a C
# build script it can't find libLLVM.dylib. A wrapper that points it
# there.
TOOLCHAIN="$(rustc --print sysroot)"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
XWIN_CACHE="${XWIN_CACHE_DIR:-$HOME/Library/Caches/cargo-xwin}"
mkdir -p "$XWIN_CACHE"
if [ -L "$XWIN_CACHE/lld-link" ] || [ ! -e "$XWIN_CACHE/lld-link" ]; then
  rm -f "$XWIN_CACHE/lld-link"
  cat > "$XWIN_CACHE/lld-link" <<WRAP
#!/bin/sh
export DYLD_FALLBACK_LIBRARY_PATH="$TOOLCHAIN/lib\${DYLD_FALLBACK_LIBRARY_PATH:+:\$DYLD_FALLBACK_LIBRARY_PATH}"
exec -a lld-link "$TOOLCHAIN/lib/rustlib/$HOST/bin/rust-lld" "\$@"
WRAP
  chmod +x "$XWIN_CACHE/lld-link"
fi

PATH="$LLVM:$PATH" \
XWIN_ACCEPT_LICENSE=1 WIN_ACCEPT_LICENSE=1 \
  cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle "$@"

ls -la target/x86_64-pc-windows-msvc/release/dbine.exe
