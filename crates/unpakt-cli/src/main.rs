mod output;
mod selfmgmt;

use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use output::clean_lines;
use tracing_subscriber::EnvFilter;
use unpakt_core::Error;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine, NATIVE_BACKEND, NativeConsent, Route, UpdateOpts};
use unpakt_core::model::{FormatKind, SourceKind};
use unpakt_core::progress::Progress;

const EXIT_ERROR: u8 = 1;
const EXIT_REFUSED: u8 = 2;
const EXIT_INTERRUPTED: u8 = 130;

#[derive(Parser)]
#[command(
    name = "unpakt",
    version,
    about = "Install AppImages, tarballs and deb/rpm/pacman packages per-user"
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
    #[command(
        after_long_help = "Exit status 2 means unpakt refused: a compatibility problem or a declined confirmation.\n\nOn a native install, --yes also agrees to run the package's install scripts as root."
    )]
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
        /// Only consider release assets of this format
        #[arg(long, value_name = "FORMAT", value_parser = parse_format)]
        format: Option<FormatKind>,
        /// Install a package even when it may not run on this machine
        #[arg(long)]
        allow_risky: bool,
        /// Install into your user directory even when the system package manager could install it
        #[arg(long)]
        extract: bool,
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
        /// Only drop unpakt's record of a system-wide app; the package stays installed
        #[arg(long)]
        forget: bool,
    },
    /// Update apps to their latest release
    Update {
        /// App to update; all apps when omitted
        id: Option<String>,
        /// Only list available updates
        #[arg(long)]
        check: bool,
        /// Update even when the new version may not run on this machine
        #[arg(long)]
        allow_risky: bool,
        /// Install native updates without asking
        #[arg(long)]
        yes: bool,
    },
    /// Switch an app back to its previous version
    Rollback { id: String },
    /// Check the installation for problems
    Doctor,
    /// Print the shell completion script for SHELL
    Completion { shell: clap_complete::Shell },
    /// Update or remove unpakt itself
    #[command(name = "self")]
    SelfCmd {
        #[command(subcommand)]
        action: selfmgmt::SelfAction,
    },
}

fn parse_format(s: &str) -> Result<FormatKind, String> {
    s.parse::<FormatKind>().map_err(|e| e.to_string())
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
    let changing = AtomicBool::new(false);
    let removing = matches!(command, Command::Remove { .. });
    let mut engine = Engine::open_default()?
        .with_cancel(interrupt.cancel.clone())
        .with_progress(Arc::new(move |event| match event {
            Progress::WaitingForLock => {
                eprintln!("waiting for another unpakt operation to finish…")
            }
            Progress::WaitingForAuthorization => eprintln!("waiting for authorization…"),
            Progress::SystemChange { .. } if !changing.swap(true, Ordering::SeqCst) => {
                let verb = if removing { "removing" } else { "installing" };
                eprintln!("{verb} with the system package manager…");
            }
            _ => {}
        }));
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
            format,
            allow_risky,
            extract,
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
                format,
                allow_risky,
                extract,
            };
            let mut plan = engine.inspect(&input, opts)?;
            output::print_summary(&plan);
            if let Some(error) = refusal_with_alternative(&plan) {
                engine.discard(plan);
                return Err(error.into());
            }
            let native = plan.route() == Some(Route::Native);
            let question = if native {
                "Install system-wide?"
            } else {
                "Install?"
            };
            if !yes && let Err(e) = confirm("install", question) {
                engine.discard(plan);
                return Err(e);
            }
            plan.native_consent = native;
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
        Command::Remove { id, yes, forget } => {
            let native = engine.get(&id)?.0.backend == NATIVE_BACKEND;
            if forget {
                // An extract app is refused by the engine; no point asking first.
                if native && !yes {
                    confirm(
                        "forget",
                        &format!("Forget {id}? The package stays installed."),
                    )?;
                }
                let _armed = interrupt.arm();
                engine.forget(&id)?;
                println!("forgot {id}");
                return Ok(());
            }
            if !yes {
                let question = if native {
                    format!("Remove {id} from the system?")
                } else {
                    format!("Remove {id}?")
                };
                confirm("remove", &question)?;
            }
            let _armed = interrupt.arm();
            engine.remove(&id)?;
            println!("removed {id}");
        }
        Command::Update {
            id,
            check,
            allow_risky,
            yes,
        } => {
            if check {
                let checkable = match id.as_deref() {
                    Some(id) => {
                        let app = engine.get(id)?.0;
                        if app.source.kind == SourceKind::LocalFile {
                            return Err(Error::Unsupported(format!(
                                "{id} was installed from a local file and has no update source"
                            ))
                            .into());
                        }
                        1
                    }
                    None => engine
                        .list()?
                        .iter()
                        .filter(|a| a.source.kind != SourceKind::LocalFile)
                        .count(),
                };
                let found = engine.check_updates(id.as_deref())?;
                output::print_updates(&found.updates);
                if found.updates.is_empty() && found.failed.is_empty() {
                    match id.as_deref() {
                        Some(id) => println!("{id} is up to date"),
                        None if checkable > 0 => println!("all apps are up to date"),
                        None if engine.list()?.is_empty() => println!("no apps installed"),
                        None => println!("no apps with an update source"),
                    }
                }
                report_failed_checks(&found.failed)?;
            } else {
                update_apps(&mut engine, &interrupt, id.as_deref(), allow_risky, yes)?;
            }
        }
        Command::Rollback { id } => {
            let _armed = interrupt.arm();
            let label = engine.rollback(&id)?;
            println!("{label}");
        }
        Command::Doctor => output::print_checks(&engine.doctor()),
        Command::Completion { .. } | Command::SelfCmd { .. } => {
            unreachable!("handled before the engine opens")
        }
    }
    Ok(())
}

