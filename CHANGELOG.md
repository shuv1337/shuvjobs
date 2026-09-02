# Changelog

## 0.2.0 - Unreleased

- Establish the greenfield ShuvJobs product, package, command, and release identity.
- Record the maintained-fork boundary and upstream sync policy.
- Correct cron shortcut parsing and local/remote scheduler timezone handling.
- Treat SSH transport failures as fatal instead of returning partial audits.
- Raise the minimum supported Rust version from 1.75 to 1.88.
- Upgrade Ratatui, Crossterm, plist, and transitive dependencies to resolve
  current RustSec advisories.
- Publish fork crates under the standard SPDX `MIT` license expression inherited
  from the workspace, with a copy of the upstream MIT notice packaged in every
  crate archive.
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
- Collect on a background thread so the TUI stays responsive during a refresh
  instead of freezing for the whole SSH round-trip; the auto-refresh interval is
  now measured from the last completed refresh.
- Add the `r` key to refresh on demand, with or without `--refresh`.
- Keep the last good data and show `refresh failed: <reason>` in the header when
  a refresh fails, instead of exiting the TUI.
- Render absolute timestamps in the TUI (detail pane last/next run, one-shot
  schedules, and the far-future date fallback) in the local timezone with the
  offset shown, instead of UTC.
- Compute the next run time for launchd jobs that use
  `StartCalendarInterval`, including arrays of intervals (earliest wins) and
  both Sunday spellings of `Weekday`. `StartInterval` jobs still show no next
  run, because launchd does not expose the load time the interval counts from.
- Collect user-scope systemd timers (`systemctl --user`) alongside system
  timers, locally and over SSH; user tasks get `user/`-prefixed ids and a
  `(user)` name suffix, and an unreachable user manager is skipped
  silently instead of failing the systemd source.
- Batch the systemd `systemctl show` calls: one invocation per property
  set per scope (chunked at 64 units) instead of two process spawns per
  timer, locally and over SSH. Blocks are keyed by the unit's `Id=`, so a
  missing or reordered block can never be applied to the wrong task.
  Local `--json` collection on a 12-task host drops from ~46 ms to ~24 ms.
- Report an overdue anacron job as due now instead of showing a next run in
  the past.
- Verify the declared minimum Rust version in CI.
- Add `location` and `enabled` to the JSON task export: `location` is the
  backing file (cron file, systemd `FragmentPath`, anacrontab, launchd
  plist) and `enabled` is whether the job would run, both `null` when the
  source has no such notion. Older JSON without the keys still loads.
- Populate those two fields for every source: systemd timers report
  `FragmentPath` and their `UnitFileState`/`ActiveState` enablement, cron
  file entries report their file, run-parts scripts report their path and
  executable bit, anacron entries report `/etc/anacrontab`, and launchd
  jobs report their plist and whether launchd has them loaded.
- List systemd timers that `systemctl list-timers` cannot see. A timer that
  has been disabled and stopped is unloaded, so it vanished from the listing
  and `list`, `enable`, `edit`, and `rm` all reported it as not found. Both
  the local adapter and the SSH bridge now also read
  `systemctl [--user] list-unit-files --type=timer --all`, and every timer
  unit file that the timer listing did not name is reported with its
  schedule, command, location, and enablement. Template units (`foo@.timer`)
  are skipped.
