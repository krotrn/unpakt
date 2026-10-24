mod common;

use common::tree_snapshot;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use unpakt_core::Txn;

fn seed(root: &Path) {
    fs::create_dir_all(root.join("a")).unwrap();
    fs::write(root.join("a/old.txt"), b"old").unwrap();
    symlink(root.join("a"), root.join("link")).unwrap();
}

fn all_ops(root: &Path) -> Txn {
    let mut t = Txn::new();
    t.create_dir_all(&root.join("x/y")).unwrap();
    t.write(&root.join("a/old.txt"), b"new", 0o644).unwrap();
    t.write(&root.join("x/y/f"), b"f", 0o755).unwrap();
    t.symlink_swap(&root.join("x"), &root.join("link")).unwrap();
    t.remove_dir_all(&root.join("a")).unwrap();
    t
}

fn no_leftovers(root: &Path) {
    for k in tree_snapshot(root).keys() {
        let s = k.to_string_lossy();
        assert!(!s.contains(".unpakt-"), "leftover: {s}");
    }
}

#[test]
fn uncommitted_txn_restores_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    let before = tree_snapshot(root);
    {
        let _t = all_ops(root);
    }
    assert_eq!(tree_snapshot(root), before);
}

#[test]
fn committed_txn_keeps_changes_and_purges_trash() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    all_ops(root).commit();
    assert!(!root.join("a").exists());
    assert_eq!(fs::read(root.join("x/y/f")).unwrap(), b"f");
    assert_eq!(fs::read_link(root.join("link")).unwrap(), root.join("x"));
    no_leftovers(root);
}

#[test]
fn remove_dir_all_on_symlink_removes_only_link() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"k").unwrap();
    symlink(outside.path(), root.join("l")).unwrap();
    let mut t = Txn::new();
    t.remove_dir_all(&root.join("l")).unwrap();
    t.commit();
    assert!(fs::symlink_metadata(root.join("l")).is_err());
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"k");
    no_leftovers(root);
}

#[test]
fn symlink_swap_is_atomic_replace() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    symlink("v1", root.join("cur")).unwrap();
    let mut t = Txn::new();
    t.symlink_swap(Path::new("v2"), &root.join("cur")).unwrap();
    assert_eq!(fs::read_link(root.join("cur")).unwrap(), Path::new("v2"));
    t.commit();
    assert_eq!(fs::read_link(root.join("cur")).unwrap(), Path::new("v2"));
    no_leftovers(root);
}

#[test]
fn rename_and_remove_file_roll_back() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("f"), b"1").unwrap();
    fs::write(root.join("g"), b"2").unwrap();
    let before = tree_snapshot(root);
    {
        let mut t = Txn::new();
        t.rename(&root.join("f"), &root.join("h")).unwrap();
        t.remove_file(&root.join("g")).unwrap();
        fs::write(root.join("made"), b"x").unwrap();
        t.track_new(&root.join("made"));
        assert!(t.rename(&root.join("h"), &root.join("made")).is_err());
    }
    assert_eq!(tree_snapshot(root), before);
}

#[test]
fn create_dir_all_over_file_fails_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("f"), b"1").unwrap();
    symlink("nowhere", root.join("dangling")).unwrap();
    let before = tree_snapshot(root);
    let mut t = Txn::new();
    assert!(t.create_dir_all(&root.join("f")).is_err());
    assert!(t.create_dir_all(&root.join("f/sub")).is_err());
    assert!(t.create_dir_all(&root.join("dangling")).is_err());
    drop(t);
    assert_eq!(tree_snapshot(root), before);
}

#[test]
fn write_over_existing_replaces_and_rolls_back_content_and_mode() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("f");
    fs::write(&p, b"old").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    let before = tree_snapshot(tmp.path());
    {
        let mut t = Txn::new();
        t.write(&p, b"new", 0o755).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert!(t.write(tmp.path(), b"x", 0o644).is_err());
    }
    assert_eq!(fs::read(&p).unwrap(), b"old");
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(tree_snapshot(tmp.path()), before);
}

#[test]
fn owns_only_tracked_backups() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f");
    fs::write(&f, "old").unwrap();
    let stray = dir.path().join(".unpakt-bak-x");
    fs::write(&stray, "user").unwrap();
    let mut t = Txn::new();
    t.write(&f, b"new", 0o644).unwrap();
    let backup = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p != &f
                && p != &stray
                && p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".unpakt-bak-")
        })
        .unwrap();
    assert!(t.owns(&backup));
    assert!(!t.owns(&stray));
    assert!(!t.owns(&f));
}

#[test]
fn remove_file_refuses_directories() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("d");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/f"), "x").unwrap();
    let before = tree_snapshot(root.path());
    let mut txn = Txn::new();
    assert!(txn.remove_file(&dir).is_err());
    txn.commit();
    assert_eq!(tree_snapshot(root.path()), before);

    // A symlink to a directory is still a link and may be removed as one.
    let link = root.path().join("l");
    symlink(&dir, &link).unwrap();
    let mut txn = Txn::new();
    txn.remove_file(&link).unwrap();
    txn.commit();
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(dir.join("sub/f").is_file());
}
