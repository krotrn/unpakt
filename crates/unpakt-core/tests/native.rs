mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::fake_pk::FakePk;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine, Route};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::manifest::Manifest;
use unpakt_core::packagekit::{PackageKit, PkEvent, PkPackage};
use unpakt_core::progress::{Progress, ProgressFn};
use unpakt_core::{Error, Layout};

fn fake() -> FakePk {
    let pk = FakePk::default();
    {
        let mut state = pk.0.lock().unwrap();
        state.can_install = true;
        state.events = vec![
            PkEvent::WaitingForAuth,
            PkEvent::Changing { percent: Some(10) },
            PkEvent::Changing { percent: Some(90) },
        ];
        state.next_install = PkPackage::parse("foo;1.0-1;amd64;local");
    }
    pk
}

#[test]
fn fake_interrupts_when_cancelled_before_changes_start() {
    let pk = fake();
    let cancel = Cancel::new();
    cancel.cancel();
    let err = pk
        .install_file(Path::new("/tmp/foo.deb"), &cancel, &mut |_| {})
        .unwrap_err();
    assert!(matches!(err, Error::Interrupted));
    assert!(pk.0.lock().unwrap().installed.is_empty());
}

#[test]
fn fake_ignores_cancel_once_changes_start() {
    let pk = fake();
    let cancel = Cancel::new();
    pk.install_file(Path::new("/tmp/foo.deb"), &cancel, &mut |e| {
        if matches!(e, PkEvent::Changing { .. }) {
            cancel.cancel();
        }
    })
    .unwrap();
    assert!(pk.0.lock().unwrap().installed.contains_key("foo"));
}

fn deb_host() -> Host {
    Host {
        os_id: Some("debian".into()),
        os_like: vec![],
        immutable_root: false,
        fuse2: true,
        ..Host::detect()
    }
}

#[test]
fn default_engine_has_no_packagekit() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let (_, deb) = common::deb_with_elf(src.path(), "1.0.0", &[], &[], false);
    let engine = Engine::new(Layout::under(root.path()), deb_host()).unwrap();
    let plan = engine
        .inspect(deb.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert_eq!(plan.backend, "extract");
}

fn hello_deb(dir: &Path, postinst: Option<&str>) -> std::path::PathBuf {
    let control = format!(
        "Package: hello\nVersion: 1.0\nArchitecture: {}\nMaintainer: t <t@example.org>\n",
        common::host_package_arch()
    );
    let scripts: Vec<(&str, &[u8])> = postinst
        .map(|text| ("postinst", text.as_bytes()))
        .into_iter()
        .collect();
    common::build_deb(
        dir,
        "hello_1.0.deb",
        &control,
        &scripts,
        &[
            common::E::File("opt/hello/hello", b"#!/bin/sh\n", 0o755),
            common::E::Link("usr/bin/hello", "/opt/hello/hello"),
        ],
    )
}

fn native_engine(root: &Path, fake: &FakePk) -> Engine {
    Engine::new(Layout::under(root), deb_host())
        .unwrap()
        .with_packagekit(Box::new(fake.clone()))
}

fn inspect_hello(
    postinst: Option<&str>,
    host: Host,
    fake: &FakePk,
    opts: AddOpts,
) -> unpakt_core::engine::InstallPlan {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let deb = hello_deb(src.path(), postinst);
    let engine = Engine::new(Layout::under(root.path()), host)
        .unwrap()
        .with_packagekit(Box::new(fake.clone()));
    engine.inspect(deb.to_str().unwrap(), opts).unwrap()
}

const POSTINST: &str = "#!/bin/sh\nupdate-desktop-database\n";

#[test]
fn same_family_routes_native() {
    let plan = inspect_hello(Some(POSTINST), deb_host(), &fake(), AddOpts::default());
    assert_eq!(plan.route(), Some(Route::Native));
    assert_eq!(plan.backend, "packagekit");
    assert!(plan.native_hint.is_none());
    assert!(!plan.native_consent);
}

#[test]
fn extract_flag_keeps_user_scope() {
    let opts = AddOpts {
        extract: true,
        ..Default::default()
    };
    let plan = inspect_hello(Some(POSTINST), deb_host(), &fake(), opts);
    assert_eq!(plan.route(), Some(Route::Extract));
    assert!(plan.native_skipped.is_none());
}

#[test]
fn no_packagekit_extracts_and_says_why() {
    let pk = fake();
    pk.0.lock().unwrap().can_install = false;
    let plan = inspect_hello(Some(POSTINST), deb_host(), &pk, AddOpts::default());
    assert_eq!(plan.route(), Some(Route::Extract));
    assert_eq!(
        plan.native_skipped.as_deref(),
        Some("PackageKit is not available")
    );
    assert!(plan.native_hint.is_some());
}

