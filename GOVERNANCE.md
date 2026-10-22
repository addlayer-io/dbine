# Governance

How decisions about DBine are made, who holds each role, and how the project
keeps going if its maintainer is unavailable.

## Decision model

DBine is developed by AddLayer and led by a single maintainer, who has the
final say on features, design, releases and security. Decisions follow
written rules that apply to everyone, the maintainer included:

- Every feature works on every engine that can support it
  ([AGENTS.md](AGENTS.md)).
- Every text ships in all ten UI languages.
- A release goes out only after the test gate and a security review pass
  ([SECURITY.md](SECURITY.md)).
- What's planned is public in [ROADMAP.md](ROADMAP.md).

Proposals come as [issues](https://github.com/addlayer-io/dbine/issues) or
pull requests ([CONTRIBUTING.md](CONTRIBUTING.md)). The maintainer answers
each one, and a decision that changes direction is recorded in the roadmap
or the docs.

## Roles

| Role | What it covers | Who holds it |
|---|---|---|
| Maintainer | Reviews and merges changes, cuts releases, triages issues and security reports, decides on the roadmap | Marcos Panichella (AddLayer) |
| Release signing | The update signing key (minisign), the Apple Developer ID and the Windows code-signing certificate | The maintainer |
| Security contact | Private vulnerability reports ([SECURITY.md](SECURITY.md)) | The maintainer |
| Code of conduct | Reports to [conduct@addlayer.io](mailto:conduct@addlayer.io) ([CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)) | The maintainer, with the emergency access custodian as backup |
| Contributor | Proposes changes through pull requests; anyone can | Anyone who follows CONTRIBUTING.md |
| Emergency access custodian | Can recover the project's accounts and keys if the maintainer is unavailable (below) | A member of the maintainer's family |

When contributors join and show sustained, trusted work, the maintainer
appoints one of them as a second maintainer with the same access, and this
table records it.

## Access continuity

If the maintainer becomes unavailable, the project can continue: the
emergency access custodian keeps sealed recovery instructions and the
credentials needed to hand the project over to a new maintainer:

- Owner access to the `addlayer-io` GitHub organization (recovery codes), so
  issues can still be created and closed, pull requests merged, and releases
  published.
- The release signing material: the update signing key and its password, the
  Apple Developer account and the Windows code-signing certificate.
- The `dbine.com` domain and its hosting.
- The `addlayer.io` mail accounts used for security and conduct reports.

The custodian's task is to give that access to a new maintainer (another
AddLayer member or a trusted contributor), not to run the project. The
instructions are reviewed whenever a credential changes.

## Changing this document

Changes to governance go through a pull request like any other change; the
history of this file records them.
