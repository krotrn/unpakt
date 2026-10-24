use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use flate2::Compression;
use flate2::write::GzEncoder;

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

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unpakt"))
        .env_clear()
        .env("UNPAKT_ROOT", root)
        .env("HOME", root)
        .env(
            "PATH",
            format!("{}/.local/bin:/usr/bin:/bin", root.display()),
        )
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn add_without_yes_on_non_tty_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let out = run(tmp.path(), &["add", tarball.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    let list = run(tmp.path(), &["list"]);
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(stdout(&list), "");
}

#[test]
fn add_list_info_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let out = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let list = stdout(&run(tmp.path(), &["list"]));
    let id = list.split_whitespace().next().expect("one listed app");
    assert!(id.contains("fixture"), "{list}");
    assert!(list.contains("tarball"), "{list}");

    let info = run(tmp.path(), &["info", id]);
    assert_eq!(info.status.code(), Some(0));
    let label = list.split_whitespace().nth(1).unwrap();
    assert!(stdout(&info).contains(label), "{}", stdout(&info));

    let again = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(1));
    assert!(stderr(&again).starts_with("error: "), "{}", stderr(&again));

    let refused = run(tmp.path(), &["remove", id]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(!stdout(&run(tmp.path(), &["list"])).is_empty());

    let removed = run(tmp.path(), &["remove", id, "--yes"]);
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert_eq!(stdout(&run(tmp.path(), &["list"])), "");
}

#[test]
fn inspect_prints_candidates_without_installing() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let out = run(tmp.path(), &["inspect", tarball.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("bin/unpakt-fixture-tool"), "{text}");
    assert_eq!(stdout(&run(tmp.path(), &["list"])), "");
}

#[test]
fn doctor_reports_checks_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["doctor"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("bin-in-path"), "{}", stdout(&out));
}

#[test]
fn rollback_without_older_version_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let out = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let list = stdout(&run(tmp.path(), &["list"]));
    let id = list.split_whitespace().next().unwrap();
    let label = list.split_whitespace().nth(1).unwrap();

    let out = run(tmp.path(), &["rollback", id]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stderr(&out).starts_with("error: "), "{}", stderr(&out));
    let info = stdout(&run(tmp.path(), &["info", id]));
    assert!(info.contains(&format!("current: {label}")), "{info}");
}

#[test]
fn update_of_local_file_app_is_reported_not_attempted() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    let id = stdout(&run(tmp.path(), &["list"]))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    let all = run(tmp.path(), &["update", "--check"]);
    assert_eq!(all.status.code(), Some(0), "{}", stderr(&all));
    assert_eq!(stdout(&all), "");
    let all = run(tmp.path(), &["update"]);
    assert_eq!(all.status.code(), Some(0), "{}", stderr(&all));

    let one = run(tmp.path(), &["update", &id]);
    assert_eq!(one.status.code(), Some(1));
    assert!(stderr(&one).contains("local file"), "{}", stderr(&one));
}

#[test]
fn add_accepts_a_pattern_and_validates_it() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let bad = run(
        tmp.path(),
        &["add", "--yes", "--pattern", "(", tarball.to_str().unwrap()],
    );
    assert_eq!(bad.status.code(), Some(1), "{}", stderr(&bad));
    assert!(
        stderr(&bad).contains("invalid --pattern"),
        "{}",
        stderr(&bad)
    );

    let good = run(
        tmp.path(),
        &[
            "add",
            "--yes",
            "--pattern",
            "fixture-tool",
            tarball.to_str().unwrap(),
        ],
    );
    assert_eq!(good.status.code(), Some(0), "{}", stderr(&good));
    assert!(!stdout(&run(tmp.path(), &["list"])).is_empty());
}
