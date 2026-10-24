mod common;

use common::{Comp, E, MockForge, build_appimage, build_tarball, marker_script};
use std::ffi::OsString;
use unpakt_core::Layout;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::format::detect;
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::model::Artifact;

#[test]
fn inspect_never_executes_input() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("marker");
    let script = marker_script(&marker);

    let tarball = build_tarball(
        d.path(),
        "app-1.0.tar.gz",
        Comp::Gz,
        &[E::File("bin/app", &script, 0o755)],
    );
    let appimage = build_appimage(
        d.path(),
        "App-1.0.AppImage",
        &[E::File("AppRun", &script, 0o755)],
    );

    for path in [tarball, appimage] {
        let a = Artifact {
            file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path,
        };
        detect(&a).unwrap().inspect(&a).unwrap();
        assert!(!marker.exists(), "inspecting {} ran its input", a.file_name);
    }
}

#[test]
fn install_never_executes_input() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("marker");
    let script = marker_script(&marker);

    let tarball = build_tarball(
        d.path(),
        "unpakt-fixture-run-1.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-run/bin/unpakt-fixture-run",
            &script,
            0o755,
        )],
    );
    let appimage = build_appimage(
        d.path(),
        "Unpakt-Fixture-Image-1.0.AppImage",
        &[E::File("AppRun", &script, 0o755)],
    );

    for path in [tarball, appimage] {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::under(root.path());
        let bin = OsString::from(&layout.bin);
        let mut engine = Engine::new(
            layout,
            Host {
                fuse2: true,
                ..Host::detect()
            },
        )
        .unwrap()
        .with_path_var(bin);
        let plan = engine
            .inspect(path.to_str().unwrap(), AddOpts::default())
            .unwrap();
        let id = plan.plan.id.clone();
        engine.install(plan).unwrap();
        assert!(
            !marker.exists(),
            "installing {} ran its input",
            path.display()
        );
        engine.remove(&id).unwrap();
        assert!(
            !marker.exists(),
            "removing {} ran its input",
            path.display()
        );
    }
}

#[test]
fn update_never_executes_input() {
    let forge = MockForge::start();
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("marker");
    let script = marker_script(&marker);

    let publish = |version: &str| {
        let name = format!("Unpakt-Fixture-Marker-{version}.AppImage");
        let image = build_appimage(d.path(), &name, &[E::File("AppRun", &script, 0o755)]);
        forge.publish(version, vec![(name, std::fs::read(image).unwrap())]);
    };

    let root = tempfile::tempdir().unwrap();
    let layout = Layout::under(root.path());
    let bin = OsString::from(&layout.bin);
    let mut engine = Engine::new(
        layout,
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
    .unwrap()
    .with_path_var(bin)
    .with_http(Http::new(&forge.api_url(), None).unwrap());

    publish("1.0.0");
    let plan = engine
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    let id = plan.plan.id.clone();
    engine.install(plan).unwrap();

    publish("2.0.0");
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");
    assert!(!marker.exists(), "updating ran its input");
}
