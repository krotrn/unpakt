mod common;

use std::path::Path;

use common::fake_pk::FakePk;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
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