/// Generated from the command definition, so new commands and flags complete
/// without editing a script.
fn print_completion(shell: clap_complete::Shell) {
    clap_complete::generate(shell, &mut Cli::command(), "unpakt", &mut std::io::stdout());
}

/// Updates one app, or every app that has an update. One app failing does not
/// stop the others; the run still fails.
fn update_apps(
    engine: &mut Engine,
    interrupt: &Interrupt,
    id: Option<&str>,
    allow_risky: bool,
    yes: bool,
) -> Result<()> {
    let mut confirmer = native_confirmer(yes);
    if let Some(id) = id {
        let old = engine.get(id)?.0.current;
        let _armed = interrupt.arm();
        let opts = UpdateOpts {
            allow_risky,
            confirm_native: confirmer
                .as_mut()
                .map(|c| &mut **c as &mut dyn FnMut(&NativeConsent) -> bool),
        };
        report_update(id, &old, engine.update_opts(id, opts)?);
        return Ok(());
    }
    if engine.list()?.is_empty() {
        println!("no apps installed");
        return Ok(());
    }
    let check = engine.check_updates(None)?;
    let mut failed = 0;
    let mut refused = false;
    for app in engine.list()? {
        if let Some((_, e)) = check.failed.iter().find(|(f, _)| *f == app.id) {
            eprintln!("{}: check failed: {}", app.id, clean_lines(&e.to_string()));
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
            let opts = UpdateOpts {
                allow_risky,
                confirm_native: confirmer
                    .as_mut()
                    .map(|c| &mut **c as &mut dyn FnMut(&NativeConsent) -> bool),
            };
            engine.update_opts(&app.id, opts)
        };
        match outcome {
            Ok(new) => report_update(&app.id, &info.current, new),
            Err(Error::PackageKitUnavailable(_)) => {
                println!("{}: skipped (PackageKit is not available)", app.id);
            }
            Err(e) => {
                refused |= matches!(
                    e,
                    Error::Refused(_) | Error::NeedsAcceptance(_) | Error::NeedsConsent(_)
                );
                note_update_failure(&app.id, e, &mut failed)?
            }
        }
    }
    match update_failure(failed, refused) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

type NativeConfirmer = Box<dyn FnMut(&NativeConsent) -> bool>;

/// How a native update gets its consent: `--yes` agrees after showing what
/// runs, a terminal asks, and without either the engine refuses.
fn native_confirmer(yes: bool) -> Option<NativeConfirmer> {
    if yes {
        return Some(Box::new(|consent| {
            eprintln!("{}", clean_lines(&consent.message()));
            true
        }));
    }
    if !std::io::stdin().is_terminal() {
        return None;
    }
    Some(Box::new(|consent| {
        eprintln!("{}", clean_lines(&consent.message()));
        confirm("update", "Update system-wide?").is_ok()
    }))
}

/// The plan's refusal, with the release's portable alternative named the way
/// this command line installs it.
fn refusal_with_alternative(plan: &unpakt_core::engine::InstallPlan) -> Option<Error> {
    let hint = plan.alternative.as_ref().map_or_else(String::new, |a| {
        format!(
            "\nThis release also has {}; install it with --format {}",
            a.asset,
            a.format.as_str()
        )
    });
    Some(match plan.refusal()? {
        Error::NeedsAcceptance(m) => Error::NeedsAcceptance(m + &hint),
        Error::Refused(m) => Error::Refused(m + &hint),
        other => other,
    })
}

/// An update run that refused any app exits like a refusal, not a failure.
#[derive(Debug)]
struct UpdatesRefused(String);

impl std::fmt::Display for UpdatesRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UpdatesRefused {}

fn update_failure(failed: usize, refused: bool) -> Option<anyhow::Error> {
    if failed == 0 {
        return None;
    }
    let message = format!("{failed} update(s) failed");
    Some(if refused {
        UpdatesRefused(message).into()
    } else {
        anyhow::anyhow!(message)
    })
}

/// Records one app's failed update. An interruption is not a failure to count:
/// it ends the whole run.
fn note_update_failure(id: &str, e: Error, failed: &mut usize) -> Result<()> {
    if matches!(e, Error::Interrupted) {
        return Err(e.into());
    }
    eprintln!("error: {id}: {}", clean_lines(&format!("{e:#}")));
    if matches!(e, Error::NeedsAcceptance(_)) {
        eprintln!("  run `unpakt update {id} --allow-risky` to accept them");
    }
    if matches!(e, Error::NeedsConsent(_)) {
        eprintln!("  run `unpakt update {id} --yes` to accept it");
    }
    *failed += 1;
    Ok(())
}

fn report_failed_checks(failed: &[(String, Error)]) -> Result<()> {
    for (id, e) in failed {
        eprintln!("{id}: check failed: {}", clean_lines(&e.to_string()));
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
        || err.is::<UpdatesRefused>()
        || matches!(
            err.downcast_ref::<Error>(),
            Some(Error::Refused(_) | Error::NeedsAcceptance(_) | Error::NeedsConsent(_))
        );
    if refused { EXIT_REFUSED } else { EXIT_ERROR }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let result = match cli.command {
        Command::Completion { shell } => {
            print_completion(shell);
            Ok(())
        }
        Command::SelfCmd { action } => selfmgmt::run(action),
        command => run(command),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if err.is::<NeedsConfirmation>() {
                eprintln!("{}", clean_lines(&err.to_string()));
            } else {
                eprintln!("error: {}", clean_lines(&format!("{err:#}")));
                if matches!(err.downcast_ref::<Error>(), Some(Error::NeedsAcceptance(_))) {
                    eprintln!("pass --allow-risky to accept them");
                }
                if matches!(err.downcast_ref::<Error>(), Some(Error::NeedsConsent(_))) {
                    eprintln!("pass --yes to confirm");
                }
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
    fn refused_updates_exit_with_the_refusal_code() {
        assert!(update_failure(0, false).is_none());
        let plain = update_failure(2, false).unwrap();
        assert_eq!(exit_code(&plain), EXIT_ERROR);
        let refused = update_failure(2, true).unwrap();
        assert_eq!(exit_code(&refused), EXIT_REFUSED);
        assert_eq!(refused.to_string(), "2 update(s) failed");
    }

    #[test]
    fn needs_consent_exits_with_the_refusal_code() {
        let err: anyhow::Error = Error::NeedsConsent("x".into()).into();
        assert_eq!(exit_code(&err), EXIT_REFUSED);
    }

    #[test]
    fn format_flag_accepts_package_formats() {
        assert_eq!(parse_format("deb"), Ok(FormatKind::Deb));
        assert!(parse_format("zip").is_err());
    }

    #[test]
    fn other_update_failures_are_counted_and_the_run_continues() {
        let mut failed = 0;
        note_update_failure("a", Error::NotFound("x".into()), &mut failed).unwrap();
        assert_eq!(failed, 1);
    }
}
