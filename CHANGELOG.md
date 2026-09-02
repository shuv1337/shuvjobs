# Changelog

## 0.2.0 - Unreleased

- Establish the greenfield ShuvJobs product, package, command, and release identity.
- Record the maintained-fork boundary and upstream sync policy.
- Correct cron shortcut parsing and local/remote scheduler timezone handling.
- Treat SSH transport failures as fatal instead of returning partial audits.
- Raise the minimum supported Rust version from 1.75 to 1.88.
- Upgrade Ratatui, Crossterm, plist, and transitive dependencies to resolve
  current RustSec advisories.
- Publish fork crates under the MIT license while retaining the upstream notice.
- Derive systemd status from the bound service instead of the timer unit, so
  failed and in-flight activations are reported; populate run duration from the
  service's main-process timestamps.
- Read the invoking user's own crontab with plain `crontab -l`; `crontab -u` is
  root-only, so unprivileged local and remote runs previously showed no
  per-user jobs at all.
- Accept the cronie anacron period aliases `@daily`, `@weekly`, `@yearly`, and
  `@annually`.
- Keep the SSH multiplex master alive across `--refresh` cycles instead of
  tearing it down after every collection.
- Stop capturing the mouse in the TUI so terminal text selection works.
- Verify the declared minimum Rust version in CI.
