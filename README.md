# ShuvJobs

ShuvJobs is a unified terminal interface for inspecting and managing
every scheduled task on a Linux or macOS host: cron, systemd timers,
`at`, anacron, and launchd.

## Screenshots

### Main View

![ShuvJobs main view](docs/screenshots/main.png)

### Detail Pane

![ShuvJobs detail pane](docs/screenshots/detail.png)

### Filter Mode

![ShuvJobs filter mode](docs/screenshots/filter.png)

## Why

A typical Linux server has scheduled work in three different places at
once: systemd timers under `systemctl list-timers` (in both the system
and per-user managers), system crontabs
under `/etc/cron.{d,hourly,daily,weekly,monthly}`, and per-user
crontabs under `crontab -l`. macOS adds launchd to the mix. Each
subsystem has its own command, output format, and convention for "next
run" and "last result". Auditing what's actually scheduled to fire on
a host means running four or five commands, parsing their output by
hand, and reconciling the differences.

`shuvjobs` collapses all of that into a single sortable, filterable table.

## Features

- One unified table for cron, systemd timers, `at`, anacron, and launchd
- Create, edit, enable, disable, and delete jobs in every one of those
  schedulers, from the CLI or the TUI
- `--dry-run` renders a unified diff and the exact commands before anything
  is written; overwritten files are backed up outside the target directory
- Both systemd scopes: system timers and the calling user's own
  `systemctl --user` timers, shown as `name (user)`
- Live filter by source kind
- Sort by next run, last run, name, or status
- Detail pane with the full command, schedule expression, last status, and run duration
- Absolute timestamps in your local timezone, with the UTC offset spelled out
- Substring search across name and command
- Non-blocking auto-refresh on a configurable interval, plus manual refresh with `r`
- Remote-host mode over SSH — no binary upload, runs the host's own commands
- JSON export for scripting and pipelines
- Soft-skip for unavailable subsystems (systemd on macOS, launchd on Linux, etc.)

## Installation

```sh
cargo install --git https://github.com/shuv1337/shuvjobs shuvjobs
```

Crates.io, AUR, and Homebrew packages are planned.

## Usage

### Local TUI

Run with no arguments to inspect the current host:

```sh
shuvjobs
```

### Remote host over SSH

`shuvjobs` does not ship a binary to the remote machine. It opens an SSH
connection, runs the same commands an operator would type by hand
(`systemctl`, `crontab -l`, `atq`, `cat /etc/anacrontab`, and so on),
and parses the output locally.

```sh
shuvjobs --host user@hostname
```

SSH key authentication must already be set up — `shuvjobs` runs in
`BatchMode=yes` and never prompts for a password.

#### Custom SSH port

```sh
shuvjobs --host user@hostname --port 2222
```

#### Custom SSH key

```sh
shuvjobs --host user@hostname --key ~/.ssh/id_ed25519
```

### JSON export

Print every collected task as a JSON array and exit:

```sh
shuvjobs --json
```

`shuvjobs list --json` is the same thing spelled explicitly, and is the
form to prefer in scripts.

Works in both local and remote mode and is intended for piping into
other tools:

```sh
shuvjobs --host user@hostname --json | jq '.[] | select(.source == "systemd")'
```

### Auto-refresh

Re-collect and redraw every N seconds:

```sh
shuvjobs --refresh 30
```

Combine with remote mode for continuous monitoring of a server:

```sh
shuvjobs --host user@hostname --refresh 60
```

Collection runs on a background thread, so the table stays scrollable,
searchable, and filterable while a refresh (including a slow SSH round-trip) is
in flight. The header shows `refreshing…` while one is running, and the interval
is measured from the last *completed* refresh, so refreshes never stack up.

Press `r` at any time to refresh immediately — with or without `--refresh`. A
request made while a refresh is already in flight is ignored.

If a refresh fails, the last successfully collected data stays on screen and the
header shows `refresh failed: <reason>` until the next successful refresh; the
TUI does not exit.

## Managing jobs

Every source `shuvjobs` reads it can also write. `add`, `edit`, `rm`,
`enable`, and `disable` work the same way locally and over `--host`.

