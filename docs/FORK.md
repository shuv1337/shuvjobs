# ShuvJobs Fork Boundary

ShuvJobs is a maintained fork of [Ali Goren's `sta`](https://github.com/aligoren/sta),
based on upstream revision `7957527f6f39f8ed65fa5e3e49c7b5a27dfeada4` (`v0.1.2`).
The fork narrows the upstream `MIT OR Apache-2.0` package declaration to MIT.
Every workspace crate declares the standard SPDX expression `MIT` through
`license.workspace = true`, and each crate directory carries a verbatim copy of
the root `LICENSE`, so the upstream MIT license and copyright notice remain
intact and ship in every crate archive. `scripts/check-identity.sh` fails if any
crate copy drifts from the root `LICENSE`.

## Canonical Identity

- Product and display name: **ShuvJobs**
- Repository: `shuv1337/shuvjobs`
- CLI package and command: `shuvjobs`
- Workspace libraries: `shuvjobs-core`, `shuvjobs-adapters`, and `shuvjobs-tui`
- Release artifacts: `shuvjobs-<platform>-<architecture>`

ShuvJobs is greenfield as a product identity. The former package, command,
crate, release-asset, and runtime names are retired. No compatibility aliases,
migration paths, persisted identifiers, or protocol identifiers are preserved.

## Deliberate Deltas

The initial fork establishes the ShuvJobs identity and includes correctness
fixes for scheduler parsing, timezone handling, and remote SSH failures. The
first ShuvJobs version is `0.2.0`, avoiding collision with the inherited
upstream `v0.1.2` tag. The minimum Rust version is 1.88, and the initial fork
also updates terminal and plist dependencies to versions without known RustSec
vulnerabilities. The product direction — full create, read, update, and delete management across
cron, systemd timers, `at`, anacron, and launchd — is implemented: `add`,
`edit`, `rm`, `enable`, and `disable` work from the CLI and the TUI, locally
and over SSH, with `--dry-run` plans, opt-in `sudo -n` escalation through
`--sudo`, and backups written on the operator's machine.

Management introduces three ShuvJobs conventions that have no upstream
equivalent:

- `# managed by shuvjobs` heads every cron and systemd file ShuvJobs creates,
  and a `ShuvjobsManaged` key marks every launchd plist it creates. ShuvJobs
  rewrites or deletes only files carrying the marker; anything else is
  preserved, or refused with an `unsupported` error.
- `#shuvjobs-disabled# ` prefixes a disabled cron or anacron line. ShuvJobs
  still parses such a line and reports it as a job with `enabled: false`;
  every other scheduler sees an ordinary comment.
- Managed directories are fixed: `~/.config/systemd/user` and
  `/etc/systemd/system` for timers, `/etc/cron.d` for system cron,
  `/etc/anacrontab` for anacron, and `~/Library/LaunchAgents` and
  `/Library/LaunchDaemons` for launchd. Writes outside them — vendor units,
  run-parts scripts, `/System` plists — are refused rather than attempted.

## Upstream Policy

The `upstream` remote tracks `https://github.com/aligoren/sta`. Future upstream
changes may be reviewed and merged selectively. Every sync must preserve this
identity boundary, pass `scripts/check-identity.sh`, and retain upstream license
and attribution material.

## Identity Classification

- Canonical surfaces use only ShuvJobs names: manifests, commands, help text,
  UI labels, documentation, release workflows, artifacts, and runtime paths.
- Compatibility identifiers: none.
- Provenance identifiers remain only in this file, the MIT license, and version
  control history.
- Test fixtures and internal Rust symbols follow the canonical ShuvJobs identity;
  a future fixture that legitimately contains provenance must be explicitly
  allowlisted by the boundary check.
