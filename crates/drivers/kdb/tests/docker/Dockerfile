# syntax=docker/dockerfile:1
# kdb+ (KDB-X) for the live tests: KX's own installer, with the portal
# token and the license (KX_PORTAL_TOKEN and KX_LICENSE_B64 in .env) as
# build secrets. The license is checked by the installer and removed: the
# container gets it again when it starts. q runs as an unprivileged user.
# Steps: tests/live.rs.
FROM ubuntu:26.04@sha256:6a06111c3b0a8600feadb270aec771dc239bf407b25625f93f44ab00e11f4d3c
RUN apt-get update && apt-get install -y --no-install-recommends curl unzip ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN --mount=type=secret,id=kxtoken --mount=type=secret,id=kxlic \
    curl -fsSLo /tmp/install_kdb.sh --oauth2-bearer "$(cat /run/secrets/kxtoken)" \
      https://portal.dl.kx.com/assets/raw/kdb-x/install_kdb/~latest~/install_kdb.sh \
 && bash /tmp/install_kdb.sh -y --b64lic "$(cat /run/secrets/kxlic)" \
 && find / -xdev \( -name kc.lic -o -name k4.lic \) -delete 2>/dev/null; rm -f /tmp/install_kdb.sh \
 && mv /root/.kx /opt/kx
RUN useradd --create-home --uid 10001 q \
 && printf 'dbine:dbinepass\n' > /home/q/users.txt \
 && mkdir -p /home/q/lic && chown -R q:q /opt/kx /home/q
ENV QHOME=/opt/kx QLIC=/home/q/lic
USER q
WORKDIR /home/q
EXPOSE 5000
HEALTHCHECK --interval=10s --timeout=3s --retries=6 CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/5000'
CMD echo "$KX_LICENSE_B64" | base64 -d > /home/q/lic/kc.lic && unset KX_LICENSE_B64 \
 && exec /opt/kx/bin/q -p 5000 -u /home/q/users.txt
