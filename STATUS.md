# DBine security status

The security findings open in DBine right now, from every tool that looks for them. A finding leaves this page when it's fixed. How the checks work and how to report a vulnerability: [SECURITY.md](https://github.com/addlayer-io/dbine/blob/main/SECURITY.md).

_Updated 2026-10-21._

## Open findings

| Source | Critical | High | Medium | Low |
|---|---|---|---|---|
| claude-security (source-code review) | 0 | 0 | 0 | 0 |
| Trivy (dependencies, secrets, configuration) | 0 | 0 | 2 | 1 |
| CodeQL (static analysis) | 0 | 0 | 0 | 0 |

Findings from the source-code review and CodeQL show only their kind and component while they're open.

| ID | Severity | Source | Finding | Where | Fix |
|---|---|---|---|---|---|
| TV-GHSA-wrw7-89jp-8q8g-glib-13ee4b | MEDIUM | trivy | glib 0.18.5: Unsoundness in `Iterator` and `DoubleEndedIterator` impls for `glib::VariantStrIter` | `Cargo.lock` | upgrade to 0.20.0 |
| TV-GHSA-wrw7-89jp-8q8g-glib-9761a8 | MEDIUM | trivy | glib 0.18.5: Unsoundness in `Iterator` and `DoubleEndedIterator` impls for `glib::VariantStrIter` | `mobile/src-tauri/Cargo.lock` | upgrade to 0.20.0 |
| TV-GHSA-cq8v-f236-94qc-rand-13ee4b | LOW | trivy | rand 0.7.3: Rand is unsound with a custom logger using rand::rng() | `Cargo.lock` | upgrade to 0.9.3, 0.10.1, 0.8.6 |

## What the last scans covered

| Tool | Date | Covered | Result | Not covered yet |
|---|---|---|---|---|
| claude-security, release v1.0.0 | 2026-10-19 | v0.1.14..954fc5e3 (everything 1.0.0 added up to the macOS shortcut fixes) | 0 open | 954fc5e3..v1.0.0: one credential-scrub fix (9a94302f) and the five new languages; everything after v1.0.0 on main |
| codeql | 2026-10-21 | Rust, TypeScript/Vue and workflows | 0 open | — |
| scorecard | 2026-10-21 | repository settings and workflows at [`f18cb1c81`](https://github.com/addlayer-io/dbine/commit/f18cb1c813a0) | score 2.9/10 | — |
| trivy | 2026-10-21 | whole repository at [`427645aa9`](https://github.com/addlayer-io/dbine/commit/427645aa9e0e) | 3 open | — |

## Not counted, and why

Findings judged not to be a vulnerability. Each one says why and who decided.

| Finding | Where | Why | Decided in |
|---|---|---|---|
| Trivy secret rule exception `bigquery-test-service-account` | `crates/drivers/bigquery/src/gcp.rs` | The unit test in gcp_tests builds a fake service-account JSON around the throwaway key in crates/drivers/bigquery/tests/test_key.pem; it holds no real credential. | trivy-secret.yaml |
| Trivy secret rule exception `postgres-test-entra-token` | `crates/drivers/postgres/src/cloud_auth.rs` | The unit test entra_claims_say_who_signed_in decodes a made-up, unsigned JWT ({"upn":"ana@contoso.com"}); it grants nothing. | trivy-secret.yaml |

## Repository practices

OpenSSF Scorecard: **2.9/10** (2026-10-21). The checks below 10, with Scorecard's own risk level. They describe how the repository is run, not a vulnerability in DBine, and aren't counted in the badge.

| Check | Risk | Docs |
|---|---|---|
| Binary-Artifacts: 8/10 — binaries present in source code | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#binary-artifacts) |
| Branch-Protection: 0/10 — branch protection not enabled on development/release branches | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#branch-protection) |
| Code-Review: 0/10 — Found 0/30 approved changesets -- score normalized to 0 | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#code-review) |
| Dependency-Update-Tool: 0/10 — no update tool detected | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#dependency-update-tool) |
| Maintained: 0/10 — project was created within the last 90 days. Please review its contents carefully | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#maintained) |
| Signed-Releases: 8/10 — 5 out of the last 5 releases have a total of 5 signed artifacts. | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#signed-releases) |
| Token-Permissions: 0/10 — detected GitHub workflow tokens with excessive permissions | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#token-permissions) |
| Vulnerabilities: 0/10 — 30 existing vulnerabilities detected | HIGH | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#vulnerabilities) |
| Fuzzing: 0/10 — project is not fuzzed | MEDIUM | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#fuzzing) |
| Pinned-Dependencies: 4/10 — dependency not pinned by hash detected -- score normalized to 4 | MEDIUM | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#pinned-dependencies) |
| SAST: 0/10 — no SAST tool detected | MEDIUM | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#sast) |
| Security-Policy: 4/10 — security policy file detected | MEDIUM | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#security-policy) |
| CII-Best-Practices: 0/10 — no effort to earn an OpenSSF best practices badge detected | LOW | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#cii-best-practices) |
| Contributors: 0/10 — project has 0 contributing companies or organizations -- score normalized to 0 | LOW | [link](https://github.com/ossf/scorecard/blob/61e84743356275f6ffca23b1b2f2cb945e19eb96/docs/checks.md#contributors) |
