#!/usr/bin/env python3
"""Render the winget manifests of one release (docs/distribution.md).

    komac analyze --hash DBine_X.Y.Z_x64-setup.exe > analysis.yaml
    python3 packaging/winget/render.py --tag vX.Y.Z --analysis analysis.yaml \
        --release-date YYYY-MM-DD --out <dir>

Writes <dir>/manifests/a/AddLayer/DBine/X.Y.Z/ with the three manifests
(version, installer, defaultLocale) in the layout of microsoft/winget-pkgs,
ready for `komac submit`. It is used for the package's first submission;
later versions go through `komac update`, which carries these fields over.

The installer fields (ProductCode, scope, Apps & Features entry, default
install location, SHA-256) come from komac's analysis of the NSIS setup.
The summary and description reuse the Flathub metainfo's wording.
Needs PyYAML.
"""

import argparse
import pathlib
import re
import sys
import xml.etree.ElementTree as ET

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[2]
METAINFO = ROOT / "flatpak" / "io.addlayer.dbine.metainfo.xml"

PACKAGE_ID = "AddLayer.DBine"
MANIFEST_VERSION = "1.12.0"
REPO = "https://github.com/addlayer-io/dbine"
TAGS = [
    "database", "database-manager", "sql", "sql-client", "nosql",
    "postgresql", "mysql", "mariadb", "sqlserver", "sqlite", "oracle",
    "mongodb", "redis", "clickhouse", "duckdb",
]


def schema_header(kind: str) -> str:
    return (
        "# yaml-language-server: $schema=https://aka.ms/winget-manifest."
        f"{kind}.{MANIFEST_VERSION}.schema.json\n\n"
    )


def flat(text: str) -> str:
    return re.sub(r"\s+", " ", text or "").strip()


def metainfo_text():
    """(summary, description) from the metainfo, as plain text."""
    root = ET.parse(METAINFO).getroot()
    summary = flat(root.findtext("summary"))
    parts = []
    for el in root.find("description"):
        if el.tag == "p":
            text = flat("".join(el.itertext()))
            # A short paragraph followed by a list is that list's heading.
            parts.append(text if len(text) > 60 else f"{text}:")
        elif el.tag == "ul":
            parts.append("\n".join(f"- {flat(''.join(li.itertext()))}" for li in el))
    description = "\n\n".join(parts)
    # A heading and its list go together, without a blank line between them.
    description = re.sub(r":\n\n- ", ":\n- ", description)
    return summary, description


def _str(dumper, s):
    style = "|" if "\n" in s else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", s, style=style)


yaml.SafeDumper.add_representer(str, _str)


def dump(path: pathlib.Path, kind: str, data: dict):
    path.write_text(
        schema_header(kind)
        + yaml.safe_dump(data, sort_keys=False, allow_unicode=True, width=1000),
        encoding="utf-8",
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", required=True)
    ap.add_argument("--analysis", required=True, help="output of `komac analyze --hash`")
    ap.add_argument("--release-date", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    m = re.fullmatch(r"v(\d+\.\d+\.\d+)", args.tag)
    if not m:
        sys.exit(f"'{args.tag}' is not a release tag (vX.Y.Z)")
    version = m.group(1)
    url = f"{REPO}/releases/download/{args.tag}/DBine_{version}_x64-setup.exe"

    a = yaml.safe_load(pathlib.Path(args.analysis).read_text())
    # What the manifests promise has to match the installer.
    if a.get("InstallerType") != "nullsoft":
        sys.exit(f"expected an NSIS installer, komac found {a.get('InstallerType')}")
    if a.get("Scope") != "user":
        sys.exit(f"expected a per-user NSIS installer, komac found scope {a.get('Scope')}")
    sha = a.get("InstallerSha256", "")
    if not re.fullmatch(r"[0-9A-Fa-f]{64}", sha):
        sys.exit("the analysis has no InstallerSha256 (run `komac analyze --hash`)")
    arp = a.get("AppsAndFeaturesEntries") or []
    for entry in arp:
        if str(entry.get("DisplayVersion")) != version:
            sys.exit(f"the setup's DisplayVersion is {entry.get('DisplayVersion')}, not {version}")

    out = pathlib.Path(args.out) / "manifests" / "a" / "AddLayer" / "DBine" / version
    out.mkdir(parents=True, exist_ok=True)
    common = {"PackageIdentifier": PACKAGE_ID, "PackageVersion": version}

    dump(out / f"{PACKAGE_ID}.yaml", "version", {
        **common,
        "DefaultLocale": "en-US",
        "ManifestType": "version",
        "ManifestVersion": MANIFEST_VERSION,
    })

    installer = {
        **common,
        "InstallerLocale": "en-US",
        "InstallerType": "nullsoft",
        "Scope": "user",
        "InstallModes": ["interactive", "silent", "silentWithProgress"],
        "UpgradeBehavior": "install",
        "ReleaseDate": args.release_date,
    }
    if a.get("ProductCode"):
        installer["ProductCode"] = str(a["ProductCode"])
    if arp:
        installer["AppsAndFeaturesEntries"] = arp
    if a.get("InstallationMetadata"):
        installer["InstallationMetadata"] = a["InstallationMetadata"]
    installer["Installers"] = [{
        # The NSIS stub is 32-bit; the app it installs is x64.
        "Architecture": "x64",
        "InstallerUrl": url,
        "InstallerSha256": sha.upper(),
    }]
    installer["ManifestType"] = "installer"
    installer["ManifestVersion"] = MANIFEST_VERSION
    dump(out / f"{PACKAGE_ID}.installer.yaml", "installer", installer)

    summary, description = metainfo_text()
    dump(out / f"{PACKAGE_ID}.locale.en-US.yaml", "defaultLocale", {
        **common,
        "PackageLocale": "en-US",
        "Publisher": "AddLayer",
        "PublisherUrl": "https://dbine.com",
        "PublisherSupportUrl": f"{REPO}/issues",
        "Author": "AddLayer",
        "PackageName": "DBine",
        "PackageUrl": "https://dbine.com",
        "License": "MIT",
        "ShortDescription": summary,
        "Description": description,
        "Moniker": "dbine",
        "Tags": TAGS,
        "ReleaseNotesUrl": f"{REPO}/releases/tag/{args.tag}",
        "ManifestType": "defaultLocale",
        "ManifestVersion": MANIFEST_VERSION,
    })
    print(out)


if __name__ == "__main__":
    main()
