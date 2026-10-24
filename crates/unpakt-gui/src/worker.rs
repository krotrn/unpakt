//! The engine runs on one worker thread; the UI sends commands and renders events.

use relm4::{ComponentSender, Worker};
use unpakt_core::Error;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine, InstallPlan, UpdateInfo};
use unpakt_core::manifest::AppRecord;
use unpakt_core::progress::Progress;

/// The kind of command that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Inspect,
    Install,
    Check,
    Update,
    Rollback,
    Remove,
    List,
}

#[derive(Debug)]
pub enum Cmd {
    Inspect {
        input: String,
        cancel: Cancel,
    },
    Install {
        plan: Box<InstallPlan>,
        cancel: Cancel,
    },
    Discard(Box<InstallPlan>),
    List,
    CheckUpdates,
    Update {
        id: String,
        cancel: Cancel,
    },
    Rollback {
        id: String,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug)]
pub enum Event {
    Progress(Progress),
    Inspected(Box<InstallPlan>),
    Installed(AppRecord),
    Apps(Vec<AppRecord>),
    Updates {
        available: Vec<UpdateInfo>,
        failed: usize,
    },
    Updated {
        id: String,
        label: Option<String>,
    },
    RolledBack {
        id: String,
        label: String,
    },
    Removed {
        id: String,
    },
    Failed {
        op: Op,
        error: Error,
    },
}

impl Cmd {
    fn op(&self) -> Op {
        match self {
            Cmd::Inspect { .. } | Cmd::Discard(_) => Op::Inspect,
            Cmd::Install { .. } => Op::Install,
            Cmd::List => Op::List,
            Cmd::CheckUpdates => Op::Check,
            Cmd::Update { .. } => Op::Update,
            Cmd::Rollback { .. } => Op::Rollback,
            Cmd::Remove { .. } => Op::Remove,
        }
    }
}

/// Runs one command to completion on the calling thread. Every successful
/// mutating command is followed by `Event::Apps` with the fresh list.
pub fn handle(engine: &mut Engine, cmd: Cmd, emit: &dyn Fn(Event)) {
    let op = cmd.op();
    let result: Result<(), Error> = match cmd {
        Cmd::Inspect { input, cancel } => {
            engine.set_cancel(cancel);
            engine
                .inspect(&input, AddOpts::default())
                .map(|plan| emit(Event::Inspected(Box::new(plan))))
        }
        Cmd::Install { plan, cancel } => {
            engine.set_cancel(cancel);
            engine.install(*plan).and_then(|app| {
                emit(Event::Installed(app));
                emit_apps(engine, emit)
            })
        }
        Cmd::Discard(plan) => {
            engine.discard(*plan);
            Ok(())
        }
        Cmd::List => emit_apps(engine, emit),
        Cmd::CheckUpdates => engine.check_updates(None).map(|check| {
            emit(Event::Updates {
                available: check.updates,
                failed: check.failed.len(),
            })
        }),
        Cmd::Update { id, cancel } => {
            engine.set_cancel(cancel);
            engine.update(&id).and_then(|label| {
                emit(Event::Updated { id, label });
                emit_apps(engine, emit)
            })
        }
        Cmd::Rollback { id } => {
            engine.set_cancel(Cancel::new());
            engine.rollback(&id).and_then(|label| {
                emit(Event::RolledBack { id, label });
                emit_apps(engine, emit)
            })
        }
        Cmd::Remove { id } => {
            engine.set_cancel(Cancel::new());
            engine.remove(&id).and_then(|()| {
                emit(Event::Removed { id });
                emit_apps(engine, emit)
            })
        }
    };
    if let Err(error) = result {
        emit(Event::Failed { op, error });
    }
}

fn emit_apps(engine: &Engine, emit: &dyn Fn(Event)) -> Result<(), Error> {
    emit(Event::Apps(engine.list()?));
    Ok(())
}

/// Owns the engine on the worker thread. If the engine could not be opened,
/// the reason is kept and every command fails with it.
pub struct EngineWorker {
    engine: Result<Engine, String>,
}

impl Worker for EngineWorker {
    type Init = ();
    type Input = Cmd;
    type Output = Event;

    fn init((): (), sender: ComponentSender<Self>) -> Self {
        let out = sender.output_sender().clone();
        let engine = Engine::open_default()
            .map(|e| {
                e.with_progress(std::sync::Arc::new(move |p| {
                    let _ = out.send(Event::Progress(p));
                }))
            })
            .map_err(|e| e.to_string());
        EngineWorker { engine }
    }

    fn update(&mut self, cmd: Cmd, sender: ComponentSender<Self>) {
        let emit = |event| {
            let _ = sender.output(event);
        };
        match &mut self.engine {
            Ok(engine) => handle(engine, cmd, &emit),
            Err(reason) => emit(Event::Failed {
                op: cmd.op(),
                error: Error::Unsupported(reason.clone()),
            }),
        }
    }
}
