mod common;

use std::path::Path;

use common::fake_pk::FakePk;
use unpakt_core::Error;
use unpakt_core::cancel::Cancel;
use unpakt_core::packagekit::{PackageKit, PkEvent, PkPackage};

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
