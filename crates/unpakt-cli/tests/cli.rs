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

fn ar_member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    out.extend_from_slice(
        format!(
            "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            0,
            0,
            0,
            "100644",
            data.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(b'\n');
    }
}

fn gz_tar(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for (path, body, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(*mode);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, format!("./{path}"), *body)
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// A deb whose postinst creates an account, which the compatibility check
/// reports as risky.
fn risky_deb(dir: &Path) -> PathBuf {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let control = format!(
        "Package: riskyfoo\nVersion: 1.0\nArchitecture: {arch}\nMaintainer: t <t@example.org>\n"
    );
    let mut bytes = b"!<arch>\n".to_vec();
    ar_member(&mut bytes, "debian-binary", b"2.0\n");
    ar_member(
        &mut bytes,
        "control.tar.gz",
        &gz_tar(&[
            ("control", control.as_bytes(), 0o644),
            ("postinst", b"#!/bin/sh\nuseradd riskyfoo\n", 0o755),
        ]),
    );
    ar_member(
        &mut bytes,
        "data.tar.gz",
        &gz_tar(&[("opt/riskyfoo/riskyfoo", b"#!/bin/sh\necho hi\n", 0o755)]),
    );
    let path = dir.join("riskyfoo_1.0_pkg.deb");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unpakt"))
        .env_clear()
        .env("UNPAKT_ROOT", root)
        .env("UNPAKT_PACKAGEKIT", "0")
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
    assert_eq!(stdout(&all), "no apps with an update source\n");
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
        .env("UNPAKT_PACKAGEKIT", "0")
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
    assert_eq!(stdout(&out), "no apps installed\n");
}

#[test]
fn update_check_of_a_local_file_app_says_it_has_no_source() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    let id = stdout(&run(tmp.path(), &["list"]))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let out = run(tmp.path(), &["update", "--check", &id]);
    assert_eq!(stdout(&out), "");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains(&format!(
            "{id} was installed from a local file and has no update source"
        )),
        "{}",
        stderr(&out)
    );
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

#[test]
fn update_check_of_an_unknown_id_fails_naming_it() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["update", "--check", "no-such-app"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no-such-app"), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

#[test]
fn remove_reports_waiting_for_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let tarball = fixture_tarball(tmp.path());
    let added = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(added.status.code(), Some(0), "{}", stderr(&added));
    let list = stdout(&run(tmp.path(), &["list"]));
    let id = list.split_whitespace().next().expect("one listed app");

    let held =
        unpakt_core::lock::acquire(&tmp.path().join(".local/share/unpakt/lock"), || {}).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_unpakt"))
        .env_clear()
        .env("UNPAKT_ROOT", tmp.path())
        .env("UNPAKT_PACKAGEKIT", "0")
        .env("HOME", tmp.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["remove", id, "--yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("waiting for another unpakt operation to finish…"));
}

#[test]
fn completion_scripts_cover_every_command() {
    let tmp = tempfile::tempdir().unwrap();
    for (shell, marker) in [
        ("bash", "complete -F _unpakt"),
        ("zsh", "#compdef unpakt"),
        ("fish", "complete -c unpakt"),
    ] {
        let out = run(tmp.path(), &["completion", shell]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        let script = stdout(&out);
        assert!(script.contains(marker), "{shell}: {script}");
        for command in [
            "add", "inspect", "list", "info", "update", "rollback", "remove", "doctor", "self",
        ] {
            assert!(script.contains(command), "{shell} misses {command}");
        }
    }
    let data = tmp.path().join(".local/share/unpakt");
    assert!(!data.exists(), "completion must not create unpakt's data");
}

// ── unpakt self ──────────────────────────────────────────────────────────────

const NEW_VERSION: &str = "99.0.0";

/// Serves `files` (URL path -> body) on a local port until the test ends.
fn serve(files: Vec<(String, Vec<u8>)>) -> String {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let reply = match files.iter().find(|(p, _)| p == path) {
                Some((_, body)) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    [head.as_bytes(), body].concat()
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&reply);
        }
    });
    base
}

/// A release server offering the static binary `body` as `NEW_VERSION`.
fn release_server(body: &[u8], sum: Option<String>) -> String {
    use sha2::{Digest, Sha256};

    let name = format!("unpakt-{NEW_VERSION}-x86_64-linux-musl");
    let sum = sum.unwrap_or_else(|| hex::encode(Sha256::digest(body)));
    serve(vec![
        (
            "/releases/latest/download/SHA256SUMS".into(),
            format!("{sum}  {name}\n").into_bytes(),
        ),
        (
            format!("/releases/download/v{NEW_VERSION}/{name}"),
            body.to_vec(),
        ),
    ])
}

/// Copies the built binary to where get.sh puts the command-line install.
fn install_binary_copy(root: &Path) -> PathBuf {
    let bin = root.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("unpakt");
    std::fs::copy(env!("CARGO_BIN_EXE_unpakt"), &exe).unwrap();
    exe
}

