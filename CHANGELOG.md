# Changelog

## 0.3.0 - 2026-09-02

First ShuvJobs release. Version 0.2.0 was reserved for the fork identity
work and never tagged; everything below ships together as 0.3.0.

- Exit quietly instead of panicking when stdout is closed early, e.g.
  `shuvjobs list | head`.
- Skip binary files in the fork identity check so it passes on macOS runners.

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
- Manage jobs, not just read them: `add`, `edit`, `rm`, `enable`, and
  `disable` subcommands alongside the explicit `list` and `show`, for cron,
  systemd timers, `at`, anacron, and launchd. Ids are the ones `list` prints;
  `--source` disambiguates an id two schedulers both claim.
- Add `--dry-run` (render a unified diff per file plus the exact commands and
  exit without writing), `-y`/`--yes` (skip the `rm` confirmation; a
  non-terminal stdin without it is refused rather than treated as yes), and
  `--sudo` (wrap privileged steps in `sudo -n --`). Without `--sudo`,
  operations needing root fail before writing anything and name the flag.
- Define exit codes for scripting: 0 ok, 1 runtime/not-found/ambiguous/
  aborted, 2 usage or validation, 3 needs root, 4 conflict, 5 unsupported.
  `--json` mutations report `{ok, op, source, id, host, dry_run, files,
  commands, notes}`, or `{ok: false, error: {kind, message}}`.
- Back up every file a mutation overwrites or removes to
  `$XDG_STATE_HOME/shuvjobs/backups/<host>/<flattened-path>.<timestamp>` on
  the machine running shuvjobs, never next to the target, before the first
  write; the path is reported even when the apply fails.
- Per-scheduler management: cron writes the invoking user's crontab or
  `/etc/cron.d/<name>`; systemd writes a `.timer`/`.service` pair under
  `~/.config/systemd/user` or `/etc/systemd/system` and reloads and enables
  them; `at` queues and requeues jobs (edit recreates, never losing the job);
  anacron edits `/etc/anacrontab`; launchd writes LaunchAgents/LaunchDaemons
  plists and bootstraps them. Files shuvjobs created carry
  `# managed by shuvjobs` (or a `ShuvjobsManaged` plist key) and only those
  are rewritten; disabling a cron or anacron job prefixes its line with
  `#shuvjobs-disabled# `. Vendor systemd units, `/System` plists, and
  run-parts scripts are refused with an `unsupported` error.
- Manage over SSH: every subcommand works against `--host`, with reads and
  writes sharing one multiplex master.
- Add TUI editing: `a` add, `e` edit, `d` delete, `t` toggle enabled. A form
  popup (Tab/Shift-Tab between fields, ◂ ▸ to change pickers, Enter to plan,
  Esc to cancel) leads to a scrollable confirmation popup showing the plan
  (`y` apply, `n` cancel). `q` no longer quits from inside a popup, and under
  `--dry-run` applying reports `dry run: not applied`.