| Command   | What it does                              | Example |
|-----------|-------------------------------------------|---------|
| `list`    | Print every collected job as a table      | `shuvjobs list` |
| `show`    | Print one job's fields, one per line      | `shuvjobs show user/backup.timer` |
| `add`     | Create a job in the named scheduler       | `shuvjobs add --source systemd --scope user --name backup --schedule daily --command /usr/bin/backup` |
| `edit`    | Change the schedule, command, name, or user of an existing job | `shuvjobs edit user/backup.timer --schedule 'Mon *-*-* 09:00:00'` |
| `rm`      | Delete a job (prompts unless `--yes`)     | `shuvjobs rm user/backup.timer --yes` |
| `enable`  | Make the job's schedule fire again        | `shuvjobs enable user/backup.timer` |
| `disable` | Stop it firing without deleting it        | `shuvjobs disable user/backup.timer` |

Ids are the values in the `ID` column of `list`. When two schedulers happen to
claim the same id, pass `--source` to disambiguate; without it the command
exits 1 and says so.

`add` takes `--source`, `--schedule`, and `--command`, plus the optional
`--name` (unit name, `cron.d` file name, anacron job id, or launchd label),
`--user`, `--scope user|system` (default `user`), and `--disabled`. The
schedule is written in the source's own syntax — five cron fields, a systemd
`OnCalendar` expression, `now + 1 hour` for `at` — or as a plain interval like
`30m`, `2h`, `1d`.

`edit` requires at least one of `--schedule`, `--command`, `--name`, `--user`;
an edit that changes nothing is a usage error rather than a silent rewrite.

### Dry run, confirmation, and privilege

| Flag        | Effect |
|-------------|--------|
| `--dry-run` | Render the plan — a unified diff per file, then the exact commands — and exit 0 without touching anything. No prompt. |
| `-y`, `--yes` | Answer the `rm` confirmation with yes. Without a terminal and without `--yes`, `rm` exits 2 instead of assuming a silent yes. |
| `--sudo`    | Allow privileged steps to run through `sudo -n --`. |

`shuvjobs` never escalates privilege on its own. Operations that need root —
another user's crontab, `/etc/cron.d`, run-parts scripts, `/etc/anacrontab`,
system systemd units, `/Library/LaunchDaemons` — fail before writing anything
with exit code 3 and a hint to pass `--sudo`. With `--sudo`, commands and file
writes are wrapped in `sudo -n --`, which is non-interactive: passwordless sudo
must already be configured on the target host, or the operation fails rather
than prompting.

### Backups

Before an apply overwrites or removes a file, its previous contents are copied
to:

```
$XDG_STATE_HOME/shuvjobs/backups/<host>/<flattened-path>.<unix-timestamp>
```

falling back to `$HOME/.local/state` when `XDG_STATE_HOME` is unset. `<host>`
is `local` or the `--host` argument. Backups always live on the machine running
`shuvjobs`, never next to the target — a stray `/etc/cron.d/backup.bak` would
itself be read by cron. The backup path is printed with the change and appears
as `files[].backup` in the JSON report, including when the apply fails.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success, including a completed `--dry-run` |
| 1 | Runtime failure, unknown id, ambiguous id, or a declined prompt |
| 2 | Usage or validation error (bad flags, unparseable schedule, `rm` without a terminal and without `--yes`) |
| 3 | The operation needs root and `--sudo` was not given, or `sudo -n` refused |
| 4 | Conflict: the job changed on disk since it was read, or the target already exists |
| 5 | Unsupported: the scheduler cannot express the request, or the target is not one `shuvjobs` writes |

### Example

```
$ shuvjobs add --source systemd --scope user --name shuvjobs-doc \
    --schedule daily --command /bin/true --dry-run
--- /dev/null
+++ /home/alice/.config/systemd/user/shuvjobs-doc.timer
@@ -0,0 +1,10 @@
+# managed by shuvjobs
+[Unit]
+Description=shuvjobs-doc (managed by shuvjobs)
+
+[Timer]
+OnCalendar=daily
+Persistent=true
+
+[Install]
+WantedBy=timers.target
--- /dev/null
+++ /home/alice/.config/systemd/user/shuvjobs-doc.service
@@ -0,0 +1,7 @@
+# managed by shuvjobs
+[Unit]
+Description=shuvjobs-doc (managed by shuvjobs)
+
+[Service]
+Type=oneshot
+ExecStart=/bin/sh -c "/bin/true"
commands:
  mkdir -p '/home/alice/.config/systemd/user'
  systemd-analyze calendar 'daily'
  systemctl --user daemon-reload
  systemctl --user enable --now 'shuvjobs-doc.timer'
```

