//! The command-line surface.
//!
//! Parsing only: every type here turns operator text into the domain
//! types `ops.rs` and the writers already speak, and nothing here
//! touches a host. That split is what lets the parse tests below pin
//! the whole interface without a machine to run against.

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use shuvjobs_core::manage::{self, JobScope, JobSpec};
use shuvjobs_core::{Result, TaskSourceKind};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "shuvjobs",
    about = "ShuvJobs — inspect and manage cron, systemd timer, at, anacron, and launchd jobs in one table",
    version
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    /// Re-collect and redraw every N seconds. TUI only.
    #[arg(long, value_name = "SECONDS")]
    pub refresh: Option<u64>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// The one rule clap cannot express: `--refresh` drives the TUI's
    /// redraw loop, and a subcommand never enters the TUI, so the two
    /// together are a mistake rather than a no-op.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.refresh.is_some() && self.command.is_some() {
            return Err("--refresh only applies to the TUI: drop it or drop the subcommand".into());
        }
        Ok(())
    }
}

/// Flags that mean the same thing wherever they appear, so they are
/// accepted both before and after the subcommand.
#[derive(Args, Debug, Clone)]
pub struct Global {
    /// Emit JSON instead of text: the task list, or the mutation report.
    #[arg(long, global = true)]
    pub json: bool,

    /// Work against a remote host over SSH (e.g. `alice@server.example.com`).
    /// Key auth must be set up — shuvjobs runs ssh in BatchMode and never prompts.
    #[arg(long, global = true, value_name = "USER@HOST")]
    pub host: Option<String>,

    /// SSH port for `--host`.
    #[arg(long, global = true, requires = "host")]
    pub port: Option<u16>,

    /// SSH private key for `--host`.
    #[arg(long, global = true, requires = "host", value_name = "PATH")]
    pub key: Option<PathBuf>,

    /// Run privileged scheduler commands and file writes through `sudo -n`.
    /// Requires passwordless sudo on the target host; without it, operations
    /// that need root fail early instead of prompting.
    #[arg(long, global = true)]
    pub sudo: bool,

    /// Show what would change and exit without touching anything.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Answer the deletion prompt with yes.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// List every job the host's schedulers report.
    List,
    /// Show one job in full.
    Show(IdArgs),
    /// Create a job.
    Add(AddArgs),
    /// Change an existing job.
    Edit(EditArgs),
    /// Delete a job.
    Rm(IdArgs),
    /// Enable a job so its schedule fires.
    Enable(IdArgs),
    /// Disable a job without deleting it.
    Disable(IdArgs),
}

/// A job, named by the id `list` printed. `--source` only matters when
/// two schedulers happen to mint the same id.
#[derive(Args, Debug, Clone)]
pub struct IdArgs {
    /// Job id as shown in the ID column.
    pub id: String,

    /// Disambiguate when the id exists under more than one scheduler.
    #[arg(long, value_enum)]
    pub source: Option<SourceArg>,
}

#[derive(Args, Debug, Clone)]
pub struct AddArgs {
    /// Which scheduler should own the job.
    #[arg(long, value_enum)]
    pub source: SourceArg,

    /// Schedule in the source's own syntax, or `30m`/`2h`/`1d`.
    #[arg(long)]
    pub schedule: String,

    /// The command line to run.
    #[arg(long)]
    pub command: String,

    /// Unit name, `cron.d` file name, anacron job id, or launchd label.
    #[arg(long)]
    pub name: Option<String>,

    /// Run the job as this user. System scope only.
    #[arg(long)]
    pub user: Option<String>,

    /// Where the job lives: the invoking user's own scheduler, or the machine's.
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,

    /// Create the job without enabling it.
    #[arg(long)]
    pub disabled: bool,
}

impl AddArgs {
    /// The operator's words as a [`JobSpec`], validated before any host
    /// is contacted.
    pub fn to_spec(&self) -> Result<JobSpec> {
        let source: TaskSourceKind = self.source.into();
        let schedule = manage::parse_schedule(source, &self.schedule)?;
        let mut spec = JobSpec::new(source, schedule, self.command.clone());
        spec.name = self.name.clone();
        spec.user = self.user.clone();
        spec.scope = self.scope.into();
        spec.enabled = !self.disabled;
        spec.validate()?;
        Ok(spec)
    }
}

/// At least one field must change, otherwise `edit` would be a no-op
/// that still rewrites the file.
#[derive(Args, Debug, Clone)]
#[command(group(
    ArgGroup::new("edits")
        .required(true)
        .multiple(true)
        .args(["schedule", "command", "name", "user"])
))]
pub struct EditArgs {
    #[command(flatten)]
    pub target: IdArgs,

    /// New schedule, in the source's own syntax.
    #[arg(long)]
    pub schedule: Option<String>,

    /// New command line.
    #[arg(long)]
    pub command: Option<String>,

    /// New unit name, file name, job id, or label.
    #[arg(long)]
    pub name: Option<String>,

