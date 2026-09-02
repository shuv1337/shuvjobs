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
