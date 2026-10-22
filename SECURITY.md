# Security

DBine holds database credentials and runs SQL on production servers, so a
release goes out only when it has no known CRITICAL or HIGH vulnerability.

## Current status

**[Security status](https://github.com/addlayer-io/dbine/blob/security-status/STATUS.md)**
lists every finding open right now, from every tool below, what the last
scan of each one covered and what it didn't, and the findings judged not to
be a vulnerability, with the reason. A fixed finding leaves the page. The
**security** badge in the README counts the open findings, coloured by the
worst one.

While a finding from the source-code review or CodeQL is open, the page shows only its
severity, kind and component: publishing how to exploit it before it's
fixed would put users at risk.

## How DBine is checked

| Check | What it covers | When | Blocks |
|---|---|---|---|
| [Trivy](https://github.com/aquasecurity/trivy) | Known vulnerabilities in the Rust and npm dependencies (`Cargo.lock`, lockfiles), committed secrets, insecure configuration | Every push to `main`, every pull request and weekly | A CRITICAL or HIGH finding with a fix available fails the check |
| claude-security | A source-code security review: credentials, SQL that reaches a server, the driver protocol, files on disk, networking and the update path. Every finding is checked by an independent panel before it's reported | Before every release, on what the release adds since the previous one | A CRITICAL or HIGH finding blocks the release until it's fixed and scanned again |
| [OSV-Scanner](https://google.github.io/osv-scanner/) | Advisories for every dependency in every lockfile, from the OSV database (RustSec, GitHub advisories); the same data Scorecard uses | Every push to `main` and weekly | Its open findings count on the status page |
| [CodeQL](https://codeql.github.com) | Static analysis of the Rust, TypeScript/Vue and workflow code | Every push to `main`, every pull request and weekly | Informational; its open alerts count on the status page |
| [OpenSSF Scorecard](https://scorecard.dev/viewer/?uri=github.com/addlayer-io/dbine) | The repository's security practices | Every push to `main` and weekly | Informational |

Dependabot proposes dependency updates every week. OSV-Scanner, Trivy,
CodeQL and Scorecard update the status page by themselves. A
claude-security release scan is added to it when it runs
(`scripts/security/status.py`).

What was reviewed and found not to affect DBine is in the repository, each
entry with how it was checked: dependency advisories in
[`osv-scanner.toml`](osv-scanner.toml) and secret-scan false positives in
[`trivy-secret.yaml`](trivy-secret.yaml). The status page lists them too.

## Verify a download

Every installer and update package on the
[releases page](https://github.com/addlayer-io/dbine/releases) has a `.sig`
file next to it: a [minisign](https://jedisct1.github.io/minisign/) signature
made with DBine's release key, the same key the app checks before installing
an update. Its public half:

```
untrusted comment: minisign public key: C63E4C5716A166AD
RWStZqEWV0w+xjv1h6D6ZEWuaqpzCJ07eWEVorK2vhz+nitUvDtjwMSn
```

It's also in the app's configuration (`plugins.updater.pubkey` in
[`src-tauri/tauri.conf.json`](src-tauri/tauri.conf.json), base64-encoded).

To check a file (here the Linux `.deb`): save the two lines above as
`dbine.pub`, download the file and its `.sig`, and run:

```
base64 -d < DBine_1.1.0_amd64.deb.sig > DBine_1.1.0_amd64.deb.minisig
minisign -Vm DBine_1.1.0_amd64.deb -x DBine_1.1.0_amd64.deb.minisig -p dbine.pub
```

The `.sig` files are base64-encoded minisign signatures, hence the first
line. A good file prints `Signature and comment signature verified` and a
trusted comment with the file name and version.

Platform signatures come on top of that: the macOS app is signed with an
Apple Developer ID and notarized by Apple, and the Windows installer and app
are Authenticode-signed with a timestamp.

## Supported versions

Fixes go into the latest release; DBine updates itself, so that's the
version to run. Older versions don't get security fixes.

## Reporting a vulnerability

Don't open a public issue. Report it privately through GitHub:
<https://github.com/addlayer-io/dbine/security/advisories/new> (the
repository's **Security** tab, **Report a vulnerability**). Include the
version, the steps to reproduce it and what an attacker gets.

What happens next:

1. We confirm we received it within 3 business days.
2. We assess it and tell you the severity we give it and the plan.
3. A CRITICAL or HIGH vulnerability is fixed in a release as soon as it's
   ready, ideally within 30 days; lower severities go into a following
   release. The finding shows on the status page meanwhile, without the
   details.
4. Once the fix is released, we publish a GitHub security advisory (with a
   CVE when it applies) and credit you, unless you prefer otherwise.

Please keep the details private until the fixed version is out.
