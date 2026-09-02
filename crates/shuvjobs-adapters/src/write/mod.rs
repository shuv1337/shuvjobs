//! The write path: markers, validators, and the per-scheduler writers.
//!
//! Everything a writer puts into somebody's config file is checked here
//! first. The rules are deliberately narrower than what the schedulers
//! themselves accept: a name we generate is also a filename, a unit
//! name, and sometimes a shell word, so it has to be safe in all three.

pub mod calendar;
pub mod cron;

use std::collections::HashMap;
use std::sync::Arc;

use shuvjobs_core::host::Host;
use shuvjobs_core::manage::JobWriter;
use shuvjobs_core::{Error, Result, TaskSourceKind};

/// Every writer this build knows about, keyed by the source they write.
///
/// One `Arc<dyn Host>` is shared by all of them, so a session over SSH
/// keeps exactly one multiplex master however many sources it touches.
pub fn writers_for(host: Arc<dyn Host>) -> HashMap<TaskSourceKind, Box<dyn JobWriter>> {
    let mut writers: HashMap<TaskSourceKind, Box<dyn JobWriter>> = HashMap::new();
    writers.insert(
        TaskSourceKind::Cron,
        Box::new(cron::CronWriter::new(Arc::clone(&host))),
    );
    writers
}

/// Prefix that turns a job line into a comment we can find again.
///
/// The trailing space keeps the rendered line readable, and the whole
/// thing is distinctive enough that no hand-written comment collides
/// with it.
pub const DISABLED_MARKER: &str = "#shuvjobs-disabled# ";

/// First line of every file we create, so update and delete can tell a
/// file they own from one the operator wrote.
pub const MANAGED_MARKER: &str = "# managed by shuvjobs";

/// Longest name we will generate. Comfortably under every scheduler's
/// own limit, and under `NAME_MAX` with any suffix we add.
const MAX_NAME_LEN: usize = 128;

/// Which naming rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    /// `/etc/cron.d/<name>`: run-parts skips names containing a dot, and
    /// cron uses the same rule for the drop-in directory.
    CronD,
    /// A unit name stem; `@` would make it a template instance.
    Systemd,
    /// The anacron job identifier, also a spool filename.
    Anacron,
    /// A launchd label, also the plist filename.
    Launchd,
}

impl NameKind {
    fn what(self) -> &'static str {
        match self {
            Self::CronD => "cron.d file name",
            Self::Systemd => "systemd unit name",
            Self::Anacron => "anacron job id",
            Self::Launchd => "launchd label",
        }
    }
}

/// A command we are willing to put in a scheduler's config file.
///
/// One line only: cron, anacron, and unit files are all line-oriented,
/// so an embedded newline would inject a second entry. A leading `#`
/// would make the whole entry a comment.
pub fn validate_command(cmd: &str) -> Result<()> {
    if cmd.trim().is_empty() {
        return Err(Error::Validation("command must not be empty".into()));
    }
    if cmd.contains('\n') || cmd.contains('\r') {
        return Err(Error::Validation(
            "command must be a single line: newlines are not allowed".into(),
        ));
    }
    if cmd.trim_start().starts_with('#') {
        return Err(Error::Validation(
            "command must not start with `#`: it would be read as a comment".into(),
        ));
    }
    Ok(())
}

/// A name we are willing to use as a file, unit, or label.
pub fn validate_simple_name(name: &str, kind: NameKind) -> Result<()> {
    let what = kind.what();
    let bad = |reason: &str| {
        Err(Error::Validation(format!(
            "invalid {what} `{name}`: {reason}"
        )))
    };

    if name.is_empty() {
        return bad("must not be empty");
    }
    if name.len() > MAX_NAME_LEN {
        return bad(&format!("must be at most {MAX_NAME_LEN} characters"));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("name is not empty");
    if !first.is_ascii_alphanumeric() {
        return bad("must start with a letter or a digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
        return bad("may contain only letters, digits, `_`, `.` and `-`");
    }

    match kind {
        NameKind::CronD => {
            if name.contains('.') {
                return bad("cron ignores files in /etc/cron.d whose name contains a dot");
            }
        }
        NameKind::Systemd => {
            if name.contains('@') {
                return bad("`@` would make this a template unit");
            }
            if name.ends_with(".timer") || name.ends_with(".service") {
                return bad("give the bare name; the unit suffixes are added for you");
            }
        }
        NameKind::Anacron | NameKind::Launchd => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_keys_every_writer_by_its_own_source() {
        let host: Arc<dyn Host> = Arc::new(shuvjobs_core::host::FakeHost::new());
        let writers = writers_for(host);
        assert!(!writers.is_empty());
        for (kind, writer) in &writers {
            assert_eq!(*kind, writer.kind());
        }
    }

    #[test]
    fn markers_are_pinned() {
        assert_eq!(DISABLED_MARKER, "#shuvjobs-disabled# ");
        assert_eq!(MANAGED_MARKER, "# managed by shuvjobs");
    }

    #[test]
    fn commands_must_be_one_non_comment_line() {
        validate_command("/usr/bin/backup --all").unwrap();
        validate_command("echo hi # trailing comment is fine").unwrap();
        for bad in [
            "",
            "   ",
            "echo a\necho b",
            "echo a\r\necho b",
            "# not a command",
            "   # indented comment",
        ] {
            let err = validate_command(bad).expect_err(bad);
            assert!(matches!(err, Error::Validation(_)), "{bad:?} gave {err:?}");
        }
    }

    #[test]
    fn names_follow_the_shared_charset() {
        for kind in [
            NameKind::CronD,
            NameKind::Systemd,
            NameKind::Anacron,
            NameKind::Launchd,
        ] {
            validate_simple_name("backup", kind).unwrap();
            validate_simple_name("backup-2_x", kind).unwrap();
            for bad in [
                "",
                "-leading",
                ".leading",
                "has space",
                "has/slash",
                "semi;colon",
                "unicodé",
                &"x".repeat(MAX_NAME_LEN + 1),
            ] {
                assert!(
                    validate_simple_name(bad, kind).is_err(),
                    "{bad:?} accepted for {kind:?}"
                );
            }
            assert!(validate_simple_name(&"x".repeat(MAX_NAME_LEN), kind).is_ok());
        }
    }

    #[test]
    fn cron_d_rejects_dots() {
        assert!(validate_simple_name("nightly.backup", NameKind::CronD).is_err());
        assert!(validate_simple_name("nightly.backup", NameKind::Anacron).is_ok());
    }

    #[test]
    fn systemd_rejects_templates_and_unit_suffixes() {
        assert!(validate_simple_name("backup@", NameKind::Systemd).is_err());
        assert!(validate_simple_name("backup.timer", NameKind::Systemd).is_err());
        assert!(validate_simple_name("backup.service", NameKind::Systemd).is_err());
        assert!(validate_simple_name("backup.daily", NameKind::Systemd).is_ok());
    }
}
