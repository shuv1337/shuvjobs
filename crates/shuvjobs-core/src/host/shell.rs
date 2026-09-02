//! POSIX shell quoting and the pinned scripts every host runs.
//!
//! The remote host's login shell may be fish or zsh, so nothing here may
//! rely on bashisms, and every script is wrapped in `sh -c '…'` before it
//! is sent. The local sudo path runs the same strings, so the privileged
//! local and remote code paths are byte-for-byte identical.

/// Exit status meaning "the path is not there", as distinct from "the
/// command failed". Picked out of the way of the usual small exit codes
/// and of `sh`'s 126/127, so we never confuse absence with a broken shell.
pub const ABSENT_EXIT: i32 = 66;

/// POSIX-safe single-quoting: wraps `s` in `'...'` and escapes any
/// embedded `'` by closing the literal, inserting `\'`, and reopening.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Whitelist for usernames before interpolating them into a shell command.
pub fn shell_safe_username(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

pub fn shell_safe_filename(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Wrap a script so it runs under `sh` regardless of the login shell.
pub fn sh_c(script: &str) -> String {
    format!("sh -c {}", shell_quote(script))
}

/// Render an argv as a shell command line, quoting every word.
pub fn cmd_line(argv: &[&str]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Print the file, or exit [`ABSENT_EXIT`] when it does not exist.
pub fn read_file_script(path: &str) -> String {
    let p = shell_quote(path);
    format!("if test -e {p}; then cat {p}; else exit {ABSENT_EXIT}; fi")
}

/// Succeed when the path exists, fail otherwise.
pub fn exists_script(path: &str) -> String {
    format!("test -e {}", shell_quote(path))
}

/// One name per line, including dotfiles, or [`ABSENT_EXIT`] when the
/// directory does not exist.
pub fn list_dir_script(path: &str) -> String {
    let p = shell_quote(path);
    format!("if test -d {p}; then ls -1A {p}; else exit {ABSENT_EXIT}; fi")
}

/// Remove the file, or exit [`ABSENT_EXIT`] when it was already gone.
pub fn remove_file_script(path: &str) -> String {
    let p = shell_quote(path);
    format!("if test -e {p}; then rm -f {p}; else exit {ABSENT_EXIT}; fi")
}

pub fn mkdir_script(path: &str) -> String {
    format!("mkdir -p {}", shell_quote(path))
}

/// Read the contents from stdin into a temp file *in the target
/// directory*, so the final `mv` is a same-filesystem rename and readers
/// never see a partial file. Under sudo the result is root-owned, which
/// `cron.d`, unit directories, and `LaunchDaemons` require.
pub fn write_file_script(path: &str, mode: u32) -> String {
    let p = shell_quote(path);
    format!(
        "d=$(dirname {p}) && t=$(mktemp \"$d/.shuvjobs.XXXXXX\") && \
         {{ cat > \"$t\" && chmod {mode:o} \"$t\" && mv -f \"$t\" {p} || \
         {{ rm -f \"$t\"; exit 1; }}; }}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that would break naive quoting, used to pin every builder.
    const TRICKY: &str = "/tmp/it's here/x";

    #[test]
    fn shell_quote_wraps_in_single_quotes() {
        assert_eq!(shell_quote("simple"), "'simple'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        assert_eq!(shell_quote("foo'bar"), r"'foo'\''bar'");
    }

    #[test]
    fn shell_safe_username_rejects_metacharacters() {
        assert!(shell_safe_username("alice"));
        assert!(shell_safe_username("user_1"));
        assert!(!shell_safe_username("alice; rm -rf /"));
        assert!(!shell_safe_username(""));
    }

    #[test]
    fn shell_safe_filename_rejects_slashes() {
        assert!(shell_safe_filename("cron.daily"));
        assert!(!shell_safe_filename("../etc/passwd"));
        assert!(!shell_safe_filename(""));
    }

    #[test]
    fn sh_c_wraps_the_script() {
        assert_eq!(sh_c("echo hi"), "sh -c 'echo hi'");
        assert_eq!(sh_c("echo 'hi'"), r"sh -c 'echo '\''hi'\'''");
    }

    #[test]
    fn cmd_line_quotes_every_word() {
        assert_eq!(
            cmd_line(&["crontab", "-u", "alice", "-"]),
            "'crontab' '-u' 'alice' '-'"
        );
        assert_eq!(cmd_line(&["echo", "it's"]), r"'echo' 'it'\''s'");
    }

    #[test]
    fn read_file_script_is_pinned() {
        assert_eq!(
            read_file_script("/etc/crontab"),
            "if test -e '/etc/crontab'; then cat '/etc/crontab'; else exit 66; fi"
        );
        assert_eq!(
            read_file_script(TRICKY),
            r"if test -e '/tmp/it'\''s here/x'; then cat '/tmp/it'\''s here/x'; else exit 66; fi"
        );
    }

    #[test]
    fn exists_script_is_pinned() {
        assert_eq!(exists_script("/etc/crontab"), "test -e '/etc/crontab'");
        assert_eq!(exists_script(TRICKY), r"test -e '/tmp/it'\''s here/x'");
    }

    #[test]
    fn list_dir_script_is_pinned() {
        assert_eq!(
            list_dir_script("/etc/cron.d"),
            "if test -d '/etc/cron.d'; then ls -1A '/etc/cron.d'; else exit 66; fi"
        );
        assert_eq!(
            list_dir_script(TRICKY),
            r"if test -d '/tmp/it'\''s here/x'; then ls -1A '/tmp/it'\''s here/x'; else exit 66; fi"
        );
    }

    #[test]
    fn remove_file_script_is_pinned() {
        assert_eq!(
            remove_file_script("/etc/cron.d/x"),
            "if test -e '/etc/cron.d/x'; then rm -f '/etc/cron.d/x'; else exit 66; fi"
        );
        assert_eq!(
            remove_file_script(TRICKY),
            r"if test -e '/tmp/it'\''s here/x'; then rm -f '/tmp/it'\''s here/x'; else exit 66; fi"
        );
    }

    #[test]
    fn mkdir_script_is_pinned() {
        assert_eq!(
            mkdir_script("/etc/systemd/system"),
            "mkdir -p '/etc/systemd/system'"
        );
        assert_eq!(mkdir_script(TRICKY), r"mkdir -p '/tmp/it'\''s here/x'");
    }

    #[test]
    fn write_file_script_is_pinned() {
        assert_eq!(
            write_file_script("/etc/cron.d/x", 0o644),
            "d=$(dirname '/etc/cron.d/x') && t=$(mktemp \"$d/.shuvjobs.XXXXXX\") && \
             { cat > \"$t\" && chmod 644 \"$t\" && mv -f \"$t\" '/etc/cron.d/x' || \
             { rm -f \"$t\"; exit 1; }; }"
        );
        assert_eq!(
            write_file_script(TRICKY, 0o600),
            "d=$(dirname '/tmp/it'\\''s here/x') && t=$(mktemp \"$d/.shuvjobs.XXXXXX\") && \
             { cat > \"$t\" && chmod 600 \"$t\" && mv -f \"$t\" '/tmp/it'\\''s here/x' || \
             { rm -f \"$t\"; exit 1; }; }"
        );
    }
}
