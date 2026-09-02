//! Spawning processes with an optional stdin payload.
//!
//! `std::process` has no "write this to stdin and collect the output"
//! helper, and doing it naively deadlocks: a child that fills its stdout
//! pipe blocks while we are still blocked writing its stdin. So the
//! payload goes out on its own thread while the parent drains stdout and
//! stderr through `wait_with_output`.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use shuvjobs_core::host::CmdOutput;

/// Run `cmd` to completion, optionally feeding `stdin` to it.
///
/// A non-zero exit is `Ok`; only a spawn failure is `Err`. Without a
/// payload the child's stdin is `/dev/null`, never inherited, so a
/// command that decides to read never eats the operator's terminal.
pub fn run_process(cmd: &mut Command, stdin: Option<&[u8]>) -> std::io::Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let Some(payload) = stdin else {
        cmd.stdin(Stdio::null());
        return cmd.spawn()?.wait_with_output();
    };

    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut pipe = child
        .stdin
        .take()
        .expect("stdin was configured as a pipe just above");
    let payload = payload.to_vec();
    let writer = std::thread::spawn(move || {
        // A child that exits without reading everything (`head`, or any
        // command that fails early) gives us EPIPE; that is the child's
        // exit status to report, not an error of ours.
        let _ = pipe.write_all(&payload);
        let _ = pipe.flush();
        drop(pipe);
    });
    let output = child.wait_with_output();
    let _ = writer.join();
    output
}

/// Narrow a finished process to the host-level view of it.
pub fn to_cmd_output(out: Output) -> CmdOutput {
    CmdOutput {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_round_trips_through_cat() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("cat");
        let out = run_process(&mut cmd, Some(b"hello stdin\n")).expect("spawn");
        let out = to_cmd_output(out);
        assert_eq!(out.code, Some(0));
        assert_eq!(out.stdout_str(), "hello stdin\n");
        assert!(out.success());
    }

    #[test]
    fn large_stdin_does_not_deadlock() {
        // Bigger than a pipe buffer in both directions, so a naive
        // write-then-wait implementation would hang here.
        let payload = "x".repeat(512 * 1024);
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("cat");
        let out = to_cmd_output(run_process(&mut cmd, Some(payload.as_bytes())).expect("spawn"));
        assert_eq!(out.stdout.len(), payload.len());
    }

    #[test]
    fn non_zero_exit_is_reported_not_an_error() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo oops >&2; exit 3");
        let out = to_cmd_output(run_process(&mut cmd, None).expect("spawn"));
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stderr.trim(), "oops");
        assert!(!out.success());
    }

    #[test]
    fn missing_binary_is_an_error() {
        let mut cmd = Command::new("shuvjobs-no-such-binary");
        assert!(run_process(&mut cmd, None).is_err());
    }
}
