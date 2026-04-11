# sta

Scheduled Task Auditor — a unified terminal UI for inspecting every
scheduled task on a Linux or macOS host: cron, systemd timers, `at`,
anacron, and launchd.

## Screenshots

<!-- Replace these placeholders with real screenshots before publishing. -->

### Main View

![sta main view](docs/screenshots/main.png)

### Detail Pane

![sta detail pane](docs/screenshots/detail.png)

### Filter Mode

![sta filter mode](docs/screenshots/filter.png)

## Why

A typical Linux server has scheduled work in three different places at
once: systemd timers under `systemctl list-timers`, system crontabs
under `/etc/cron.{d,hourly,daily,weekly,monthly}`, and per-user
crontabs under `crontab -l`. macOS adds launchd to the mix. Each
subsystem has its own command, output format, and convention for "next
run" and "last result". Auditing what's actually scheduled to fire on
a host means running four or five commands, parsing their output by
hand, and reconciling the differences.

`sta` collapses all of that into a single sortable, filterable table.

## Features

- One unified table for cron, systemd timers, `at`, anacron, and launchd
- Live filter by source kind
- Sort by next run, last run, name, or status
- Detail pane with the full command, schedule expression, last status, and run duration
- Substring search across name and command
- Auto-refresh on a configurable interval
- Remote-host mode over SSH — no binary upload, runs the host's own commands
- JSON export for scripting and pipelines
- Soft-skip for unavailable subsystems (systemd on macOS, launchd on Linux, etc.)

## Installation

```sh
cargo install sta
```

AUR and Homebrew packages are planned and will be linked here once
published.

## Usage

### Local TUI

Run with no arguments to inspect the current host:

```sh
sta
```

### Remote host over SSH

`sta` does not ship a binary to the remote machine. It opens an SSH
connection, runs the same commands an operator would type by hand
(`systemctl`, `crontab -l`, `atq`, `cat /etc/anacrontab`, and so on),
and parses the output locally.

```sh
sta --host user@hostname
```

SSH key authentication must already be set up — `sta` runs in
`BatchMode=yes` and never prompts for a password.

#### Custom SSH port

```sh
sta --host user@hostname --port 2222
```

#### Custom SSH key

```sh
sta --host user@hostname --key ~/.ssh/id_ed25519
```

### JSON export

Print every collected task as a JSON array and exit:

```sh
sta --json
```

Works in both local and remote mode and is intended for piping into
other tools:

```sh
sta --host user@hostname --json | jq '.[] | select(.source == "systemd")'
```

### Auto-refresh

Re-collect and redraw every N seconds:

```sh
sta --refresh 30
```

Combine with remote mode for continuous monitoring of a server:

```sh
sta --host user@hostname --refresh 60
```

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
| `/`             | Enter search mode                             |
| `Backspace`     | Delete one character (in search mode)         |
| `q`             | Quit                                          |

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

## Contributing

Pull requests are welcome. Before submitting:

1. Run `cargo test` and make sure everything passes.
2. Run `cargo clippy --workspace --all-targets` and address any
   warnings.
3. If you are touching a parser, add a fixture test. Real captured
   command output beats idealized examples — see the existing
   fixtures in `crates/sta-adapters/src/*.rs` for the convention.

For larger changes please open an issue first to discuss the
approach.

## License

MIT — see [LICENSE](LICENSE).
