mod output;

use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use unpakt_core::Error;
use unpakt_core::engine::{AddOpts, Engine};

const EXIT_ERROR: u8 = 1;
const EXIT_REFUSED: u8 = 2;

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
    /// Install a package file
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

fn run(command: Command) -> Result<()> {
    let mut engine = Engine::open_default()?;
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
                pattern: None,
            };
            let plan = engine.inspect(&input, opts)?;
            output::print_summary(&plan);
            if !yes {
                confirm("install", "Install?")?;
            }
            let app = engine.install(plan)?;
            println!("installed {} {}", app.id, app.current);
        }
        Command::Inspect { input } => {
            output::print_summary(&engine.inspect(&input, AddOpts::default())?);
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
            engine.remove(&id)?;
            println!("removed {id}");
        }
        Command::Doctor => output::print_checks(&engine.doctor()),
    }
    Ok(())
}

fn exit_code(err: &anyhow::Error) -> u8 {
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
