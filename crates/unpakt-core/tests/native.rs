mod common;

use std::path::Path;

use common::fake_pk::FakePk;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine, Route};
use unpakt_core::host::Host;
use unpakt_core::packagekit::{PackageKit, PkEvent, PkPackage};
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
