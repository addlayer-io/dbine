"""Sign one Windows binary for the release build (Tauri's
bundle.windows.signCommand runs it with the file's path).

The certificate is Certum's cloud code-signing certificate (SimplySign),
used through `ssign`, which reads CERTUM_EMAIL and CERTUM_OTP from the
environment. Executables (the app, the NSIS installer and uninstaller) are
signed; ssign can't sign MSI packages yet, so those are left as they are
with a warning.

ssign writes the signed copy to a temporary folder and this script copies
it over the original: on Windows, replacing a file by renaming another over
it is denied while something (the antivirus) has it open.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time

path = sys.argv[1]
if path.lower().endswith((".msi", ".msp", ".msm")):
    print(f"::warning::{path}: not signed (ssign signs executables only)")
    sys.exit(0)


def sign(out_dir):
    cmd = ["ssign", "-n", "DBine", "-u", "https://dbine.com", "-o", out_dir, path]
    for attempt in range(1, 4):
        if subprocess.run(cmd).returncode == 0:
            return os.path.join(out_dir, os.path.basename(path))
        print(f"ssign failed on {path} (attempt {attempt}/3)", file=sys.stderr)
        # A one-time code is accepted once: wait for the next one.
        time.sleep(31)
    sys.exit(1)


def copy_over(signed):
    for attempt in range(1, 11):
        try:
            shutil.copyfile(signed, path)
            return
        except OSError as e:
            print(f"copying the signed file over {path} failed: {e} (attempt {attempt}/10)", file=sys.stderr)
            time.sleep(3)
    sys.exit(1)


with tempfile.TemporaryDirectory() as out_dir:
    copy_over(sign(out_dir))