    /// New user to run the job as.
    #[arg(long)]
    pub user: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum SourceArg {
    Cron,
    Systemd,
    At,
    Anacron,
    Launchd,
}

impl From<SourceArg> for TaskSourceKind {
    fn from(arg: SourceArg) -> Self {
        match arg {
            SourceArg::Cron => TaskSourceKind::Cron,
            SourceArg::Systemd => TaskSourceKind::Systemd,
            SourceArg::At => TaskSourceKind::At,
            SourceArg::Anacron => TaskSourceKind::Anacron,
            SourceArg::Launchd => TaskSourceKind::Launchd,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum ScopeArg {
    User,
    System,
}

impl From<ScopeArg> for JobScope {
    fn from(arg: ScopeArg) -> Self {
        match arg {
            ScopeArg::User => JobScope::User,
            ScopeArg::System => JobScope::System,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = parse(&["shuvjobs"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.global.json);
        cli.validate().unwrap();
    }

    #[test]
    fn json_alone_still_means_the_list() {
        let cli = parse(&["shuvjobs", "--json"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.global.json);
    }

    #[test]
    fn json_is_accepted_after_a_subcommand() {
        let cli = parse(&["shuvjobs", "list", "--json"]).unwrap();
        assert!(matches!(cli.command, Some(Command::List)));
        assert!(cli.global.json);
    }

    #[test]
    fn global_flags_are_accepted_before_a_subcommand() {
        let cli = parse(&["shuvjobs", "--dry-run", "--sudo", "list"]).unwrap();
        assert!(cli.global.dry_run);
        assert!(cli.global.sudo);
    }

    #[test]
    fn add_requires_a_schedule() {
        let err = parse(&["shuvjobs", "add", "--source", "cron", "--command", "true"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn add_requires_a_source() {
        let err = parse(&[
            "shuvjobs",
            "add",
            "--schedule",
            "0 9 * * *",
            "--command",
            "true",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn add_defaults_to_user_scope_and_enabled() {
        let cli = parse(&[
            "shuvjobs",
            "add",
            "--source",
            "systemd",
            "--schedule",
            "0 9 * * *",
            "--command",
            "true",
        ])
        .unwrap();
        let Some(Command::Add(args)) = cli.command else {
            panic!("expected add");
        };
        assert_eq!(args.scope, ScopeArg::User);
        assert!(!args.disabled);
        let spec = args.to_spec().unwrap();
        assert_eq!(spec.scope, JobScope::User);
        assert!(spec.enabled);
        assert_eq!(spec.source, TaskSourceKind::Systemd);
    }

    #[test]
    fn add_rejects_a_schedule_the_source_cannot_express() {
        let cli = parse(&[
            "shuvjobs",
            "add",
            "--source",
            "anacron",
            "--schedule",
            "@reboot",
            "--command",
            "true",
        ])
        .unwrap();
        let Some(Command::Add(args)) = cli.command else {
            panic!("expected add");
        };
        assert!(args.to_spec().is_err());
    }

    #[test]
    fn edit_needs_at_least_one_field() {
        let err = parse(&["shuvjobs", "edit", "x"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn edit_accepts_one_field() {
        let cli = parse(&["shuvjobs", "edit", "x", "--schedule", "hourly"]).unwrap();
        let Some(Command::Edit(args)) = cli.command else {
            panic!("expected edit");
        };
        assert_eq!(args.target.id, "x");
        assert_eq!(args.schedule.as_deref(), Some("hourly"));
        assert!(args.command.is_none());
    }

    #[test]
    fn rm_accepts_a_leading_dash_id_after_the_separator() {
        let cli = parse(&["shuvjobs", "rm", "--", "-x"]).unwrap();
        let Some(Command::Rm(args)) = cli.command else {
            panic!("expected rm");
        };
        assert_eq!(args.id, "-x");
        assert!(args.source.is_none());
    }

    #[test]
    fn port_without_host_is_an_error() {
        let err = parse(&["shuvjobs", "--port", "22"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn key_without_host_is_an_error() {
        let err = parse(&["shuvjobs", "--key", "/tmp/id"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn refresh_with_a_subcommand_is_rejected() {
        let cli = parse(&["shuvjobs", "--refresh", "5", "list"]).unwrap();
        assert!(cli.validate().is_err());
    }

    #[test]
    fn refresh_without_a_subcommand_is_fine() {
        let cli = parse(&["shuvjobs", "--refresh", "5"]).unwrap();
        assert_eq!(cli.refresh, Some(5));
        cli.validate().unwrap();
    }

    #[test]
    fn show_takes_an_optional_source() {
        let cli = parse(&["shuvjobs", "show", "user/x.timer", "--source", "systemd"]).unwrap();
        let Some(Command::Show(args)) = cli.command else {
            panic!("expected show");
        };
        assert_eq!(args.source, Some(SourceArg::Systemd));
        assert_eq!(
            TaskSourceKind::from(args.source.unwrap()),
            TaskSourceKind::Systemd
        );
    }

    #[test]
    fn yes_has_a_short_form() {
        let cli = parse(&["shuvjobs", "rm", "x", "-y"]).unwrap();
        assert!(cli.global.yes);
    }

    #[test]
    fn scope_arg_maps_to_the_domain_scope() {
        assert_eq!(JobScope::from(ScopeArg::System), JobScope::System);
        assert_eq!(JobScope::from(ScopeArg::User), JobScope::User);
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
