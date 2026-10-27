mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::fake_pk::FakePk;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine, Route, UpdateOpts};
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
    hello_deb_at(dir, "1.0", postinst)
}

fn hello_deb_at(dir: &Path, version: &str, postinst: Option<&str>) -> std::path::PathBuf {
    let control = format!(
        "Package: hello\nVersion: {version}\nArchitecture: {}\nMaintainer: t <t@example.org>\n",
        common::host_package_arch()
    );
    let scripts: Vec<(&str, &[u8])> = postinst
        .map(|text| ("postinst", text.as_bytes()))
        .into_iter()
        .collect();
    common::build_deb(
        dir,
        &format!("hello_{version}.deb"),
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
fn pacman_package_is_extracted_without_probing() {
    let pk = fake();
    pk.0.lock().unwrap().panic_on_probe = true;
    let host = Host {
        os_id: Some("arch".into()),
        os_like: vec![],
        immutable_root: false,
        fuse2: true,
        ..Host::detect()
    };
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pkg = common::build_pacman(
        src.path(),
        "hello-1.0-1-any.pkg.tar.zst",
        "pkgname = hello\npkgver = 1.0-1\narch = any\n",
        None,
        &[common::E::File("opt/hello/hello", b"#!/bin/sh\n", 0o755)],
        common::Comp::Zst,
    );
    let engine = Engine::new(Layout::under(root.path()), host)
        .unwrap()
        .with_packagekit(Box::new(pk));
    let plan = engine
        .inspect(pkg.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert_ne!(plan.route(), Some(Route::Native));
    assert_eq!(
        plan.native_skipped.as_deref(),
        Some("PackageKit cannot install local pacman packages")
    );
    assert!(plan.native_hint.unwrap().contains("sudo pacman -U"));
    let opts = AddOpts {
        extract: true,
        ..Default::default()
    };
    let plan = engine.inspect(pkg.to_str().unwrap(), opts).unwrap();
    assert!(plan.native_skipped.is_none());
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
fn lost_journal_after_success_is_not_an_error() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().events = vec![PkEvent::Changing { percent: Some(50) }];
    let deb = hello_deb(src.path(), None);
    let native = Layout::under(root.path()).data.join("native");
    let f: ProgressFn = Arc::new(move |p| {
        if matches!(p, Progress::SystemChange { .. }) {
            for entry in std::fs::read_dir(&native).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
        }
    });
    let mut engine = native_engine(root.path(), &pk).with_progress(f);
    let plan = native_plan(&engine, &deb, true);
    engine.install(plan).unwrap();
    assert_eq!(engine.list().unwrap().len(), 1);
    engine.remove("hello").unwrap();
    assert!(engine.list().unwrap().is_empty());
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

#[test]
fn resolve_failure_after_install_is_settled_next_run() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().fail_resolve_once = true;
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    assert!(matches!(engine.install(plan), Err(Error::PackageKit(_))));
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(journals(engine.layout()), 1);

    let recovered = engine.recover().unwrap();
    assert_eq!(recovered.native, 1);
    assert_eq!(engine.get("hello").unwrap().0.backend, "packagekit");
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn pending_native_journal_does_not_block_other_installs() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    pk.0.lock().unwrap().fail_resolve_once = true;
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root.path(), &pk);
    let plan = native_plan(&engine, &deb, true);
    assert!(engine.install(plan).is_err());

    let mut engine = Engine::new(Layout::under(root.path()), deb_host()).unwrap();
    let tarball = common::build_tarball(
        src.path(),
        "tool-1.0.tar.gz",
        common::Comp::Gz,
        &[common::E::File("tool/tool", b"#!/bin/sh\n", 0o755)],
    );
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    engine.install(plan).unwrap();
    assert_eq!(journals(engine.layout()), 1);
    let checks = engine.doctor();
    let check = checks.iter().find(|c| c.name == "journal").unwrap();
    assert!(!check.ok);
    assert!(
        check.detail.contains("system package operations"),
        "{}",
        check.detail
    );
}

fn engine_with_hello(root: &Path, pk: &FakePk) -> Engine {
    let src = tempfile::tempdir().unwrap();
    let deb = hello_deb(src.path(), None);
    let mut engine = native_engine(root, pk);
    let plan = native_plan(&engine, &deb, true);
    engine.install(plan).unwrap();
    engine
}

fn removes(pk: &FakePk) -> Vec<String> {
    let state = pk.0.lock().unwrap();
    state
        .calls
        .iter()
        .filter(|c| c.starts_with("remove "))
        .cloned()
        .collect()
}

#[test]
fn remove_native_calls_packagekit() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    engine.remove("hello").unwrap();
    assert_eq!(removes(&pk), ["remove hello;1.0;amd64;local"]);
    assert!(matches!(engine.get("hello"), Err(Error::NotFound(_))));
    assert!(pk.0.lock().unwrap().installed.is_empty());
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn remove_native_already_gone_forgets_it() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    pk.0.lock().unwrap().installed.clear();
    engine.remove("hello").unwrap();
    assert!(removes(&pk).is_empty());
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn remove_native_failure_keeps_record() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    pk.0.lock().unwrap().fail_remove =
        Some(|| Error::PackageKit("hello is needed by world".into()));
    match engine.remove("hello") {
        Err(Error::PackageKit(m)) => assert!(m.contains("needed by"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(engine.get("hello").is_ok());
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn remove_native_without_packagekit_points_to_forget() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let engine = engine_with_hello(root.path(), &pk);
    let mut engine = engine.with_packagekit(Box::new(unpakt_core::packagekit::Unavailable));
    match engine.remove("hello") {
        Err(Error::PackageKitUnavailable(m)) => {
            assert!(m.contains("unpakt remove --forget hello"), "{m}")
        }
        other => panic!("{other:?}"),
    }
    assert!(engine.get("hello").is_ok());
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn forget_drops_only_the_record() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    engine.forget("hello").unwrap();
    assert!(matches!(engine.get("hello"), Err(Error::NotFound(_))));
    assert!(removes(&pk).is_empty());
    assert!(pk.0.lock().unwrap().installed.contains_key("hello"));
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn forget_refuses_extract_apps() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = common::build_tarball(
        src.path(),
        "tool-1.0.tar.gz",
        common::Comp::Gz,
        &[common::E::File("tool/tool", b"#!/bin/sh\n", 0o755)],
    );
    let mut engine = native_engine(root.path(), &fake());
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    engine.install(plan).unwrap();
    match engine.forget("tool") {
        Err(Error::Unsupported(m)) => assert!(m.contains("use `unpakt remove"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(engine.list().unwrap().len(), 1);
    assert!(matches!(engine.forget("nope"), Err(Error::NotFound(_))));
}

#[test]
fn rollback_native_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    match engine.rollback("hello") {
        Err(Error::Unsupported(m)) => assert!(m.contains("can't be rolled back"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn doctor_reports_missing_native_entry() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let engine = engine_with_hello(root.path(), &pk);
    let checks = engine.doctor();
    let check = checks.iter().find(|c| c.name == "app:hello").unwrap();
    assert!(!check.ok);
    assert!(
        check.detail.contains("removed outside unpakt"),
        "{}",
        check.detail
    );
}

#[test]
fn remove_native_without_recorded_name_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    let db = rusqlite::Connection::open(&engine.layout().db).unwrap();
    db.execute("DELETE FROM options WHERE key = 'pk.name'", [])
        .unwrap();
    drop(db);
    pk.0.lock().unwrap().calls.clear();
    match engine.remove("hello") {
        Err(Error::PackageKit(m)) => assert!(m.contains("no recorded package name"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(engine.get("hello").is_ok());
    assert_eq!(journals(engine.layout()), 0);
    assert!(pk.0.lock().unwrap().calls.is_empty());
}

#[test]
fn remove_native_resolve_failure_keeps_record() {
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = engine_with_hello(root.path(), &pk);
    pk.0.lock().unwrap().fail_resolve_once = true;
    assert!(matches!(engine.remove("hello"), Err(Error::PackageKit(_))));
    assert!(engine.get("hello").is_ok());
    assert_eq!(journals(engine.layout()), 0);
    assert!(removes(&pk).is_empty());
}

fn publish_hello(forge: &common::MockForge, tag: &str, version: &str) {
    let src = tempfile::tempdir().unwrap();
    let deb = hello_deb_at(src.path(), version, None);
    forge.publish(
        tag,
        vec![(format!("hello_{version}.deb"), std::fs::read(&deb).unwrap())],
    );
}

/// A native `hello` 1.0 installed from the forge's first release.
fn forge_hello(root: &Path, pk: &FakePk, forge: &common::MockForge) -> Engine {
    publish_hello(forge, "1.0", "1.0");
    let mut engine = native_engine(root, pk).with_http(Http::new(&forge.api_url(), None).unwrap());
    let mut plan = engine
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    plan.native_consent = true;
    engine.install(plan).unwrap();
    engine
}

/// No native journal and no download left behind.
fn assert_clean(engine: &Engine) {
    assert_eq!(journals(engine.layout()), 0);
    let downloads = engine.layout().cache.join("downloads");
    let left = std::fs::read_dir(&downloads)
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "downloads left in {}", downloads.display());
}

fn update_yes(engine: &mut Engine) -> unpakt_core::Result<Option<String>> {
    let mut confirm = |_: &unpakt_core::engine::NativeConsent| true;
    engine.update_opts(
        "hello",
        UpdateOpts {
            allow_risky: false,
            confirm_native: Some(&mut confirm),
        },
    )
}

fn current_version(engine: &Engine) -> Option<String> {
    let (app, versions, _) = engine.get("hello").unwrap();
    versions
        .into_iter()
        .find(|v| v.label == app.current)
        .unwrap()
        .version
}

fn package_id(root: &Path) -> Option<String> {
    let manifest = Manifest::open(&Layout::under(root).db).unwrap();
    manifest.option("hello", "pk.package_id").unwrap()
}

#[test]
fn native_update_asks_and_installs() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().next_install = PkPackage::parse("hello;2.0;amd64;local");

    let mut asked = Vec::new();
    let mut confirm = |c: &unpakt_core::engine::NativeConsent| {
        asked.push(c.clone());
        true
    };
    let label = engine
        .update_opts(
            "hello",
            UpdateOpts {
                allow_risky: false,
                confirm_native: Some(&mut confirm),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].version, "2.0");
    assert_eq!(installs(&pk), 2);
    let (app, _, _) = engine.get("hello").unwrap();
    assert_eq!(app.current, label);
    assert_eq!(app.backend, "packagekit");
    assert_eq!(current_version(&engine).as_deref(), Some("2.0"));
    assert_eq!(
        package_id(root.path()).as_deref(),
        Some("hello;2.0;amd64;local")
    );
    assert_eq!(journals(engine.layout()), 0);
}

#[test]
fn native_update_without_confirm_needs_consent() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().next_install = PkPackage::parse("hello;2.0;amd64;local");
    assert!(matches!(
        engine.update_with("hello", false),
        Err(Error::NeedsConsent(_))
    ));
    assert_eq!(current_version(&engine).as_deref(), Some("1.0"));
    assert_eq!(installs(&pk), 1);
    assert_clean(&engine);
}

#[test]
fn native_update_declined_keeps_current() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    let mut confirm = |_: &unpakt_core::engine::NativeConsent| false;
    match engine.update_opts(
        "hello",
        UpdateOpts {
            allow_risky: false,
            confirm_native: Some(&mut confirm),
        },
    ) {
        Err(Error::Refused(m)) => assert!(m.contains("declined"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(current_version(&engine).as_deref(), Some("1.0"));
    assert_eq!(installs(&pk), 1);
    assert_clean(&engine);
}

#[test]
fn system_newer_records_and_skips() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().installed.insert(
        "hello".into(),
        PkPackage::parse("hello;2.1;amd64;local").unwrap(),
    );
    let mut ran = false;
    let mut confirm = |_: &unpakt_core::engine::NativeConsent| {
        ran = true;
        true
    };
    let result = engine
        .update_opts(
            "hello",
            UpdateOpts {
                allow_risky: false,
                confirm_native: Some(&mut confirm),
            },
        )
        .unwrap();
    assert_eq!(result, None);
    assert!(!ran);
    assert_eq!(installs(&pk), 1);
    assert_eq!(current_version(&engine).as_deref(), Some("2.1"));
    assert_eq!(
        package_id(root.path()).as_deref(),
        Some("hello;2.1;amd64;local")
    );
    assert!(
        engine
            .check_updates(Some("hello"))
            .unwrap()
            .updates
            .is_empty()
    );
}

#[test]
fn system_epoch_newer_skips() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().installed.insert(
        "hello".into(),
        PkPackage::parse("hello;1:1.5;amd64;local").unwrap(),
    );
    assert_eq!(engine.update_with("hello", false).unwrap(), None);
    assert_eq!(installs(&pk), 1);
}

#[test]
fn native_package_gone_is_not_reinstalled() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().installed.clear();
    match engine.update_with("hello", false) {
        Err(Error::NotFound(m)) => assert!(m.contains("unpakt remove hello"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(installs(&pk), 1);
    assert_clean(&engine);
}

#[test]
fn native_update_without_packagekit_keeps_current() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    let mut engine = engine.with_packagekit(Box::new(unpakt_core::packagekit::Unavailable));
    match engine.update_with("hello", false) {
        Err(Error::PackageKitUnavailable(m)) => assert!(m.contains("not available now"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(current_version(&engine).as_deref(), Some("1.0"));
    assert_eq!(installs(&pk), 1);
    assert_clean(&engine);
}

#[test]
fn extract_app_stays_extract_when_packagekit_appears() {
    let forge = common::MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let (name, deb) = common::deb_with_elf(src.path(), "1.0.0", &[], &[], false);
    forge.publish("1.0.0", vec![(name, std::fs::read(&deb).unwrap())]);
    let off = FakePk::default();
    let mut engine =
        native_engine(root.path(), &off).with_http(Http::new(&forge.api_url(), None).unwrap());
    let plan = engine
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    assert_eq!(plan.route(), Some(Route::Extract));
    engine.install(plan).unwrap();
    drop(engine);

    let (name, deb) = common::deb_with_elf(src.path(), "2.0.0", &[], &[], false);
    forge.publish("v2.0.0", vec![(name, std::fs::read(&deb).unwrap())]);
    let on = hello_fake();
    let mut engine =
        native_engine(root.path(), &on).with_http(Http::new(&forge.api_url(), None).unwrap());
    assert!(engine.update_with("foo", false).unwrap().is_some());
    assert_eq!(engine.get("foo").unwrap().0.backend, "extract");
    assert_eq!(installs(&on), 0);
}

#[test]
fn interrupted_native_update_is_recorded_on_next_run() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    for (tag, version) in [("v2.0", "2.0"), ("v3.0", "3.0"), ("v4.0", "4.0")] {
        publish_hello(&forge, tag, version);
        {
            let mut state = pk.0.lock().unwrap();
            state.next_install = PkPackage::parse(&format!("hello;{version};amd64;local"));
            // The update looks the package up first, then again after installing.
            state.fail_resolve_once = version == "4.0";
            state.resolves_before_fail = 1;
        }
        let mut confirm = |_: &unpakt_core::engine::NativeConsent| true;
        let result = engine.update_opts(
            "hello",
            UpdateOpts {
                allow_risky: false,
                confirm_native: Some(&mut confirm),
            },
        );
        if version == "4.0" {
            assert!(matches!(result, Err(Error::PackageKit(_))), "{result:?}");
        } else {
            result.unwrap();
        }
    }
    assert_eq!(current_version(&engine).as_deref(), Some("3.0"));
    assert_eq!(journals(engine.layout()), 1);

    let recovered = engine.recover().unwrap();
    assert_eq!(recovered.native, 1);
    assert_eq!(current_version(&engine).as_deref(), Some("4.0"));
    assert_eq!(journals(engine.layout()), 0);
    let (app, versions, _) = engine.get("hello").unwrap();
    assert_eq!(app.current, "4.0");
    assert_eq!(versions.len(), unpakt_core::engine::KEEP_VERSIONS);
    assert!(versions.iter().all(|v| v.label != "1.0"));
}

#[test]
fn system_version_matching_an_old_row_becomes_current() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().next_install = PkPackage::parse("hello;2.0;amd64;local");
    update_yes(&mut engine).unwrap();
    assert_eq!(engine.get("hello").unwrap().1.len(), 2);

    // The system went back to 1.0 behind unpakt's back.
    publish_hello(&forge, "v3.0", "1.0");
    pk.0.lock().unwrap().installed.insert(
        "hello".into(),
        PkPackage::parse("hello;1.0;amd64;local").unwrap(),
    );
    assert_eq!(update_yes(&mut engine).unwrap(), None);
    let (app, versions, _) = engine.get("hello").unwrap();
    assert_eq!(app.current, "1.0");
    assert_eq!(versions.len(), 2);
    assert_eq!(installs(&pk), 2);
    assert_clean(&engine);
}

#[test]
fn update_to_a_previously_installed_label_commits() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    // The system package manager moves hello ahead twice; each is recorded
    // under the system's own version label.
    for (tag, sys) in [("v2.0", "2.5"), ("v2.1", "2.6")] {
        publish_hello(&forge, tag, "2.0");
        pk.0.lock().unwrap().installed.insert(
            "hello".into(),
            PkPackage::parse(&format!("hello;{sys};amd64;local")).unwrap(),
        );
        assert_eq!(update_yes(&mut engine).unwrap(), None);
    }
    assert_eq!(engine.get("hello").unwrap().1.len(), 3);

    // A release tagged like an old row, carrying a newer package.
    publish_hello(&forge, "v2.5", "3.0");
    pk.0.lock().unwrap().next_install = PkPackage::parse("hello;3.0;amd64;local");
    assert_eq!(update_yes(&mut engine).unwrap().as_deref(), Some("2.5"));
    let (app, versions, _) = engine.get("hello").unwrap();
    assert_eq!(app.current, "2.5");
    assert_eq!(versions.len(), 3);
    assert_eq!(current_version(&engine).as_deref(), Some("3.0"));
    assert_clean(&engine);
}

#[test]
fn native_updates_past_the_keep_limit_prune_old_versions() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    for version in ["2.0", "3.0", "4.0"] {
        publish_hello(&forge, &format!("v{version}"), version);
        pk.0.lock().unwrap().next_install =
            PkPackage::parse(&format!("hello;{version};amd64;local"));
        update_yes(&mut engine).unwrap();
    }
    let (app, versions, _) = engine.get("hello").unwrap();
    assert_eq!(app.current, "4.0");
    let labels: Vec<_> = versions.iter().map(|v| v.label.as_str()).collect();
    assert_eq!(labels, ["4.0", "3.0", "2.0"]);
    assert_clean(&engine);
}

#[test]
fn native_update_refreshes_the_package_name() {
    let forge = common::MockForge::start();
    let root = tempfile::tempdir().unwrap();
    let pk = hello_fake();
    let mut engine = forge_hello(root.path(), &pk, &forge);
    publish_hello(&forge, "v2.0", "2.0");
    pk.0.lock().unwrap().next_install = PkPackage::parse("hello;2.0;amd64;local");
    update_yes(&mut engine).unwrap();
    let manifest = Manifest::open(&Layout::under(root.path()).db).unwrap();
    assert_eq!(
        manifest.option("hello", "pk.name").unwrap().as_deref(),
        Some("hello")
    );
}
