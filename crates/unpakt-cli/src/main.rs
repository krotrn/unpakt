mod output;

use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use unpakt_core::Error;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::model::SourceKind;

const EXIT_ERROR: u8 = 1;
const EXIT_REFUSED: u8 = 2;
const EXIT_INTERRUPTED: u8 = 130;

#[derive(Parser)]
#[command(
    name = "unpakt",
    version,
    about = "Install AppImages and tarballs per-user"
)]
struct Cli {
    /// More logging (-v info, -vv debug, -vvv trace); RUST_LOG wins when set
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a package file, a direct download URL or a GitHub repo
    Add {
        input: String,
        /// Install without asking for confirmation
        #[arg(long)]
        yes: bool,
        /// App id (lowercase letters, digits and `-`)
        #[arg(long)]
        id: Option<String>,
        /// Path of the executable inside the package
        #[arg(long)]
        entry: Option<std::path::PathBuf>,
        /// Name of the command placed on PATH
        #[arg(long)]
        command: Option<String>,
        /// Create a launcher entry
        #[arg(long, conflicts_with = "no_desktop")]
        desktop: bool,
        /// Do not create a launcher entry
        #[arg(long)]
        no_desktop: bool,
        /// Do not place a command on PATH
        #[arg(long)]
        no_bin: bool,
        /// Overwrite files that unpakt does not own
        #[arg(long)]
        force: bool,
        /// Regex selecting the release asset to install
        #[arg(long, value_name = "REGEX")]
        pattern: Option<String>,
    },
    /// Show what installing a package would do
    Inspect { input: String },
    /// List installed apps
    List,
    /// Show an installed app
    Info { id: String },
    /// Remove an installed app
    Remove {
        id: String,
        /// Remove without asking for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Update apps to their latest release
    Update {
        /// App to update; all apps when omitted
        id: Option<String>,
        /// Only list available updates
        #[arg(long)]
        check: bool,
    },
    /// Switch an app back to its previous version
    Rollback { id: String },
    /// Check the installation for problems
    Doctor,
}

/// A refusal to proceed, as opposed to a failure.
#[derive(Debug)]
struct NeedsConfirmation(String);

impl std::fmt::Display for NeedsConfirmation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsConfirmation {}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Asks `question` on stderr. Without a terminal there is nobody to ask, so
/// the caller is told to pass `--yes`.
fn confirm(verb: &str, question: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(NeedsConfirmation(format!(
            "refusing to {verb} without confirmation; pass --yes"
        ))
        .into());
    }
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(NeedsConfirmation("aborted".into()).into())
    }
}

/// Interrupt policy. While nothing is being changed a signal ends the process
/// at once. While a mutating engine call runs ("armed"), the first signal only
/// sets the flag the engine polls, so its changes are undone; a second one
/// exits at once.
struct Interrupt {
    cancel: Cancel,
    armed: Arc<AtomicBool>,
}

/// Keeps the interrupt policy armed until dropped.
struct Armed<'a>(&'a AtomicBool);

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Interrupt {
    fn install() -> Interrupt {
        let cancel = Cancel::new();
        let armed = Arc::new(AtomicBool::new(false));
        let (handler_cancel, handler_armed) = (cancel.clone(), Arc::clone(&armed));
        let installed = ctrlc::set_handler(move || {
            if !handler_armed.load(Ordering::SeqCst) || handler_cancel.is_cancelled() {
                eprintln!("interrupted");
                std::process::exit(i32::from(EXIT_INTERRUPTED));
            }
            handler_cancel.cancel();
        });
        if let Err(e) = installed {
            eprintln!("warning: cannot install the interrupt handler: {e}");
        }
        Interrupt { cancel, armed }
    }

    /// Arms the policy; call before the engine starts changing anything.
    fn arm(&self) -> Armed<'_> {
        self.armed.store(true, Ordering::SeqCst);
        Armed(&self.armed)
    }
}

