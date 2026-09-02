//! Inverses of the id each adapter hands out.
//!
//! The read path mints an id per task; every write starts from one of
//! those ids and has to get back to a file, a line, a unit, or a label.
//! Those parsers live here, next to each other, because the ambiguities
//! are between them: `/etc/cron.daily/backup:2` is a *script* whose name
//! ends in `:2`, while `/etc/cron.d/odd:name:7` is line 7 of a file whose
//! name contains a colon. The order of the checks in [`parse_cron_id`]
//! is what makes that come out right, so it is pinned by tests.

use shuvjobs_core::host::shell::shell_safe_filename;
use shuvjobs_core::host::shell::shell_safe_username;
use shuvjobs_core::{Error, Result, TaskSourceKind};

use crate::systemd::{split_task_id, Scope};

/// The run-parts directories cron drives, with the period each implies.
/// Single source of truth for the local collector, the SSH collector,
/// and the id parser.
pub const RUN_PARTS: [(&str, &str); 4] = [
    ("hourly", "/etc/cron.hourly"),
    ("daily", "/etc/cron.daily"),
    ("weekly", "/etc/cron.weekly"),
    ("monthly", "/etc/cron.monthly"),
];

/// Just the directories from [`RUN_PARTS`].
pub const RUN_PARTS_DIRS: [&str; 4] = [
    "/etc/cron.hourly",
    "/etc/cron.daily",
    "/etc/cron.weekly",
    "/etc/cron.monthly",
];

/// The period a run-parts directory implies, if it is one of cron's.
pub fn run_parts_period(dir: &str) -> Option<&'static str> {
    RUN_PARTS
        .iter()
        .find(|(_, d)| *d == dir)
        .map(|(period, _)| *period)
}

/// Where a cron task actually lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronId {
    /// A line in `/etc/crontab` or `/etc/cron.d/*`: has a user column.
    SystemFile { path: String, line: usize },
    /// A line in `crontab -l -u <user>`: no user column.
    UserCrontab { user: String, line: usize },
    /// A script in one of the run-parts directories.
    RunParts { path: String },
}

impl CronId {
    pub fn to_id(&self) -> String {
        match self {
            Self::SystemFile { path, line } => format!("{path}:{line}"),
            Self::UserCrontab { user, line } => format!("user:{user}:{line}"),
            Self::RunParts { path } => path.clone(),
        }
    }
}

fn parse_line_number(text: &str) -> Option<usize> {
    // Lines are 1-based everywhere in the read path, so 0 is not a
    // number we could ever have produced.
    text.parse::<usize>().ok().filter(|n| *n >= 1)
}

fn invalid(kind: &str, id: &str) -> Error {
    Error::Validation(format!("`{id}` is not a {kind} task id"))
}

pub fn parse_cron_id(id: &str) -> Result<CronId> {
    // 1. Per-user crontabs are unambiguous: they are the only ids with
    //    the `user:` prefix, and their user names cannot contain a colon.
    if let Some(rest) = id.strip_prefix("user:") {
        let (user, line) = rest
            .rsplit_once(':')
            .ok_or_else(|| invalid("cron", id))
            .and_then(|(user, line)| {
                let line = parse_line_number(line).ok_or_else(|| invalid("cron", id))?;
                Ok((user, line))
            })?;
        if !shell_safe_username(user) {
            return Err(invalid("cron", id));
        }
        return Ok(CronId::UserCrontab {
            user: user.to_string(),
            line,
        });
    }

    // 2. Run-parts scripts *before* any colon split: a script may well be
    //    called `backup:2`, and it is still a whole path, not a line.
    for dir in RUN_PARTS_DIRS {
        if let Some(name) = id.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) {
            if !name.is_empty() && !name.contains('/') {
                return Ok(CronId::RunParts {
                    path: id.to_string(),
                });
            }
        }
    }

    // 3. Anything else with a trailing `:<line>` on an absolute path.
    if let Some((path, line)) = id.rsplit_once(':') {
        if let Some(line) = parse_line_number(line) {
            if path.starts_with('/') {
                return Ok(CronId::SystemFile {
                    path: path.to_string(),
                    line,
                });
            }
        }
    }

    Err(invalid("cron", id))
}

