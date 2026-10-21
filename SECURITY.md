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

While a finding from the source-code review is open, the page shows only its
severity, kind and component: publishing how to exploit it before it's
fixed would put users at risk.

## How DBine is checked

| Check | What it covers | When | Blocks |
|---|---|---|---|
| [Trivy](https://github.com/aquasecurity/trivy) | Known vulnerabilities in the Rust and npm dependencies (`Cargo.lock`, lockfiles), committed secrets, insecure configuration | Every push to `main`, every pull request and weekly | A CRITICAL or HIGH finding with a fix available fails the check |
| claude-security | A source-code security review: credentials, SQL that reaches a server, the driver protocol, files on disk, networking and the update path. Every finding is checked by an independent panel before it's reported | Before every release, on what the release adds since the previous one | A CRITICAL or HIGH finding blocks the release until it's fixed and scanned again |
| [OpenSSF Scorecard](https://scorecard.dev/viewer/?uri=github.com/addlayer-io/dbine) | The repository's security practices | Every push to `main` and weekly | Informational |

Trivy and Scorecard update the status page by themselves. A claude-security
release scan is added to it when it runs (`scripts/security/status.py`).
Trivy's known false positives are in [`trivy-secret.yaml`](trivy-secret.yaml),
each with the reason, and are listed on the status page too.

## Reporting a vulnerability

Don't open a public issue. Report it privately from the repository's
**Security** tab (**Report a vulnerability**), with the details and a way to
reproduce it. We answer there.
