# Roadmap

What DBine plans for the next 12 months (to the end of 2027), and what it
won't do. Plans change with what people actually use: a feature is built when
it will get real use, and every feature works on every engine that can
support it (see [AGENTS.md](AGENTS.md)). Ideas and requests go to the
[issues](https://github.com/addlayer-io/dbine/issues).

## Planned

**Security and trust**
- Encrypted connections by default: new connections ask for TLS with the
  server's certificate verified, with a clear warning when it's turned off.
- A Content Security Policy for the app's window.
- Reproducible builds for the driver hosts and the Linux binary, with a
  documented way to check them.
- A security review of every release, published on the
  [security status page](https://github.com/addlayer-io/dbine/blob/security-status/STATUS.md).

**Accessibility**
- Keyboard access to every control, visible focus everywhere, and labels for
  icon buttons.
- Reduced motion and high-contrast support, and text colours that meet WCAG AA.

**Working with data**
- More notebook building blocks: charts, parameters and scheduled runs keep
  growing from what notebooks already do.
- Single-record (transposed) view and comparing two result sets, if they
  prove widely useful.
- A step-by-step debugger for stored procedures, on the engines that offer
  one.

**The phone app**
- Reaching the computer beyond the local network without servers of ours:
  through the user's own cloud drive, and public relays only after an
  explicit opt-in ([docs/mobile-security.md](docs/mobile-security.md)).

**Engines and releases**
- New engines as they're requested, through the driver contract
  ([docs/drivers.md](docs/drivers.md)).
- Faster releases: drivers published on their own, and shorter build times.

## Out of scope

- **Running servers for users' data.** DBine runs on the user's computer;
  connections, credentials and results stay there. Sync and backups go only
  to the user's own cloud storage, encrypted.
- **Reselling or bundling database engines.** DBine connects to databases;
  it doesn't ship or host them (DuckDB and SQLite are libraries it uses
  locally).
- **An AI that changes data on its own.** The assistant reads through
  DBine's tools, and every change needs the user's approval.
- **Telemetry with personal or database content.** Usage telemetry stays
  anonymous and can be turned off.
