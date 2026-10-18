"""Sign one Windows binary for the release build (Tauri's
bundle.windows.signCommand runs it with the file's path).

The certificate is Certum's cloud code-signing certificate (SimplySign),
used through `ssign`, which reads CERTUM_EMAIL and CERTUM_OTP from the
environment. Executables (the app, the NSIS installer and uninstaller) are
signed; ssign can't sign MSI packages yet, so those are left as they are
with a warning.
"""

import subprocess
import sys
import time

path = sys.argv[1]
if path.lower().endswith((".msi", ".msp", ".msm")):
    print(f"::warning::{path}: not signed (ssign signs executables only)")
    sys.exit(0)

cmd = ["ssign", "-n", "DBine", "-u", "https://dbine.com", path]
for attempt in range(1, 4):
    if subprocess.run(cmd).returncode == 0:
        sys.exit(0)
    print(f"ssign failed on {path} (attempt {attempt}/3)", file=sys.stderr)
    # A one-time code is accepted once: wait for the next one.
    time.sleep(31)
sys.exit(1)
