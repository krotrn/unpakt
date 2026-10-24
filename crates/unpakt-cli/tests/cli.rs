use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use flate2::Compression;
use flate2::write::GzEncoder;

fn fixture_tarball(dir: &Path) -> PathBuf {
    fixture_tarball_named(dir, "unpakt-fixture-tool-1.0.tar.gz")
}

fn fixture_tarball_named(dir: &Path, file_name: &str) -> PathBuf {
    let path = dir.join(file_name);
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
    let apps = tmp.path().join(".local/share/unpakt/apps");
    let entries: Vec<_> = std::fs::read_dir(&apps).unwrap().collect();
    assert!(entries.is_empty(), "no app dir was created: {entries:?}");
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
    let source = tarball.canonicalize().unwrap();
    assert!(list.contains(source.to_str().unwrap()), "{list}");
    assert!(list.contains("unpakt-fixture-tool"), "{list}");

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
    let source = tarball.canonicalize().unwrap();
    assert!(
        text.contains(&format!("source:  {}", source.display())),
        "{text}"
    );
    assert!(
        text.contains("asset:   unpakt-fixture-tool-1.0.tar.gz"),
        "{text}"
    );
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
    assert_eq!(stdout(&all), "all apps are up to date\n");
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

/// Spawns `add` on a pseudo-terminal so the confirmation prompt is shown, and
/// returns the child plus the pty master, which the caller keeps alive.
fn spawn_at_prompt(root: &Path, input: &Path) -> (std::process::Child, std::fs::File) {
    use std::os::fd::{FromRawFd, OwnedFd};

    let (mut master, mut slave) = (0, 0);
    // SAFETY: both out-pointers are valid; the null pointers request defaults.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    // SAFETY: openpty returned two fresh descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let child = Command::new(env!("CARGO_BIN_EXE_unpakt"))
        .env_clear()
        .env("UNPAKT_ROOT", root)
        .env("HOME", root)
        .env(
            "PATH",
            format!("{}/.local/bin:/usr/bin:/bin", root.display()),
        )
        .args(["add", input.to_str().unwrap()])
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    (child, std::fs::File::from(master))
}

/// Kills and reaps the child on every path, including a panicking test.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn single_interrupt_at_the_prompt_exits_130_and_installs_nothing() {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let (child, _master) = spawn_at_prompt(tmp.path(), &tarball);
    let mut child = KillOnDrop(child);

    let mut err = child.0.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok(n) = err.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.contains("[y/N]") {
        let left = deadline.saturating_duration_since(Instant::now());
        let chunk = rx.recv_timeout(left).expect("prompt never appeared");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }

    // SAFETY: plain signal delivery to our own child.
    unsafe { libc::kill(child.0.id() as i32, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            panic!("still running 5s after a single interrupt");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
    let list = run(tmp.path(), &["list"]);
    assert_eq!(stdout(&list), "");
    let apps = tmp.path().join(".local/share/unpakt/apps");
    let entries: Vec<_> = std::fs::read_dir(&apps).unwrap().collect();
    assert!(entries.is_empty(), "no app dir was created: {entries:?}");
}

#[test]
fn update_check_says_so_when_nothing_is_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["update", "--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "all apps are up to date\n");
}

#[test]
fn update_check_with_an_id_names_the_app() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    let id = stdout(&run(tmp.path(), &["list"]))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let out = run(tmp.path(), &["update", "--check", &id]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("{id} is up to date\n"));
}

#[test]
fn summary_version_matches_info_format() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let out = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let id = stdout(&run(tmp.path(), &["list"]))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let info = stdout(&run(tmp.path(), &["info", &id]));
    let line = |text: &str| {
        text.lines()
            .find(|l| l.starts_with("version:"))
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(line(&stdout(&out)), line(&info), "{}\n{info}", stdout(&out));
}

#[test]
fn summary_version_has_no_parentheses_when_label_is_the_raw_version() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let out = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("\nversion: 1.0\n"),
        "{}",
        stdout(&out)
    );
}
