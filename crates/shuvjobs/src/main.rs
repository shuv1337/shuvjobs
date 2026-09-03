//! Binary entry point. The only place `shuvjobs-adapters` and `shuvjobs-tui` meet.

/// Write to stdout, exiting quietly when the reader has gone away
/// (`shuvjobs list | head`) instead of panicking on a broken pipe.
fn write_stdout(args: std::fmt::Arguments<'_>, newline: bool) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let result = if newline {
        lock.write_fmt(args).and_then(|_| lock.write_all(b"\n"))
    } else {
        lock.write_fmt(args).and_then(|_| lock.flush())
    };
    if let Err(e) = result {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
    }
}

macro_rules! outln {
    ($($arg:tt)*) => { $crate::write_stdout(format_args!($($arg)*), true) };
}
macro_rules! out {
    ($($arg:tt)*) => { $crate::write_stdout(format_args!($($arg)*), false) };
}

mod cli;
mod ops;
mod remote;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use shuvjobs_core::export::{self, ExportTask};
use shuvjobs_core::manage::MutationOutcome;
use shuvjobs_core::{Op, ScheduledTask, TaskSourceKind};
use shuvjobs_tui::{MutateFn, RunOptions, Stage};

use crate::cli::{Cli, Command, EditArgs, Global, IdArgs};
use crate::ops::{CliError, ErrorReport, Report, Session};

/// `Send`: the TUI runs collection on a background worker thread.
type RefreshFn = Box<dyn FnMut() -> Result<Vec<ScheduledTask>> + Send>;

fn main() {
    // Parse before anything else so `--json` is known when an error has
    // to be reported; clap's own errors carry their own exit code.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    let json = cli.global.json;
    // What was being attempted, for the JSON error report. Filled in as
    // soon as the subcommand knows; still `None` if we failed earlier.
    let mut context = OpContext::default();
    let code = match real_main(cli, &mut context) {
        Ok(()) => 0,
        Err(err) => {
            report_error(&err, json, &context);
            ops::exit_code(&err)
        }
    };
    std::process::exit(code);
}

/// The `op` and `id` fields of the JSON error report.
#[derive(Debug, Default)]
struct OpContext {
    op: Option<&'static str>,
    id: Option<String>,
}

fn real_main(cli: Cli, context: &mut OpContext) -> Result<()> {
    cli.validate().map_err(CliError::Usage)?;
    let global = cli.global.clone();
    match cli.command {
        // No subcommand: the historical behaviour, TUI or a JSON dump.
        None if global.json => list(&global),
        None => run_tui(&global, cli.refresh),
        Some(Command::List) => list(&global),
        Some(Command::Show(args)) => show(&global, &args, context),
        Some(Command::Add(args)) => {
            context.op = Some("add");
            let session = Session::open(&global)?;
            run_mutation(&session, &global, Op::Create(args.to_spec()?), context)
        }
        Some(Command::Edit(args)) => edit(&global, &args, context),
        Some(Command::Rm(args)) => remove(&global, &args, context),
        Some(Command::Enable(args)) => set_enabled(&global, &args, true, context),
        Some(Command::Disable(args)) => set_enabled(&global, &args, false, context),
    }
}

fn list(global: &Global) -> Result<()> {
    let session = Session::open(global)?;
    let tasks = session.collect()?;
    if global.json {
        outln!("{}", export::serialize_tasks(&tasks)?);
    } else {
        ops::print_table(&tasks);
    }
    Ok(())
}

fn show(global: &Global, args: &IdArgs, context: &mut OpContext) -> Result<()> {
    context.op = Some("show");
    context.id = Some(args.id.clone());
    let session = Session::open(global)?;
    let task = session.resolve(&args.id, args.source.map(TaskSourceKind::from))?;
    if global.json {
        let export = ExportTask::from(&task);
        outln!("{}", serde_json::to_string_pretty(&export)?);
    } else {
        ops::print_task(&task);
    }
    Ok(())
}

fn edit(global: &Global, args: &EditArgs, context: &mut OpContext) -> Result<()> {
    context.op = Some("edit");
    context.id = Some(args.target.id.clone());
    let session = Session::open(global)?;
    let existing = session.resolve(
        &args.target.id,
        args.target.source.map(TaskSourceKind::from),
    )?;
    let spec = ops::merge_edit(&existing, args)?;
    run_mutation(
        &session,
        global,
        Op::Update {
            id: existing.id.clone(),
            source: existing.source,
            spec,
        },
        context,
    )
}

fn remove(global: &Global, args: &IdArgs, context: &mut OpContext) -> Result<()> {
    context.op = Some("rm");
    context.id = Some(args.id.clone());
    let session = Session::open(global)?;
    let task = session.resolve(&args.id, args.source.map(TaskSourceKind::from))?;
    // A dry run changes nothing, so there is nothing to confirm.
    if !global.dry_run {
        let prompt = format!(
            "delete {} task {} [{}]?",
            task.source, task.id, task.command
        );
        if !ops::confirm(&prompt, global.yes)? {
            return Err(CliError::Aborted.into());
        }
    }
    run_mutation(
        &session,
        global,
        Op::Delete {
            id: task.id.clone(),
            source: task.source,
        },
        context,
    )
}