/// A systemd timer: which manager, and the unit name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdId {
    pub scope: Scope,
    pub unit: String,
}

impl SystemdId {
    pub fn to_id(&self) -> String {
        self.scope.task_id(&self.unit)
    }
}

pub fn parse_systemd_id(id: &str) -> Result<SystemdId> {
    let (scope, unit) = split_task_id(id);
    // Only timers are tasks; a bare `.service` id would silently address
    // the wrong unit.
    if unit.is_empty() || !unit.ends_with(".timer") || unit.contains('/') {
        return Err(invalid("systemd", id));
    }
    Ok(SystemdId {
        scope,
        unit: unit.to_string(),
    })
}

/// The `at` job number, with or without the `at:` prefix the read path adds.
pub fn parse_at_id(id: &str) -> Result<u64> {
    let digits = id.strip_prefix("at:").unwrap_or(id);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("at", id));
    }
    digits.parse::<u64>().map_err(|_| invalid("at", id))
}

/// The anacron job identifier, with or without the `anacron:` prefix.
pub fn parse_anacron_id(id: &str) -> Result<String> {
    let job = id.strip_prefix("anacron:").unwrap_or(id);
    if !shell_safe_filename(job) {
        return Err(invalid("anacron", id));
    }
    Ok(job.to_string())
}

/// The launchd label, which is also the id.
pub fn parse_launchd_id(id: &str) -> Result<String> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok {
        return Err(invalid("launchd", id));
    }
    Ok(id.to_string())
}

/// A parsed id for any source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskId {
    Cron(CronId),
    Systemd(SystemdId),
    At(u64),
    Anacron(String),
    Launchd(String),
}

impl TaskId {
    pub fn kind(&self) -> TaskSourceKind {
        match self {
            Self::Cron(_) => TaskSourceKind::Cron,
            Self::Systemd(_) => TaskSourceKind::Systemd,
            Self::At(_) => TaskSourceKind::At,
            Self::Anacron(_) => TaskSourceKind::Anacron,
            Self::Launchd(_) => TaskSourceKind::Launchd,
        }
    }

    /// The id the read path would have produced for this target.
    pub fn to_id(&self) -> String {
        match self {
            Self::Cron(id) => id.to_id(),
            Self::Systemd(id) => id.to_id(),
            Self::At(job) => format!("at:{job}"),
            Self::Anacron(job) => format!("anacron:{job}"),
            Self::Launchd(label) => label.clone(),
        }
    }
}

