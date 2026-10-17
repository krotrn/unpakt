mod common;

use common::{Comp, E, build_appimage, build_tarball, marker_script};
use unpakt_core::format::detect;
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
