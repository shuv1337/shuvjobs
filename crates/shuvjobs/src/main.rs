//! Binary entry point. The only place `shuvjobs-adapters` and `shuvjobs-tui` meet.

mod remote;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use shuvjobs_adapters::{AnacronAdapter, AtAdapter, CronAdapter, LaunchdAdapter, SystemdAdapter};
use shuvjobs_core::{export, Error, ScheduledTask, TaskSource};
use shuvjobs_tui::RunOptions;

use crate::remote::RemoteCollector;

/// `Send`: the TUI runs collection on a background worker thread.
type RefreshFn = Box<dyn FnMut() -> Result<Vec<ScheduledTask>> + Send>;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "shuvjobs",
    about = "ShuvJobs — inspect cron, systemd timer, at, anacron, and launchd jobs in one table",
    version
)]
struct Cli {
    /// Print collected tasks as JSON to stdout and exit (no TUI).
    #[arg(long)]
    json: bool,

    /// Collect from a remote host over SSH (e.g. `alice@server.example.com`).
    /// Key auth must be set up — shuvjobs runs ssh in BatchMode and never prompts.
    #[arg(long, value_name = "USER@HOST")]
    host: Option<String>,

    /// SSH port for `--host`.
    #[arg(long, requires = "host")]
    port: Option<u16>,

    /// SSH private key for `--host`.
    #[arg(long, requires = "host", value_name = "PATH")]
    key: Option<PathBuf>,

    /// Run privileged scheduler commands and file writes through `sudo -n`.
    /// Requires passwordless sudo on the target host; without it, operations
    /// that need root fail early instead of prompting.
    #[arg(long)]
    sudo: bool,

    /// Re-collect and redraw every N seconds.
    #[arg(long, value_name = "SECONDS")]
    refresh: Option<u64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.json {
        let tasks = collect_once(&cli)?;
        println!("{}", export::serialize_tasks(&tasks)?);
        return Ok(());
    }

    let (initial, refresh) = collect_with_refresh(&cli)?;
    shuvjobs_tui::run(RunOptions {
        initial,
        refresh: Some(refresh),
        refresh_secs: cli.refresh,
    })?;
    Ok(())
}

fn collect_once(cli: &Cli) -> Result<Vec<ScheduledTask>> {
    if let Some(host) = &cli.host {
        let collector =
            RemoteCollector::new(host.clone(), cli.port, cli.key.clone()).with_sudo(cli.sudo);
        collector
            .collect()
            .with_context(|| format!("collecting from {host}"))
    } else {
        Ok(collect_local())
    }
}

/// Construct one [`RemoteCollector`] up front and move it into the
/// refresh closure so the SSH multiplex master persists across reloads.
fn collect_with_refresh(cli: &Cli) -> Result<(Vec<ScheduledTask>, RefreshFn)> {
    if let Some(host) = &cli.host {
        let collector =
            RemoteCollector::new(host.clone(), cli.port, cli.key.clone()).with_sudo(cli.sudo);
        let initial = collector
            .collect()
            .with_context(|| format!("collecting from {host}"))?;
        let refresh: RefreshFn = Box::new(move || collector.collect());
        Ok((initial, refresh))
    } else {
        let initial = collect_local();
        let refresh: RefreshFn = Box::new(|| Ok(collect_local()));
        Ok((initial, refresh))
    }
}

fn collect_local() -> Vec<ScheduledTask> {
    let sources: Vec<Box<dyn TaskSource>> = vec![
        Box::new(SystemdAdapter::new()),
        Box::new(CronAdapter::new()),
        Box::new(AtAdapter::new()),
        Box::new(AnacronAdapter::new()),
        Box::new(LaunchdAdapter::new()),
    ];

    let mut tasks: Vec<ScheduledTask> = Vec::new();
    for source in &sources {
        match source.collect() {
            Ok(mut found) => tasks.append(&mut found),
            Err(Error::Unavailable(_)) => continue,
            Err(e) => eprintln!("warning: {} adapter failed: {e}", source.kind().as_str()),
        }
    }
    tasks
}
