# Security model

What DBine protects, what it trusts, and why its design holds up: the
assurance case behind [SECURITY.md](../SECURITY.md). The current findings,
from every tool that looks for them, are on the
[security status page](https://github.com/addlayer-io/dbine/blob/security-status/STATUS.md).

## What you can expect

- **Your credentials stay on your computer.** Passwords, tokens and keys go
  to the operating system's keychain (or DBine's encrypted vault), never to
  the state file, the logs, telemetry or the cloud sync in clear. Secrets can
  also come from a secret manager or a command at connect time.
- **Nothing changes your databases without you.** The AI assistant and MCP
  clients read through server-enforced read-only sessions where the engine
  allows it, and every write waits for your approval in DBine. Scheduled
  tasks that change data run only as you approved them.
- **Updates and drivers are what AddLayer published.** The app checks the
  signature of every update and of the driver index, and the size and
  SHA-256 of every driver it downloads, before using them.
- **The phone app can't reach your data.** It never gets credentials, hosts,
  connection strings or table rows; it can't send SQL; anything that changes
  something needs that phone's permission and is confirmed on the phone
  ([mobile-security.md](mobile-security.md)).
- **Sync and backups are encrypted before they leave.** They go only to your
  own cloud storage, encrypted on your computer.

## What you can't expect

- **DBine doesn't protect a computer that's already compromised.** Malware
  running as your user can read what you can, including the keychain items
  DBine stores.
- **Connections are as secure as you configure them.** If you turn off TLS,
  or trust a server's certificate without checking it, traffic to that server
  can be read or changed on the network. Some self-hosted engines don't offer
  TLS at all.
- **DBine doesn't sandbox the SQL you run.** A statement you run or approve
  runs with your database account's privileges.
- **Commands are trusted.** A secret source or a task step that runs a
  command runs it as you; DBine asks you to confirm commands that came from
  an import or another computer.

## Trust boundaries

| Boundary | What crosses it | How it's guarded |
|---|---|---|
| Web UI ↔ Rust core (Tauri) | Command calls | One typed argument per command (serde), closed enums, allow-lists; the webview has no file system, shell or network permissions of its own |
| DBine ↔ database servers | SQL, results, server errors | TLS with certificate verification when enabled; identifiers quoted per engine; server text treated as data, never as SQL |
| App ↔ driver hosts | Requests and results over a pipe | Hosts come from the signed index, size- and hash-checked; frames are size-capped |
| DBine ↔ secret managers | Credentials | HTTPS only (plain HTTP only to this computer), pinned endpoints per provider; commands need local confirmation |
| DBine ↔ phone | Requests and summaries | Noise protocol end to end, paired keys only, closed request set, permissions per phone, biometric confirmation for changes |
| DBine ↔ MCP clients / AI | Tool calls | Loopback only, origin and host checks, hashed bearer tokens compared in constant time, approval for writes |
| DBine ↔ files and projects | Paths | Containment checks (no `..`, absolute, UNC or drive paths), per-file grants for notebooks |
| AddLayer ↔ users | Releases, drivers | minisign signatures, Apple notarization, Authenticode; see [Verify a download](../SECURITY.md#verify-a-download) |

## Threat model

Who DBine defends against:

1. **Someone on the network** between DBine and a server, a secret manager or
   the update host: reading or changing traffic. Countered by TLS with
   verification, HTTPS-only clients and signed updates.
2. **Hostile data**: rows, column and object names, server errors, imported
   connection files, notebooks or projects from someone else. Countered by
   treating them as data: quoting, escaping in every output format, no
   execution of imported commands without confirmation, markdown that keeps
   only safe links.
3. **A malicious or compromised MCP client or AI output** trying to change a
   database. Countered by read-only sessions and approval of every write.
4. **A lost or stolen phone.** Countered by per-phone permissions, biometric
   confirmation and revocation from the computer.
5. **A tampered release or driver.** Countered by signatures and hashes
   checked before use, with anti-rollback on the driver index.

Out of scope: an attacker with your user account on the computer, and the
security of the database servers themselves.

## Secure design principles applied

- **Least privilege:** the webview gets no file, shell or network access; the
  phone and AI get read access by default; writes need explicit approval.
- **Fail-safe defaults:** unknown drivers, unconfirmed imported commands and
  unpaired phones are refused; a read that can't be made read-only on the
  server asks first.
- **Complete mediation:** every write path (UI, AI, MCP, phone, tasks) goes
  through the same approval rules.
- **Economy of mechanism:** cryptography comes from established libraries
  (rustls, AWS-LC, RustCrypto, snow); DBine implements none itself.
- **Separation of privilege:** secrets live in the keychain, apart from the
  state that syncs.
- **Open design:** the code, this model and the scan results are public.

## Common weaknesses countered

| Weakness (CWE) | How DBine counters it |
|---|---|
| SQL injection (CWE-89) | Identifiers quoted per engine (`crates/dbine-driver/src/sql.rs`), parameters rendered as literals per engine; text from servers never spliced into SQL |
| Path traversal (CWE-22) | Path containment in project and notebook files; archives unpacked entry by entry with only rwx permissions |
| OS command injection (CWE-78) | DBine's own process calls pass values as arguments, without a shell; a secret source's command is the user's own command line, run by their shell only after a local confirmation when it was imported or synced |
| Cleartext transmission (CWE-319) | TLS 1.2+ with verification; HTTPS-only clients; Noise for the phone link |
| Cleartext storage of secrets (CWE-312) | OS keychain or an encrypted vault (XChaCha20-Poly1305, Argon2) |
| Improper certificate validation (CWE-295) | Verification on by default whenever TLS is on; turning it off is a per-connection choice |
| Download of code without integrity check (CWE-494) | Signed updates and driver index, SHA-256 per driver |
| Timing side channels (CWE-208) | RSA private-key operations in AWS-LC (constant time); tokens compared in constant time |
| Cross-site scripting in the UI (CWE-79) | Vue escaping, markdown escaped with only http(s)/mailto links, release notes rendered as text |
| Resource exhaustion (CWE-400) | Size caps on driver frames, phone messages, command output and translated text |

## How this is checked

The checks in [SECURITY.md](../SECURITY.md#how-dbine-is-checked) (OSV-Scanner,
Trivy, CodeQL, OpenSSF Scorecard and a claude-security review before each
release) look for regressions against this model. Anything found goes on the
status page until it's fixed.
