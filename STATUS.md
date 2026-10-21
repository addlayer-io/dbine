# DBine security status

The security findings open in DBine right now, from every tool that looks for them. A finding leaves this page when it's fixed. How the checks work and how to report a vulnerability: [SECURITY.md](https://github.com/addlayer-io/dbine/blob/main/SECURITY.md).

_Updated 2026-10-21._

## Open findings

| Source | Critical | High | Medium | Low |
|---|---|---|---|---|
| claude-security (source-code review) | 0 | 0 | 0 | 0 |
| Trivy (dependencies, secrets, configuration) | 0 | 0 | 3 | 2 |

Findings from the source-code review show only their kind and component while they're open.

| ID | Severity | Source | Finding | Where | Fix |
|---|---|---|---|---|---|
| TV-CVE-2026-111061-russh | MEDIUM | trivy | russh 0.63.3: russh: russh: Denial of Service via resource exhaustion in SSH agent | `Cargo.lock` | upgrade to 0.64.1 |
| TV-GHSA-wrw7-89jp-8q8g-glib | MEDIUM | trivy | glib 0.18.5: Unsoundness in `Iterator` and `DoubleEndedIterator` impls for `glib::VariantStrIter` | `Cargo.lock` | upgrade to 0.20.0 |
| TV-GHSA-wrw7-89jp-8q8g-glib-9761a8 | MEDIUM | trivy | glib 0.18.5: Unsoundness in `Iterator` and `DoubleEndedIterator` impls for `glib::VariantStrIter` | `mobile/src-tauri/Cargo.lock` | upgrade to 0.20.0 |
| TV-CVE-2026-111062-russh | LOW | trivy | russh 0.63.3: A russh SSH server accepts a publickey authentication whose signature uses a different algorithm than the one named in the request | `Cargo.lock` | upgrade to 0.64.0 |
| TV-GHSA-cq8v-f236-94qc-rand | LOW | trivy | rand 0.7.3: Rand is unsound with a custom logger using rand::rng() | `Cargo.lock` | upgrade to 0.9.3, 0.10.1, 0.8.6 |

## What the last scans covered

| Tool | Date | Covered | Result | Not covered yet |
|---|---|---|---|---|
| claude-security, release v1.0.0 | 2026-10-19 | v0.1.14..954fc5e3 (everything 1.0.0 added up to the macOS shortcut fixes) | 0 open | 954fc5e3..v1.0.0: one credential-scrub fix (9a94302f) and the five new languages; everything after v1.0.0 on main |
| trivy | 2026-10-21 | whole repository at [`f18cb1c81`](https://github.com/addlayer-io/dbine/commit/f18cb1c813a0) | 5 open | — |

## Not counted, and why

Findings judged not to be a vulnerability. Each one says why and who decided.

| Finding | Where | Why | Decided in |
|---|---|---|---|
| Trivy secret rule exception `bigquery-test-service-account` | `crates/drivers/bigquery/src/gcp.rs` | The unit test in gcp_tests builds a fake service-account JSON around the throwaway key in crates/drivers/bigquery/tests/test_key.pem; it holds no real credential. | trivy-secret.yaml |
| Trivy secret rule exception `postgres-test-entra-token` | `crates/drivers/postgres/src/cloud_auth.rs` | The unit test entra_claims_say_who_signed_in decodes a made-up, unsigned JWT ({"upn":"ana@contoso.com"}); it grants nothing. | trivy-secret.yaml |

## Repository practices

The checks below 10, with Scorecard's own risk level. They describe how the repository is run, not a vulnerability in DBine, and aren't counted in the badge.

_None._
