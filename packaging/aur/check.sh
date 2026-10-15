#!/usr/bin/env bash
# Build and check a rendered dbine-bin package (docs/distribution.md).
#
#   packaging/aur/check.sh <dir>
#
# Runs as root inside an archlinux:base-devel container, with <dir> holding
# the PKGBUILD that packaging/aur/render.py wrote. It writes <dir>/.SRCINFO,
# lints the PKGBUILD and the package with namcap, builds and installs the
# package, and checks that /usr/bin/dbine resolves every shared library.
# namcap's findings are printed; errors (E:) fail the check, except the
# missing license file while the repository has no LICENSE (a warning).
set -euo pipefail

dir=$(realpath "$1")

pacman -Syu --noconfirm --needed namcap >/dev/null

# makepkg doesn't run as root: a throwaway build user.
id builder >/dev/null 2>&1 || useradd -m builder
echo 'builder ALL=(ALL) NOPASSWD: /usr/bin/pacman' > /etc/sudoers.d/builder
work=$(mktemp -d)
cp "$dir"/PKGBUILD "$work"/
[ -f "$dir/LICENSE" ] && cp "$dir/LICENSE" "$work"/
chown -R builder: "$work"

cd "$work"
# shellcheck disable=SC2024 # root writes the file, on purpose
sudo -u builder makepkg --printsrcinfo > "$dir/.SRCINFO"
cp "$dir/.SRCINFO" "$work/.SRCINFO"
echo "::group::.SRCINFO"
cat "$dir/.SRCINFO"
echo "::endgroup::"

fail=0
lint() {
  local out
  out=$(namcap "$@" 2>&1 || true)
  [ -n "$out" ] && echo "$out"
  if [ ! -f "$dir/LICENSE" ]; then
    grep -q 'require license files' <<<"$out" &&
      echo "::warning::no LICENSE in the repository: the package ships no license file (docs/distribution.md)"
    out=$(grep -v 'require license files' <<<"$out" || true)
  fi
  if grep -q ' E: ' <<<"$out"; then fail=1; fi
}

echo "namcap PKGBUILD:"
lint PKGBUILD

sudo -u builder makepkg --syncdeps --noconfirm --cleanbuild
pkgs=(dbine-bin-[0-9]*.pkg.tar.*)
pkg=${pkgs[0]}
echo "namcap $pkg:"
lint "$pkg"

pacman -U --noconfirm "$pkg" >/dev/null
pacman -Qi dbine-bin | sed -n '1,/^Description/p'
pacman -Ql dbine-bin | grep -v '/$'
missing=$(ldd /usr/bin/dbine | grep 'not found' || true)
if [ -n "$missing" ]; then
  echo "::error::/usr/bin/dbine is missing libraries:"; echo "$missing"; fail=1
else
  n=$(ldd /usr/bin/dbine | grep -c ' => ')
  echo "ldd: /usr/bin/dbine resolves all $n shared libraries"
fi
exit "$fail"
