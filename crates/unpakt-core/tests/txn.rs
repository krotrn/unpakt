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