#[test]
fn image_based_host_extracts() {
    let host = Host {
        immutable_root: true,
        ..deb_host()
    };
    let plan = inspect_hello(Some(POSTINST), host, &fake(), AddOpts::default());
    assert_eq!(plan.route(), Some(Route::Extract));
    assert_eq!(
        plan.native_skipped.as_deref(),
        Some("the system is image-based")
    );
}

#[test]
fn cross_family_never_probes() {
    let pk = fake();
    pk.0.lock().unwrap().panic_on_probe = true;
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let rpm = common::build_rpm(
        src.path(),
        "bar-1.0-1.rpm",
        common::RpmTags {
            name: "bar",
            version: "1.0",
            release: "1",
            arch: common::host_rpm_arch(),
            post: None,
            requires: &[],
            source: false,
        },
        &[common::E::File("opt/bar/bar", b"#!/bin/sh\n", 0o755)],
    );
    let plan = native_engine(root.path(), &pk)
        .inspect(rpm.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert!(matches!(
        plan.route(),
        Some(Route::Extract | Route::Refused)
    ));
}

#[test]
fn native_ignores_verdict() {
    let plan = inspect_hello(
        Some("#!/bin/sh\nuseradd foo\n"),
        deb_host(),
        &fake(),
        AddOpts::default(),
    );
    assert_eq!(plan.route(), Some(Route::Native));
    assert!(plan.blocked().is_none());
}

#[test]
fn consent_message_names_scripts() {
    let plan = inspect_hello(Some(POSTINST), deb_host(), &fake(), AddOpts::default());
    let message = plan.consent().unwrap().message();
    assert!(message.starts_with("This installs hello 1.0 system-wide"));
    assert!(message.contains("Its install scripts will run as root."));
    assert!(message.contains("Scripts: postinst"));
    let plain = inspect_hello(None, deb_host(), &fake(), AddOpts::default());
    assert!(
        plain
            .consent()
            .unwrap()
            .message()
            .contains("It has no install scripts.")
    );
}

#[test]
fn tarball_has_no_route() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = common::build_tarball(
        src.path(),
        "tool-1.0.tar.gz",
        common::Comp::Gz,
        &[common::E::File("tool/tool", b"#!/bin/sh\n", 0o755)],
    );
    let plan = native_engine(root.path(), &fake())
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert_eq!(plan.route(), None);
    assert!(plan.consent().is_none());
}

fn hello_fake() -> FakePk {
    let pk = FakePk::default();
    {
        let mut state = pk.0.lock().unwrap();
        state.can_install = true;
        state.next_install = PkPackage::parse("hello;1.0;amd64;local");
    }
    pk
}

fn native_plan(engine: &Engine, deb: &Path, consent: bool) -> unpakt_core::engine::InstallPlan {
    let mut plan = engine
        .inspect(deb.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert_eq!(plan.route(), Some(Route::Native));
    plan.native_consent = consent;
    plan
}

fn journals(layout: &Layout) -> usize {
    std::fs::read_dir(layout.data.join("native"))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .count()
        })
        .unwrap_or(0)
}

fn installs(pk: &FakePk) -> usize {
    let state = pk.0.lock().unwrap();
    state
        .calls
        .iter()
        .filter(|c| c.starts_with("install "))
        .count()
}

#[test]
fn native_install_needs_consent() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let deb = hello_deb(src.path(), Some(POSTINST));
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, false);
    match engine.install(plan) {
        Err(Error::NeedsConsent(m)) => assert!(m.contains("will run as root"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(installs(&pk), 0);
    assert!(engine.list().unwrap().is_empty());
}

#[test]
fn native_install_records_app() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let deb = hello_deb(src.path(), Some(POSTINST));
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    let app = engine.install(plan).unwrap();
    assert_eq!(app.backend, "packagekit");
    assert!(!app.desktop);
    assert!(app.command.is_none());
    let (_, versions, files) = engine.get("hello").unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].entry, Path::new("/usr/bin/hello"));
    assert!(files.is_empty());
    let layout = engine.layout();
    assert!(!layout.apps.join("hello").exists());
    let desktops = std::fs::read_dir(&layout.applications)
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(desktops, 0);
    assert_eq!(journals(layout), 0);
}

#[test]
fn native_install_records_package_options() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    engine.install(plan).unwrap();
    let manifest = Manifest::open(&Layout::under(root.path()).db).unwrap();
    let get = |k: &str| manifest.option("hello", k).unwrap();
    assert_eq!(
        get("pk.package_id").as_deref(),
        Some("hello;1.0;amd64;local")
    );
    assert_eq!(get("pk.name").as_deref(), Some("hello"));
}

