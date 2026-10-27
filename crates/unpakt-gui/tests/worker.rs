use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use unpakt_core::Error;
use unpakt_core::Layout;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::packagekit::{PackageKit, PkEvent, PkPackage};
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

/// A PackageKit that installs whatever `next_install` says.
#[derive(Clone, Default)]
struct FakePk(Arc<Mutex<FakePkState>>);

#[derive(Default)]
struct FakePkState {
    installed: BTreeMap<String, PkPackage>,
    next_install: Option<PkPackage>,
}

impl PackageKit for FakePk {
    fn can_install_files(&self) -> bool {
        true
    }

    fn resolve_installed(&self, name: &str) -> unpakt_core::Result<Option<PkPackage>> {
        Ok(self.0.lock().unwrap().installed.get(name).cloned())
    }

    fn install_file(
        &self,
        _: &Path,
        _: &Cancel,
        _: &mut dyn FnMut(PkEvent),
    ) -> unpakt_core::Result<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(pkg) = state.next_install.take() {
            state.installed.insert(pkg.name.clone(), pkg);
        }
        Ok(())
    }

    fn remove(&self, id: &str, _: &Cancel, _: &mut dyn FnMut(PkEvent)) -> unpakt_core::Result<()> {
        self.0.lock().unwrap().installed.retain(|_, p| p.id != id);
        Ok(())
    }
}

fn gz(bytes: Vec<u8>) -> Vec<u8> {
    let mut w = GzEncoder::new(Vec::new(), Compression::fast());
    w.write_all(&bytes).unwrap();
    w.finish().unwrap()
}

fn tar_of(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

fn host_deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => "all",
    }
}

/// A deb of package `hello` at `version`.
fn hello_deb(version: &str) -> Vec<u8> {
    let control = format!(
        "Package: hello\nVersion: {version}\nArchitecture: {}\nMaintainer: t <t@example.org>\n",
        host_deb_arch()
    );
    let members = [
        ("debian-binary", b"2.0\n".to_vec()),
        (
            "control.tar.gz",
            gz(tar_of(&[("control", control.as_bytes(), 0o644)])),
        ),
        (
            "data.tar.gz",
            gz(tar_of(&[("opt/hello/hello", b"#!/bin/sh\n", 0o755)])),
        ),
    ];
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        out.extend(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                "100644",
                data.len()
            )
            .bytes(),
        );
        out.extend(&data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// Serves `/repos/o/r/releases/latest` with one asset, `hello_<tag>.deb`.
struct Forge {
    port: u16,
    release: Arc<Mutex<String>>,
}

impl Forge {
    fn start(tag: &str) -> Forge {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let release = Arc::new(Mutex::new(tag.to_string()));
        let tag = Arc::clone(&release);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = serve(stream, port, &tag.lock().unwrap());
            }
        });
        Forge { port, release }
    }

    fn publish(&self, tag: &str) {
        *self.release.lock().unwrap() = tag.to_string();
    }

    fn http(&self) -> Http {
        Http::new(&format!("http://127.0.0.1:{}", self.port), None).unwrap()
    }
}

fn serve(mut stream: std::net::TcpStream, port: u16, tag: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let deb = hello_deb(tag);
    let (status, body, len) = if path == "/repos/o/r/releases/latest" {
        let body = format!(
            r#"{{"tag_name":"{tag}","assets":[{{"name":"hello_{tag}.deb","browser_download_url":"http://127.0.0.1:{port}/dl/hello_{tag}.deb","size":{}}}]}}"#,
            deb.len()
        )
        .into_bytes();
        let len = body.len();
        ("200 OK", body, len)
    } else if path.starts_with("/dl/") {
        let len = deb.len();
        ("200 OK", deb, len)
    } else {
        ("404 Not Found", Vec::new(), 0)
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    )?;
    if method != "HEAD" {
        stream.write_all(&body)?;
    }
    stream.flush()
}

/// A native `hello` 1.0 installed from the forge's release.
fn native_hello(root: &Path, forge: &Forge) -> Engine {
    let fake = FakePk::default();
    fake.0.lock().unwrap().next_install = PkPackage::parse("hello;1.0;amd64;local");
    let host = Host {
        os_id: Some("debian".into()),
        os_like: vec![],
        immutable_root: false,
        fuse2: true,
        ..Host::detect()
    };
    let mut eng = Engine::new(Layout::under(root), host)
        .unwrap()
        .with_http(forge.http())
        .with_packagekit(Box::new(fake.clone()));
    let mut plan = eng
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    plan.native_consent = true;
    eng.install(plan).unwrap();
    fake.0.lock().unwrap().next_install = PkPackage::parse("hello;2.0;amd64;local");
    eng
}

/// Runs an update, answering every consent question with `answer`.
fn update_answering(eng: &mut Engine, answer: bool) -> Vec<Event> {
    let events = RefCell::new(Vec::new());
    handle(
        eng,
        Cmd::Update {
            id: "hello".into(),
            cancel: Cancel::new(),
            accept_risks: false,
        },
        &|e| {
            if let Event::ConfirmNative { reply, .. } = &e {
                reply.send(answer).unwrap();
            }
            events.borrow_mut().push(e);
        },
    );
    events.into_inner()
}

#[test]
fn native_update_asks_through_the_ui() {
    let tmp = tempfile::tempdir().unwrap();
    let forge = Forge::start("1.0");
    let mut eng = native_hello(tmp.path(), &forge);
    forge.publish("2.0");

    let events = update_answering(&mut eng, true);
    let asked = events
        .iter()
        .position(|e| matches!(e, Event::ConfirmNative { id, .. } if id == "hello"))
        .unwrap_or_else(|| panic!("{events:?}"));
    let updated = events
        .iter()
        .position(|e| matches!(e, Event::Updated { label: Some(_), .. }))
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(asked < updated, "{events:?}");
}

#[test]
fn declined_native_update_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let forge = Forge::start("1.0");
    let mut eng = native_hello(tmp.path(), &forge);
    forge.publish("2.0");

    let events = update_answering(&mut eng, false);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Failed {
                op: Op::Update,
                error: Error::Refused(_)
            }
        )),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Updated { .. })));
}