fn set_enabled(
    global: &Global,
    args: &IdArgs,
    enabled: bool,
    context: &mut OpContext,
) -> Result<()> {
    context.op = Some(if enabled { "enable" } else { "disable" });
    context.id = Some(args.id.clone());
    let session = Session::open(global)?;
    let task = session.resolve(&args.id, args.source.map(TaskSourceKind::from))?;
    run_mutation(
        &session,
        global,
        Op::SetEnabled {
            id: task.id.clone(),
            source: task.source,
            enabled,
        },
        context,
    )
}

/// The shared tail of every mutating subcommand: plan, maybe apply,
/// then report in whichever format the operator asked for.
fn run_mutation(session: &Session, global: &Global, op: Op, context: &mut OpContext) -> Result<()> {
    context.op = Some(op.verb());
    if let Some(id) = op.id() {
        context.id = Some(id.to_string());
    }
    let mut backups: HashMap<String, String> = HashMap::new();
    let outcome = if session.dry_run {
        session.plan(&op)?
    } else {
        match session.apply(&op, &mut backups) {
            Ok(outcome) => outcome,
            Err(err) => {
                // The backups are the recovery path when an apply dies
                // halfway, so say where they are even in JSON mode.
                for (path, saved) in &backups {
                    eprintln!("backup of {path}: {saved}");
                }
                return Err(err);
            }
        }
    };

    if global.json {
        let report = Report::new(
            &op,
            &outcome,
            session.host_label(),
            session.dry_run,
            session.policy(),
            &backups,
        );
        outln!("{}", serde_json::to_string_pretty(&report)?);
    } else if session.dry_run {
        out!("{}", session.render_plan(&outcome));
    } else {
        print_success(&op, &outcome, &backups);
    }
    Ok(())
}

fn print_success(op: &Op, outcome: &MutationOutcome, backups: &HashMap<String, String>) {
    let id = outcome
        .id
        .as_deref()
        .or_else(|| op.id())
        .unwrap_or("(unnamed)");
    outln!("{} {} task {id}", past_tense(op), op.source());
    for change in &outcome.changes {
        match change {
            shuvjobs_core::Change::WriteFile { path, .. }
            | shuvjobs_core::Change::RemoveFile { path, .. } => {
                let verb = if matches!(change, shuvjobs_core::Change::RemoveFile { .. }) {
                    "removed"
                } else {
                    "wrote"
                };
                match backups.get(path) {
                    Some(saved) => outln!("  {verb} {path} (backup {saved})"),
                    None => outln!("  {verb} {path}"),
                }
            }
            shuvjobs_core::Change::Command { cmd, .. } => outln!("  ran {cmd}"),
        }
    }
    for note in &outcome.notes {
        outln!("  note: {note}");
    }
}

fn past_tense(op: &Op) -> &'static str {
    match op {
        Op::Create(_) => "added",
        Op::Update { .. } => "edited",
        Op::Delete { .. } => "removed",
        Op::SetEnabled { enabled: true, .. } => "enabled",
        Op::SetEnabled { enabled: false, .. } => "disabled",
    }
}

fn report_error(err: &anyhow::Error, json: bool, context: &OpContext) {
    if json {
        let report = ErrorReport::new(err, context.op, context.id.clone());
        match serde_json::to_string_pretty(&report) {
            Ok(text) => outln!("{text}"),
            Err(_) => eprintln!("error: {err:#}"),
        }
    } else {
        eprintln!("error: {err:#}");
    }
}

/// The TUI shares one [`Session`] with its worker thread: the same host,
/// the same writers, and over SSH the same multiplex master, so opening
/// the form does not start a second connection.
fn run_tui(global: &Global, refresh_secs: Option<u64>) -> Result<()> {
    let session = Arc::new(Session::open(global)?);
    let initial = session.collect()?;

    let collector = Arc::clone(&session);
    let refresh: RefreshFn = Box::new(move || collector.collect());
    let mutator = Arc::clone(&session);
    let mutate: MutateFn = Box::new(move |op, stage| match stage {
        Stage::Plan => mutator.plan(&op),
        Stage::Apply => {
            // The paths are recorded but not surfaced: the TUI has one
            // header line, and `--json` on the CLI is where a backup
            // list belongs.
            let mut backups = HashMap::new();
            mutator.apply(&op, &mut backups)
        }
    });

    shuvjobs_tui::run(RunOptions {
        initial,
        refresh: Some(refresh),
        mutate: Some(mutate),
        refresh_secs,
        dry_run: global.dry_run,
    })?;
    Ok(())
}