#[test]
fn native_install_gets_an_existing_absolute_file() {
    let forge = common::MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let deb = hello_deb(src.path(), None);
    forge.publish(
        "1.0",
        vec![("hello_1.0.deb".into(), std::fs::read(&deb).unwrap())],
    );
    let mut engine =
        native_engine(root.path(), &pk).with_http(Http::new(&forge.api_url(), None).unwrap());
    let mut plan = engine
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    plan.native_consent = true;
    engine.install(plan).unwrap();
    let state = pk.0.lock().unwrap();
    let call = state
        .calls
        .iter()
        .find(|c| c.starts_with("install "))
        .unwrap();
    assert!(Path::new(&call["install ".len()..]).is_absolute(), "{call}");
    assert_eq!(state.file_existed, [true]);
}

fn recorder() -> (ProgressFn, Arc<Mutex<Vec<Progress>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let f: ProgressFn = Arc::new(move |p| sink.lock().unwrap().push(p));
    (f, events)
}

#[test]
fn native_progress_events() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().events = vec![
        PkEvent::WaitingForAuth,
        PkEvent::Changing { percent: Some(50) },
    ];
    let deb = hello_deb(src.path(), None);
    let (recorder, events) = recorder();
    let mut engine = native_engine(root.path(), &pk).with_progress(recorder);
    let plan = native_plan(&engine, &deb, true);
    engine.install(plan).unwrap();
    let events = events.lock().unwrap();
    let tail: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Progress::WaitingForAuthorization | Progress::SystemChange { .. }
            )
        })
        .collect();
    assert_eq!(
        tail,
        [
            &Progress::WaitingForAuthorization,
            &Progress::SystemChange { percent: Some(50) }
        ]
    );
}

#[test]
fn denied_authorization_records_nothing() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().fail_install =
        Some(|| Error::Refused("authorization was refused; nothing was changed".into()));
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    assert!(matches!(engine.install(plan), Err(Error::Refused(_))));
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn failed_install_keeps_journal_for_recovery() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().fail_install = Some(|| {
        Error::PackageKit("PackageKit did not answer; the change may still be applied".into())
    });
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    assert!(matches!(engine.install(plan), Err(Error::PackageKit(_))));
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(journals(engine.layout()), 1);
}

#[test]
fn cancel_while_waiting_installs_nothing() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().events =
        vec![PkEvent::WaitingForAuth, PkEvent::Changing { percent: None }];
    let deb = hello_deb(src.path(), None);
    let cancel = Cancel::new();
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    engine.set_cancel(cancel.clone());
    cancel.cancel();
    assert!(matches!(engine.install(plan), Err(Error::Interrupted)));
    assert!(pk.0.lock().unwrap().installed.is_empty());
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn cancel_after_changes_start_still_records() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().events =
        vec![PkEvent::WaitingForAuth, PkEvent::Changing { percent: None }];
    let deb = hello_deb(src.path(), None);
    let cancel = Cancel::new();
    let flag = cancel.clone();
    let f: ProgressFn = Arc::new(move |p| {
        if matches!(p, Progress::SystemChange { .. }) {
            flag.cancel();
        }
    });
    let mut engine = native_engine(root.path(), &pk)
        .with_cancel(cancel)
        .with_progress(f);
    let plan = native_plan(&engine, &deb, true);
    engine.install(plan).unwrap();
    assert_eq!(engine.list().unwrap().len(), 1);
}

#[test]
fn already_installed_is_a_conflict() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    engine.install(plan).unwrap();
    let again = native_plan(&engine, &deb, true);
    assert!(matches!(engine.install(again), Err(Error::Conflict(_))));
    assert_eq!(installs(&pk), 1);
}

#[test]
fn refused_consent_keeps_the_download() {
    let forge = common::MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let deb = hello_deb(src.path(), None);
    forge.publish(
        "1.0",
        vec![("hello_1.0.deb".into(), std::fs::read(&deb).unwrap())],
    );
    let mut engine =
        native_engine(root.path(), &pk).with_http(Http::new(&forge.api_url(), None).unwrap());
    let mut plan = engine
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    let artifact = plan.plan.artifact.path.clone();
    assert!(artifact.starts_with(engine.layout().cache.join("downloads")));
    assert!(matches!(
        engine.install(plan.clone()),
        Err(Error::NeedsConsent(_))
    ));
    assert!(artifact.exists());
    plan.native_consent = true;
    engine.install(plan).unwrap();
}