Add `--json` for the same plan as a machine-readable report (trimmed here):

```json
{
  "ok": true,
  "op": "add",
  "source": "systemd",
  "id": "user/shuvjobs-doc.timer",
  "host": "local",
  "dry_run": true,
  "files": [
    {
      "path": "/home/alice/.config/systemd/user/shuvjobs-doc.timer",
      "backup": null,
      "diff": "--- /dev/null\n+++ /home/alice/.config/systemd/user/shuvjobs-doc.timer\n@@ -0,0 +1,10 @@\n+# managed by shuvjobs\n..."
    }
  ],
  "commands": [
    "mkdir -p '/home/alice/.config/systemd/user'",
    "systemctl --user enable --now 'shuvjobs-doc.timer'"
  ],
  "notes": []
}
```

A failure reports `{"ok": false, "error": {"kind": …, "message": …}}`, where
`kind` is one of `needs_root`, `conflict`, `unsupported`, `validation`,
`not_found`, `ambiguous`, `aborted`, or `other`.

### Per-scheduler notes

| Source | What `add` writes | What `update`/`delete` refuse | How enable/disable works |
|--------|-------------------|-------------------------------|--------------------------|
| cron | User scope: appends to the invoking user's crontab via `crontab -`. System scope: `/etc/cron.d/<name>`, mode 0644, headed by `# managed by shuvjobs`. macOS system scope is unsupported — use launchd. | Run-parts scripts (`/etc/cron.{hourly,daily,weekly,monthly}`) cannot be edited or deleted as crontab entries; edit the script or keep the job in `/etc/cron.d`. A line that changed since it was read is a conflict. | Comments the line out with the `#shuvjobs-disabled# ` marker and back in again. Run-parts scripts toggle the executable bit. |
| systemd | `<name>.timer` and `<name>.service` in `~/.config/systemd/user` or `/etc/systemd/system`, both headed by `# managed by shuvjobs`, then `daemon-reload` and `enable --now`. | Vendor units — a `FragmentPath` outside `/etc/systemd/system`, `/etc/systemd/user`, or `~/.config/systemd/user` — are unsupported: copy the unit into `/etc` first. A service that is not marked managed is left alone, and changing its command is unsupported. Deleting a vendor unit is unsupported; disable or mask it instead. | `systemctl [--user] enable --now` / `disable --now`. A `static` unit cannot be enabled (disable stops it); a masked unit must be unmasked first. |
| at | Queues the command with `at`, one shot only. `--user` is unsupported. | Editing an `at` job recreates it: a new job is queued first, then the old one is removed with `atrm`, so the job is never lost. | Unsupported — the queue has no paused state. Delete the job and schedule a new one. |
| anacron | A `period delay job command` line in `/etc/anacrontab`. Always needs root. `--user` is unsupported. | A duplicate job id is a conflict; a line that moved or changed since it was read is a conflict. Renaming removes the old `/var/spool/anacron/<job>` stamp. | The same `#shuvjobs-disabled# ` marker comment. |
| launchd | `~/Library/LaunchAgents/<label>.plist` (domain `gui/<uid>`) or `/Library/LaunchDaemons/<label>.plist` (domain `system`), carrying a `ShuvjobsManaged` key, then `launchctl enable` and `bootstrap`. macOS only. | Plists under `/System` are sealed by SIP and unsupported — use `launchctl disable`. Any directory other than the LaunchAgents/LaunchDaemons ones is refused. An unmanaged plist keeps its unknown keys on update. | `launchctl enable` + `bootstrap`, or `bootout` + `launchctl disable`. |

Files `shuvjobs` created carry a marker so it knows what it may rewrite:
`# managed by shuvjobs` in cron and systemd files, and a `ShuvjobsManaged` key
in launchd plists. Disabled cron and anacron lines are prefixed with
`#shuvjobs-disabled# `, which `shuvjobs` still parses and reports as a job with
`enabled: false`.

### Remote management

Every subcommand above works against `--host`, with the same `--sudo`,
`--dry-run`, and `--yes` semantics:

