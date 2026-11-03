# syntax=docker/dockerfile:1
# kdb+ (KDB-X) for the live tests: KX's own installer, with the portal
# token and the license (KX_PORTAL_TOKEN and KX_LICENSE_B64 in .env) as
# build secrets. The license is checked by the installer and removed: the
# container gets it again when it starts. Steps: tests/live.rs.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends curl unzip ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN --mount=type=secret,id=kxtoken --mount=type=secret,id=kxlic \
    curl -fsSLo /tmp/install_kdb.sh --oauth2-bearer "$(cat /run/secrets/kxtoken)" \
      https://portal.dl.kx.com/assets/raw/kdb-x/install_kdb/~latest~/install_kdb.sh \
 && bash /tmp/install_kdb.sh -y --b64lic "$(cat /run/secrets/kxlic)" \
 && find / -xdev \( -name kc.lic -o -name k4.lic \) -delete 2>/dev/null; rm -f /tmp/install_kdb.sh
RUN printf 'dbine:dbinepass\n' > /users.txt
ENV QHOME=/root/.kx QLIC=/lic
WORKDIR /root
CMD mkdir -p /lic && echo "$KX_LICENSE_B64" | base64 -d > /lic/kc.lic && unset KX_LICENSE_B64 \
 && exec /root/.kx/bin/q -p 5000 -u /users.txt