pub fn parse_task_id(kind: TaskSourceKind, id: &str) -> Result<TaskId> {
    Ok(match kind {
        TaskSourceKind::Cron => TaskId::Cron(parse_cron_id(id)?),
        TaskSourceKind::Systemd => TaskId::Systemd(parse_systemd_id(id)?),
        TaskSourceKind::At => TaskId::At(parse_at_id(id)?),
        TaskSourceKind::Anacron => TaskId::Anacron(parse_anacron_id(id)?),
        TaskSourceKind::Launchd => TaskId::Launchd(parse_launchd_id(id)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_parts_tables_agree() {
        let dirs: Vec<&str> = RUN_PARTS.iter().map(|(_, d)| *d).collect();
        assert_eq!(dirs, RUN_PARTS_DIRS.to_vec());
        assert_eq!(run_parts_period("/etc/cron.weekly"), Some("weekly"));
        assert_eq!(run_parts_period("/etc/cron.d"), None);
    }

    #[test]
    fn cron_ids_round_trip() {
        let cases: &[(&str, CronId)] = &[
            (
                "/etc/crontab:12",
                CronId::SystemFile {
                    path: "/etc/crontab".into(),
                    line: 12,
                },
            ),
            (
                "/etc/cron.d/odd:name:7",
                CronId::SystemFile {
                    path: "/etc/cron.d/odd:name".into(),
                    line: 7,
                },
            ),
            (
                "user:alice:4",
                CronId::UserCrontab {
                    user: "alice".into(),
                    line: 4,
                },
            ),
            (
                "/etc/cron.daily/logrotate",
                CronId::RunParts {
                    path: "/etc/cron.daily/logrotate".into(),
                },
            ),
            (
                // A script whose name ends in `:2`, not line 2 of a file.
                "/etc/cron.daily/backup:2",
                CronId::RunParts {
                    path: "/etc/cron.daily/backup:2".into(),
                },
            ),
        ];
        for (id, expected) in cases {
            let parsed = parse_cron_id(id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(&parsed, expected, "parsing {id}");
            assert_eq!(&parsed.to_id(), id, "rendering {id}");
        }
    }

    #[test]
    fn cron_ids_reject_malformed_input() {
        for id in [
            "user:alice",                 // no line
            "user::3",                    // no user
            "user:al ice:3",              // unsafe user
            "/etc/crontab:0",             // lines are 1-based
            "/etc/crontab:x",             // not a number
            "/etc/crontab",               // no line at all
            "etc/crontab:3",              // not absolute
            "/etc/cron.daily/sub/script", // not a direct child
            "",
        ] {
            assert!(parse_cron_id(id).is_err(), "{id} must be rejected");
        }
    }

    #[test]
    fn systemd_ids_round_trip() {
        let cases: &[(&str, Scope, &str)] = &[
            ("logrotate.timer", Scope::System, "logrotate.timer"),
            ("user/radar-daily.timer", Scope::User, "radar-daily.timer"),
        ];
        for (id, scope, unit) in cases {
            let parsed = parse_systemd_id(id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(parsed.scope, *scope);
            assert_eq!(parsed.unit, *unit);
            assert_eq!(&parsed.to_id(), id);
        }
        for id in ["user/foo.service", "foo", "", "user/", "a/b.timer"] {
            assert!(parse_systemd_id(id).is_err(), "{id} must be rejected");
        }
    }

    #[test]
    fn at_ids_round_trip() {
        assert_eq!(parse_at_id("at:12").unwrap(), 12);
        assert_eq!(parse_at_id("12").unwrap(), 12);
        assert_eq!(TaskId::At(12).to_id(), "at:12");
        for id in ["at:12x", "at:", "", "-3", "at:1 2"] {
            assert!(parse_at_id(id).is_err(), "{id} must be rejected");
        }
    }

    #[test]
    fn anacron_ids_round_trip() {
        assert_eq!(
            parse_anacron_id("anacron:cron.daily").unwrap(),
            "cron.daily"
        );
        assert_eq!(parse_anacron_id("cron.daily").unwrap(), "cron.daily");
        assert_eq!(
            TaskId::Anacron("cron.daily".into()).to_id(),
            "anacron:cron.daily"
        );
        for id in ["anacron:", "", "anacron:../etc/passwd", "job name"] {
            assert!(parse_anacron_id(id).is_err(), "{id} must be rejected");
        }
    }

    #[test]
    fn launchd_ids_round_trip() {
        assert_eq!(
            parse_launchd_id("com.example.runner").unwrap(),
            "com.example.runner"
        );
        assert_eq!(
            TaskId::Launchd("com.example.runner".into()).to_id(),
            "com.example.runner"
        );
        for id in ["", "com example", "com/example", "com;rm"] {
            assert!(parse_launchd_id(id).is_err(), "{id} must be rejected");
        }
    }

    #[test]
    fn parse_task_id_dispatches_on_kind() {
        let cases: &[(TaskSourceKind, &str)] = &[
            (TaskSourceKind::Cron, "user:alice:4"),
            (TaskSourceKind::Systemd, "user/radar-daily.timer"),
            (TaskSourceKind::At, "at:7"),
            (TaskSourceKind::Anacron, "anacron:cron.daily"),
            (TaskSourceKind::Launchd, "com.example.runner"),
        ];
        for (kind, id) in cases {
            let parsed = parse_task_id(*kind, id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(parsed.kind(), *kind);
            assert_eq!(&parsed.to_id(), id);
        }
        let err = parse_task_id(TaskSourceKind::Systemd, "user/foo.service")
            .expect_err("must reject a service id");
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }
}