fn run(command: Command) -> Result<()> {
    let interrupt = Interrupt::install();
    let mut engine = Engine::open_default()?.with_cancel(interrupt.cancel.clone());
    match command {
        Command::Add {
            input,
            yes,
            id,
            entry,
            command,
            desktop,
            no_desktop,
            no_bin,
            force,
            pattern,
        } => {
            let desktop = match (desktop, no_desktop) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let opts = AddOpts {
                id,
                entry,
                command,
                desktop,
                no_bin,
                force,
                pattern,
            };
            let plan = engine.inspect(&input, opts)?;
            output::print_summary(&plan);
            if !yes && let Err(e) = confirm("install", "Install?") {
                engine.discard(plan);
                return Err(e);
            }
            let _armed = interrupt.arm();
            let app = engine.install(plan)?;
            println!("installed {} {}", app.id, app.current);
        }
        Command::Inspect { input } => {
            let plan = engine.inspect(&input, AddOpts::default())?;
            output::print_summary(&plan);
            engine.discard(plan);
        }
        Command::List => output::print_list(&engine.list()?),
        Command::Info { id } => {
            let (app, versions, files) = engine.get(&id)?;
            output::print_info(&app, &versions, &files);
        }
        Command::Remove { id, yes } => {
            engine.get(&id)?;
            if !yes {
                confirm("remove", &format!("Remove {id}?"))?;
            }
            let _armed = interrupt.arm();
            engine.remove(&id)?;
            println!("removed {id}");
        }
        Command::Update { id, check } => {
            if check {
                let found = engine.check_updates(id.as_deref())?;
                output::print_updates(&found.updates);
                report_failed_checks(&found.failed)?;
            } else {
                update_apps(&mut engine, &interrupt, id.as_deref())?;
            }
        }
        Command::Rollback { id } => {
            let _armed = interrupt.arm();
            let label = engine.rollback(&id)?;
            println!("{label}");
        }
        Command::Doctor => output::print_checks(&engine.doctor()),
    }
    Ok(())
}

/// Updates one app, or every app that has an update. One app failing does not
/// stop the others; the run still fails.
fn update_apps(engine: &mut Engine, interrupt: &Interrupt, id: Option<&str>) -> Result<()> {
    if let Some(id) = id {
        let old = engine.get(id)?.0.current;
        let _armed = interrupt.arm();
        report_update(id, &old, engine.update(id)?);
        return Ok(());
    }
    let check = engine.check_updates(None)?;
    let mut failed = 0;
    for app in engine.list()? {
        if let Some((_, e)) = check.failed.iter().find(|(f, _)| *f == app.id) {
            eprintln!("{}: check failed: {e}", app.id);
            failed += 1;
            continue;
        }
        let Some(info) = check.updates.iter().find(|u| u.id == app.id) else {
            if app.source.kind != SourceKind::LocalFile {
                println!("{}: up to date", app.id);
            }
            continue;
        };
        let outcome = {
            let _armed = interrupt.arm();
            engine.update(&app.id)
        };
        match outcome {
            Ok(new) => report_update(&app.id, &info.current, new),
            Err(e) => note_update_failure(&app.id, e, &mut failed)?,
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} update(s) failed");
    }
    Ok(())
}

/// Records one app's failed update. An interruption is not a failure to count:
/// it ends the whole run.
fn note_update_failure(id: &str, e: Error, failed: &mut usize) -> Result<()> {
    if matches!(e, Error::Interrupted) {
        return Err(e.into());
    }
    eprintln!("error: {id}: {e:#}");
    *failed += 1;
    Ok(())
}

fn report_failed_checks(failed: &[(String, Error)]) -> Result<()> {
    for (id, e) in failed {
        eprintln!("{id}: check failed: {e}");
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} update check(s) failed", failed.len())
    }
}

fn report_update(id: &str, old: &str, new: Option<String>) {
    match new {
        Some(new) => println!("{id}: {old} -> {new}"),
        None => println!("{id}: up to date"),
    }
}

fn exit_code(err: &anyhow::Error) -> u8 {
    if matches!(err.downcast_ref::<Error>(), Some(Error::Interrupted)) {
        return EXIT_INTERRUPTED;
    }
    let refused = err.is::<NeedsConfirmation>()
        || matches!(err.downcast_ref::<Error>(), Some(Error::Refused(_)));
    if refused { EXIT_REFUSED } else { EXIT_ERROR }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if err.is::<NeedsConfirmation>() {
                eprintln!("{err}");
            } else {
                eprintln!("error: {err:#}");
            }
            ExitCode::from(exit_code(&err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_update_stops_the_run() {
        let mut failed = 0;
        let err = note_update_failure("a", Error::Interrupted, &mut failed).unwrap_err();
        assert_eq!(exit_code(&err), EXIT_INTERRUPTED);
        assert_eq!(failed, 0);
    }

    #[test]
    fn other_update_failures_are_counted_and_the_run_continues() {
        let mut failed = 0;
        note_update_failure("a", Error::NotFound("x".into()), &mut failed).unwrap();
        assert_eq!(failed, 1);
    }
}
