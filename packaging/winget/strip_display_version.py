#!/usr/bin/env python3
"""Drop `DisplayVersion` lines equal to the package version from a rendered
winget manifest folder (`komac update` copies them over). winget's bots ask
for that: the same value as PackageVersion is redundant.

    python3 packaging/winget/strip_display_version.py <manifests/.../X.Y.Z>
"""

import pathlib
import re
import sys

folder = pathlib.Path(sys.argv[1])
version = folder.name
line = re.compile(r"^\s*DisplayVersion:\s*['\"]?" + re.escape(version) + r"['\"]?\s*$\n?", re.M)
for f in folder.glob("*.installer.yaml"):
    text = f.read_text()
    new = line.sub("", text)
    # An AppsAndFeaturesEntries item left with nothing but its dash goes too.
    new = re.sub(r"^\s*AppsAndFeaturesEntries:\n(\s*-\s*\n)+(?=\S)", "", new, flags=re.M)
    if new != text:
        f.write_text(new)
        print(f"{f.name}: DisplayVersion {version} removed")
