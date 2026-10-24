use std::cell::RefCell;
use std::fs::File;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use unpakt_core::Error;
use unpakt_core::Layout;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::host::Host;
use unpakt_core::progress::Progress;
use unpakt_gui::worker::{Cmd, Event, Op, handle};

fn fixture_tarball(dir: &Path) -> PathBuf {
    let path = dir.join("unpakt-fixture-tool-1.0.tar.gz");
    let enc = GzEncoder::new(File::create(&path).unwrap(), Compression::fast());
    let mut builder = tar::Builder::new(enc);
    let body = b"#!/bin/sh\necho hi\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_entry_type(tar::EntryType::Regular);
    builder
        .append_data(
            &mut header,
            "unpakt-fixture-tool/bin/unpakt-fixture-tool",
            &body[..],
        )
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    path
}

fn engine(root: &Path) -> Engine {
    Engine::new(
        Layout::under(root),
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
    .unwrap()
}

/// Runs one command and returns every event it emitted, in order.
fn run(engine: &mut Engine, cmd: Cmd) -> Vec<Event> {
    let events = RefCell::new(Vec::new());
    handle(engine, cmd, &|e| events.borrow_mut().push(e));
    events.into_inner()
}

/// Wires progress the way the worker does so tests see `Event::Progress`.
fn run_with_progress(root: &Path, cmd: impl FnOnce(&mut Engine) -> Cmd) -> Vec<Event> {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut eng = engine(root).with_progress(std::sync::Arc::new(move |p| {
        sink.lock().unwrap().push(Event::Progress(p));
    }));
    let cmd = cmd(&mut eng);
    let sink = events.clone();
    handle(&mut eng, cmd, &|e| sink.lock().unwrap().push(e));
    std::mem::take(&mut *events.lock().unwrap())
}

fn inspect(eng: &mut Engine, input: &Path) -> Box<unpakt_core::engine::InstallPlan> {
    let events = run(
        eng,
        Cmd::Inspect {
            input: input.to_str().unwrap().into(),
            opts: AddOpts::default(),
            cancel: Cancel::new(),
        },
    );
    events
        .into_iter()
        .find_map(|e| match e {
            Event::Inspected(p) => Some(p),
            _ => None,
        })
        .expect("inspected")
}

#[test]
fn inspect_then_install_emits_progress_plan_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let input = tarball.to_str().unwrap().to_string();

    let events = run_with_progress(tmp.path(), |_| Cmd::Inspect {
        input,
        opts: AddOpts::default(),
        cancel: Cancel::new(),
    });
    assert!(
        matches!(events[0], Event::Progress(Progress::Inspecting)),
        "{events:?}"
    );
    let plan = match events.last() {
        Some(Event::Inspected(p)) => p.clone(),
        other => panic!("{other:?}"),
    };

    let events = run_with_progress(tmp.path(), |_| Cmd::Install {
        plan,
        cancel: Cancel::new(),
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Progress(Progress::Installing))),
        "{events:?}"
    );
    let installed = events.iter().position(|e| matches!(e, Event::Installed(_)));
    let apps = events
        .iter()
        .position(|e| matches!(e, Event::Apps(a) if a.len() == 1));
    assert!(installed.unwrap() < apps.unwrap(), "{events:?}");
}

#[test]
fn install_after_external_install_fails_with_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let mut eng = engine(tmp.path());
    let plan = inspect(&mut eng, &tarball);

    let mut other = engine(tmp.path());
    let external = other
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    other.install(external).unwrap();

    let events = run(
        &mut eng,
        Cmd::Install {
            plan,
            cancel: Cancel::new(),
        },
    );
    assert!(
        matches!(
            &events[..],
            [Event::Failed {
                op: Op::Install,
                error: Error::Conflict(_)
            }]
        ),
        "{events:?}"
    );
}

#[test]
fn pre_cancelled_install_is_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let mut eng = engine(tmp.path());
    let plan = inspect(&mut eng, &tarball);

    let cancel = Cancel::new();
    cancel.cancel();
    let events = run(&mut eng, Cmd::Install { plan, cancel });
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Failed {
                op: Op::Install,
                error: Error::Interrupted
            }
        )),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Installed(_))));

    let events = run(&mut eng, Cmd::List);
    assert!(
        matches!(&events[..], [Event::Apps(a)] if a.is_empty()),
        "{events:?}"
    );
}

#[test]
fn rollback_without_older_version_fails_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let mut eng = engine(tmp.path());
    let plan = inspect(&mut eng, &tarball);
    let events = run(
        &mut eng,
        Cmd::Install {
            plan,
            cancel: Cancel::new(),
        },
    );
    let id = events
        .iter()
        .find_map(|e| match e {
            Event::Installed(a) => Some(a.id.clone()),
            _ => None,
        })
        .unwrap();

    let events = run(&mut eng, Cmd::Rollback { id });
    assert!(
        matches!(
            &events[..],
            [Event::Failed {
                op: Op::Rollback,
                error: Error::NotFound(_)
            }]
        ),
        "{events:?}"
    );
}

#[test]
fn remove_emits_removed_and_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let mut eng = engine(tmp.path());
    let plan = inspect(&mut eng, &tarball);
    let events = run(
        &mut eng,
        Cmd::Install {
            plan,
            cancel: Cancel::new(),
        },
    );
    let id = events
        .iter()
        .find_map(|e| match e {
            Event::Installed(a) => Some(a.id.clone()),
            _ => None,
        })
        .unwrap();

    let events = run(&mut eng, Cmd::Remove { id: id.clone() });
    match &events[..] {
        [Event::Removed { id: got }, Event::Apps(apps)] => {
            assert_eq!(*got, id);
            assert!(apps.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn discard_installs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let mut eng = engine(tmp.path());
    let plan = inspect(&mut eng, &tarball);

    let events = run(&mut eng, Cmd::Discard(plan));
    assert!(events.is_empty(), "{events:?}");
    let events = run(&mut eng, Cmd::List);
    assert!(
        matches!(&events[..], [Event::Apps(a)] if a.is_empty()),
        "{events:?}"
    );
}
