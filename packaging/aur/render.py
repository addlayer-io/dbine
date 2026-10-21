#!/usr/bin/env python3
"""Render the AUR package dbine-bin for one release (docs/distribution.md).

    python3 packaging/aur/render.py --tag vX.Y.Z --deb DBine_X.Y.Z_amd64.deb \
        --maintainer 'Name <email>' --out <dir>

Writes <dir>/PKGBUILD from packaging/aur/PKGBUILD.in (and LICENSE when the
repository has one). The .deb is the release asset: its SHA-256 goes into
the PKGBUILD, and its Depends field is checked against the PKGBUILD's
depends, so a library the app starts needing is not missed. .SRCINFO is
made afterwards with `makepkg --printsrcinfo` (the workflow does it).
"""

import argparse
import hashlib
import io
import pathlib
import re
import sys
import tarfile
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parents[2]
TEMPLATE = ROOT / "packaging" / "aur" / "PKGBUILD.in"
METAINFO = ROOT / "flatpak" / "io.addlayer.dbine.metainfo.xml"
LICENSE = ROOT / "LICENSE"
NOTICE = ROOT / "NOTICE"

# Debian package -> Arch package, for every name the .deb's Depends may list.
DEB_TO_ARCH = {
    "libwebkit2gtk-4.1-0": "webkit2gtk-4.1",
    "libgtk-3-0": "gtk3",
    "libayatana-appindicator3-1": "libayatana-appindicator",
    "libappindicator3-1": "libappindicator-gtk3",
    "librsvg2-2": "librsvg",
    "libssl3": "openssl",
    "libxdo3": "xdotool",
}


def ar_members(data: bytes):
    """{name: bytes} of an ar archive (a .deb)."""
    if not data.startswith(b"!<arch>\n"):
        sys.exit("the .deb is not an ar archive")
    pos, out = 8, {}
    while pos + 60 <= len(data):
        header = data[pos:pos + 60]
        name = header[:16].decode().strip().rstrip("/")
        size = int(header[48:58].decode().strip())
        out[name] = data[pos + 60:pos + 60 + size]
        pos += 60 + size + (size & 1)
    return out


def deb_depends(data: bytes):
    members = ar_members(data)
    control = next((v for k, v in members.items() if k.startswith("control.tar")), None)
    if control is None:
        sys.exit("the .deb has no control archive")
    with tarfile.open(fileobj=io.BytesIO(control)) as tar:
        member = next(m for m in tar.getmembers() if m.name.lstrip("./") == "control")
        text = tar.extractfile(member).read().decode()
    m = re.search(r"^Depends:\s*(.+)$", text, re.M)
    if not m:
        return []
    # "a (>= 1), b | c" -> ["a", "b"]: the first alternative, without versions.
    return [re.split(r"[\s(|]", d.strip(), maxsplit=1)[0] for d in m.group(1).split(",")]


def pkgbuild_depends(text: str):
    m = re.search(r"^depends=\((.*?)\)", text, re.M | re.S)
    return set(re.findall(r"'([^']+)'", m.group(1))) if m else set()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", required=True)
    ap.add_argument("--deb", required=True)
    ap.add_argument("--maintainer", required=True)
    ap.add_argument("--pkgrel", default="1", help="raise it to republish the same version")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    m = re.fullmatch(r"v(\d+\.\d+\.\d+)", args.tag)
    if not m:
        sys.exit(f"'{args.tag}' is not a release tag (vX.Y.Z)")
    version = m.group(1)
    if not re.fullmatch(r"[1-9][0-9]*", args.pkgrel):
        sys.exit(f"pkgrel must be a positive integer, not '{args.pkgrel}'")

    deb = pathlib.Path(args.deb).read_bytes()
    text = TEMPLATE.read_text()

    have = pkgbuild_depends(text)
    for dep in deb_depends(deb):
        arch = DEB_TO_ARCH.get(dep)
        if arch is None:
            sys.exit(f"the .deb depends on {dep}, which has no Arch name in DEB_TO_ARCH")
        if arch not in have:
            sys.exit(f"the .deb depends on {dep}: add '{arch}' to depends in PKGBUILD.in")

    summary = ET.parse(METAINFO).getroot().findtext("summary").strip()
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    if LICENSE.is_file():
        # Apache 2.0: the license and the NOTICE file travel with the package.
        files = [f for f in (LICENSE, NOTICE) if f.is_file()]
        for f in files:
            (out / f.name).write_bytes(f.read_bytes())
        lic_source = " ".join(f"'{f.name}'" for f in files)
        lic_sum = " ".join(f"'{hashlib.sha256(f.read_bytes()).hexdigest()}'" for f in files)
        lic_install = "\n".join(
            f'  install -Dm644 "$srcdir/{f.name}" "$pkgdir/usr/share/licenses/$pkgname/{f.name}"' for f in files
        )
    else:
        print("warning: the repository has no LICENSE file; the package ships none", file=sys.stderr)
        lic_source = lic_sum = ""
        lic_install = ""

    values = {
        "@MAINTAINER@": args.maintainer,
        "@VERSION@": version,
        "@PKGREL@": args.pkgrel,
        "@SUMMARY@": summary.replace("'", "'\\''"),
        "@DEB_SHA256@": hashlib.sha256(deb).hexdigest(),
        "@LICENSE_SOURCE@": lic_source,
        "@LICENSE_SHA256@": lic_sum,
        "@LICENSE_INSTALL@": lic_install,
    }
    for key, value in values.items():
        text = text.replace(key, value)
    # Without a license file: no empty arrays and no empty line left behind.
    text = re.sub(r"^(source|sha256sums)=\(\)\n", "", text, flags=re.M)
    text = re.sub(r"\n\n}\n$", "\n}\n", text)
    left = re.findall(r"@[A-Z0-9_]+@", text)
    if left:
        sys.exit(f"placeholders left in the PKGBUILD: {left}")
    (out / "PKGBUILD").write_text(text)
    print(out / "PKGBUILD")


if __name__ == "__main__":
    main()