fn run_exe(exe: &Path, root: &Path, repo: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::new(exe);
    cmd.env_clear()
        .env("UNPAKT_ROOT", root)
        .env("UNPAKT_PACKAGEKIT", "0")
        .env("HOME", root)
        .env(
            "PATH",
            format!("{}/.local/bin:/usr/bin:/bin", root.display()),
        )
        .args(args)
        .stdin(Stdio::null());
    if let Some(repo) = repo {
        cmd.env("UNPAKT_REPO_URL", repo);
    }
    cmd.output().unwrap()
}

#[test]
fn self_update_check_prints_versions_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = release_server(b"new", None);
    let out = run_exe(
        Path::new(env!("CARGO_BIN_EXE_unpakt")),
        tmp.path(),
        Some(&repo),
        &["self", "update", "--check"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(&format!("current: {}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert!(text.contains(&format!("latest:  {NEW_VERSION}")), "{text}");
    assert!(!tmp.path().join(".local/share/unpakt").exists());
}

#[test]
fn self_uninstall_without_yes_on_non_tty_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = install_binary_copy(tmp.path());
    let out = run_exe(&exe, tmp.path(), None, &["self", "uninstall"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert!(exe.exists());
}

#[test]
fn self_update_replaces_the_command_line_binary() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let exe = install_binary_copy(tmp.path());
    let fish = tmp.path().join(".config/fish/completions/unpakt.fish");
    std::fs::create_dir_all(fish.parent().unwrap()).unwrap();
    std::fs::write(&fish, "# old\n").unwrap();
    let repo = release_server(b"#!/bin/sh\necho new unpakt\n", None);
    let out = run_exe(&exe, tmp.path(), Some(&repo), &["self", "update"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    // The new binary's output replaces the stale completion.
    assert_eq!(std::fs::read_to_string(&fish).unwrap(), "new unpakt\n");
    assert_eq!(
        std::fs::read_to_string(&exe).unwrap(),
        "#!/bin/sh\necho new unpakt\n"
    );
    assert_eq!(
        std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let leftovers: Vec<_> = std::fs::read_dir(exe.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "temp file left behind: {leftovers:?}");
}

#[test]
fn self_update_with_a_bad_checksum_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = install_binary_copy(tmp.path());
    let before = std::fs::read(&exe).unwrap();
    let repo = release_server(b"tampered", Some("0".repeat(64)));
    let out = run_exe(&exe, tmp.path(), Some(&repo), &["self", "update"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("checksum"), "{}", stderr(&out));
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[test]
fn self_uninstall_removes_the_binary_and_keeps_the_data() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = install_binary_copy(tmp.path());
    let bash = tmp
        .path()
        .join(".local/share/bash-completion/completions/unpakt");
    let fish = tmp.path().join(".config/fish/completions/unpakt.fish");
    for f in [&bash, &fish] {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "# completion\n").unwrap();
    }
    let tarball = fixture_tarball(tmp.path());
    let added = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(added.status.code(), Some(0), "{}", stderr(&added));

    let out = run_exe(&exe, tmp.path(), None, &["self", "uninstall", "--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!exe.exists() && !bash.exists() && !fish.exists());
    assert!(tmp.path().join(".local/share/unpakt/db.sqlite").exists());
    assert!(stdout(&out).contains("--purge"), "{}", stdout(&out));
}

#[test]
fn self_uninstall_purge_removes_apps_and_data() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = install_binary_copy(tmp.path());
    let tarball = fixture_tarball(tmp.path());
    let added = run(tmp.path(), &["add", "--yes", tarball.to_str().unwrap()]);
    assert_eq!(added.status.code(), Some(0), "{}", stderr(&added));
    let command = tmp.path().join(".local/bin/unpakt-fixture-tool");
    assert!(command.exists(), "the app's command was placed");

    let out = run_exe(
        &exe,
        tmp.path(),
        None,
        &["self", "uninstall", "--purge", "--yes"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!exe.exists());
    assert!(!command.exists(), "the app's command is gone");
    assert!(!tmp.path().join(".local/share/unpakt").exists());
}

#[test]
fn self_uninstall_fails_when_the_install_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    // The built binary sits in target/, which is no install location.
    let out = run(tmp.path(), &["self", "uninstall", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("#install"), "{}", stderr(&out));
}

#[test]
fn add_risky_deb_is_refused_and_installs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let deb = risky_deb(tmp.path());
    let out = run(tmp.path(), &["add", "--yes", deb.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(stderr(&out).contains("--allow-risky"), "{}", stderr(&out));
    let list = run(tmp.path(), &["list"]);
    assert!(!stdout(&list).contains("riskyfoo"), "{}", stdout(&list));
}