```sh
shuvjobs --host user@hostname add --source cron --schedule '*/5 * * * *' \
    --command '/usr/local/bin/collect' --dry-run
```

Reads and writes share a single SSH multiplex master, and backups are still
written on the machine running `shuvjobs`, under the `user@hostname` label.

## Keyboard shortcuts

| Key             | Action                                        |
|-----------------|-----------------------------------------------|
| `j` / `↓`       | Move selection down                           |
| `k` / `↑`       | Move selection up                             |
| `Page Down`     | Move selection 10 rows down                   |
| `Page Up`       | Move selection 10 rows up                     |
| `Home`          | Jump to first row                             |
| `End`           | Jump to last row                              |
| `Enter` / `l`   | Open detail pane for the selected row         |
| `Esc` / `h`     | Close detail pane                             |
| `f`             | Open the source filter bar                    |
| `Space`         | Toggle source under cursor (in filter mode)   |
| `s`             | Cycle sort mode                               |
| `r`             | Refresh now                                   |
| `/`             | Enter search mode                             |
| `Backspace`     | Delete one character (in search mode)         |
| `a`             | Add a job (opens the form)                    |
| `e`             | Edit the selected job (form prefilled)        |
| `d`             | Delete the selected job                       |
| `t`             | Toggle the selected job enabled or disabled   |
| `q`             | Quit                                          |

`a`, `e`, `d`, and `t` are inert while a refresh or a mutation is in flight,
and in a read-only session (the footer then shows the viewer keys only).

### In the add/edit form

| Key                 | Action                                    |
|---------------------|-------------------------------------------|
| `Tab` / `Shift-Tab` | Next / previous field                     |
| `←` / `→`           | Change the `◂ ▸` picker under the cursor  |
| `Enter`             | Plan the change and open the confirmation |
| `Esc`               | Cancel and close the form                 |

### In the confirmation popup

| Key             | Action                                        |
|-----------------|-----------------------------------------------|
| `y` / `Enter`   | Apply the plan                                |
| `n` / `Esc`     | Cancel, changing nothing                      |
| `j` / `k`       | Scroll the plan (also `↑`/`↓`, `PgUp`/`PgDn`) |

`q` does not quit while a form or the confirmation popup is open — it is
ordinary text or an ignored key there. Close the popup with `Esc` first. Under
`--dry-run` the popup shows the plan and answering `y` reports
`dry run: not applied`.

## Platform support

| Source   | Arch / CachyOS | Debian / Ubuntu | Fedora / RHEL | Alpine        | macOS         |
|----------|----------------|-----------------|---------------|---------------|---------------|
| systemd  | yes            | yes             | yes           | optional      | unavailable   |
| cron     | optional       | yes (vixie)     | yes (cronie)  | yes (busybox) | yes (legacy)  |
| at       | optional       | optional        | optional      | optional      | optional      |
| anacron  | optional       | yes             | yes           | no            | no            |
| launchd  | unavailable    | unavailable     | unavailable   | unavailable   | yes           |

"Unavailable" means the adapter is silently skipped on that platform.
"Optional" means the subsystem may not be installed; if it is not,
that source is silently skipped too.

Management follows the same table, with two exceptions: launchd jobs can only
be created or changed on macOS, and cron *system* scope (`/etc/cron.d`) is
unsupported on macOS — use launchd there. Per-user crontabs are writable on
macOS as everywhere else.

User-scope systemd timers are collected in addition to system timers,
locally and over SSH. Their ids are prefixed with `user/` and their
names are suffixed with `(user)`. When no user manager is reachable —
no session bus, no `loginctl enable-linger` on a remote host, or root
without `XDG_RUNTIME_DIR` — that scope is silently skipped and system
timers are still reported.

## Contributing

Pull requests are welcome. Before submitting:

1. Run `cargo test` and make sure everything passes.
2. Run `cargo clippy --workspace --all-targets` and address any
   warnings.
3. Run `scripts/check-identity.sh`.
4. If you are touching a parser, add a fixture test. Real captured
   command output beats idealized examples — see the existing
   fixtures in `crates/shuvjobs-adapters/src/*.rs` for the convention.

For larger changes please open an issue first to discuss the
approach.

## License

MIT — see [LICENSE](LICENSE).
