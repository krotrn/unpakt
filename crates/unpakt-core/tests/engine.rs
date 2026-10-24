mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Comp, E, build_appimage, build_tarball, tree_snapshot};
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::host::Host;
use unpakt_core::{Error, Layout};

const ELF: &[u8] = b"\x7fELF-fixture";
const TOOL_DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Unpakt Fixture Tool\nExec=unpakt-fixture-tool %U\nIcon=unpakt-fixture-tool\nCategories=Utility;\n";
const APP_DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Unpakt Fixture App\nExec=AppRun\nIcon=unpakt-fixture-app\nCategories=Utility;\n";

fn png(width: u32) -> Vec<u8> {
    let mut data = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    data.extend_from_slice(b"IHDR");
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&width.to_be_bytes());
    data
}

fn engine(root: &Path) -> Engine {
    let layout = Layout::under(root);
    let bin = layout.bin.clone();
    Engine::new(layout, Host::detect())
        .unwrap()
        .with_path_var(OsString::from(bin))
}

fn tool_tarball(dir: &Path) -> PathBuf {
    let icon = png(128);
    build_tarball(
        dir,
        "unpakt-fixture-tool-1.0.tar.gz",
        Comp::Gz,
        &[
            E::File("unpakt-fixture-tool/bin/unpakt-fixture-tool", ELF, 0o755),
            E::File(
                "unpakt-fixture-tool/share/applications/unpakt-fixture-tool.desktop",
                TOOL_DESKTOP,
                0o644,
            ),
            E::File(
                "unpakt-fixture-tool/share/icons/hicolor/128x128/apps/unpakt-fixture-tool.png",
                &icon,
                0o644,
            ),
        ],
    )
}

fn app_image(dir: &Path) -> PathBuf {
    let icon = png(64);
    build_appimage(
        dir,
        "Unpakt-Fixture-App-2.1.AppImage",
        &[
            E::File("AppRun", ELF, 0o755),
            E::File("unpakt-fixture-app.desktop", APP_DESKTOP, 0o644),
            E::File("unpakt-fixture-app.png", &icon, 0o644),
        ],
    )
}

fn validate(desktop: &Path) {
    let Ok(out) = Command::new("desktop-file-validate").arg(desktop).output() else {
        eprintln!("desktop-file-validate not installed; skipping");
        return;
    };
    assert!(
        out.status.success(),
        "desktop-file-validate: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn install_and_remove(artifact: &Path, root: &Path, id: &str, command: &str) {
    let mut engine = engine(root);
    let layout = engine.layout().clone();
    let baseline = tree_snapshot(root);

    let plan = engine
        .inspect(artifact.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert_eq!(plan.plan.id, id);
    let app = engine.install(plan).unwrap();
    assert_eq!(app.id, id);

    let desktop = layout.applications.join(format!("unpakt-{id}.desktop"));
    assert!(desktop.is_file());
    validate(&desktop);
    assert!(
        std::fs::symlink_metadata(layout.bin.join(command)).is_ok(),
        "bin/{command} is missing"
    );
    assert!(layout.apps.join(id).join("current").exists());
    assert_eq!(engine.list().unwrap().len(), 1);
    let (_, versions, files) = engine.get(id).unwrap();
    assert_eq!(versions.len(), 1);
    assert!(files.len() >= 3);
    let checks = engine.doctor();
    let app_check = checks
        .iter()
        .find(|c| c.name == format!("app:{id}"))
        .unwrap();
    assert!(app_check.ok, "{}", app_check.detail);

    engine.remove(id).unwrap();
    assert!(engine.list().unwrap().is_empty());
    assert!(matches!(engine.get(id), Err(Error::NotFound(_))));
    assert_eq!(tree_snapshot(root), baseline);
}

#[test]
fn install_and_remove_tarball_leaves_zero_files() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = tool_tarball(src.path());
    install_and_remove(
        &tarball,
        root.path(),
        "unpakt-fixture-tool",
        "unpakt-fixture-tool",
    );
}

#[test]
fn install_and_remove_appimage_leaves_zero_files() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let image = app_image(src.path());
    install_and_remove(
        &image,
        root.path(),
        "unpakt-fixture-app",
        "unpakt-fixture-app",
    );
}

/// Reverses the string escape, then the argument quoting, of the first
/// argument of an `Exec` value that starts with an absolute path.
fn first_exec_argv0(value: &str) -> String {
    let mut unescaped = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            unescaped.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => unescaped.push('\n'),
            Some('t') => unescaped.push('\t'),
            Some('r') => unescaped.push('\r'),
            Some('s') => unescaped.push(' '),
            Some(other) => unescaped.push(other),
            None => unescaped.push('\\'),
        }
    }
    let mut args: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut it = unescaped.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            '\\' if in_quotes => {
                if let Some(n) = it.next() {
                    cur.push(n);
                }
            }
            ' ' if !in_quotes => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(cur);
    }
    args.into_iter()
        .map(|a| a.replace("%%", "%"))
        .find(|a| a.starts_with('/'))
        .expect("an absolute program path in Exec")
}

#[test]
fn install_under_root_with_spaces_validates() {
    let src = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("a b $c 100%");
    std::fs::create_dir_all(&root).unwrap();
    let image = app_image(src.path());

    let mut engine = engine(&root);
    let layout = engine.layout().clone();
    let plan = engine
        .inspect(image.to_str().unwrap(), AddOpts::default())
        .unwrap();
    engine.install(plan).unwrap();

    let desktop = layout
        .applications
        .join("unpakt-unpakt-fixture-app.desktop");
    assert!(desktop.is_file());
    validate(&desktop);
    let text = std::fs::read_to_string(&desktop).unwrap();
    let exec = text
        .lines()
        .find_map(|l| l.strip_prefix("Exec="))
        .expect("Exec line");
    let program = PathBuf::from(first_exec_argv0(exec));
    assert!(
        program.starts_with(&root),
        "{} outside root",
        program.display()
    );
    assert!(program.exists(), "{} does not exist", program.display());
}

#[test]
fn second_add_same_id_conflicts() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = tool_tarball(src.path());
    let mut engine = engine(root.path());

    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    engine.install(plan).unwrap();
    let after_first = tree_snapshot(root.path());

    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    let err = engine.install(plan).unwrap_err();
    assert!(
        matches!(&err, Error::Conflict(m) if m.contains("unpakt update unpakt-fixture-tool")),
        "{err:?}"
    );
    assert_eq!(tree_snapshot(root.path()), after_first);
    assert_eq!(engine.list().unwrap().len(), 1);
}

#[test]
fn entry_override_selects_candidate() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = build_tarball(
        src.path(),
        "unpakt-fixture-multi-1.0.tar.gz",
        Comp::Gz,
        &[
            E::File("unpakt-fixture-multi/bin/unpakt-fixture-multi", ELF, 0o755),
            E::File("unpakt-fixture-multi/bin/unpakt-fixture-other", ELF, 0o755),
        ],
    );
    let mut engine = engine(root.path());

    let default_plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    let first = default_plan.plan.entry.clone().unwrap().path;
    let other = default_plan
        .plan
        .payload
        .entries
        .iter()
        .map(|e| e.path.clone())
        .find(|p| *p != first)
        .expect("a second candidate");

    let plan = engine
        .inspect(
            tarball.to_str().unwrap(),
            AddOpts {
                entry: Some(other.clone()),
                ..AddOpts::default()
            },
        )
        .unwrap();
    assert_eq!(plan.plan.entry.as_ref().unwrap().path, other);
    let stem = other.file_stem().unwrap().to_string_lossy().into_owned();
    assert_eq!(plan.command.as_deref(), Some(stem.as_str()));
    engine.install(plan).unwrap();
    let target = std::fs::read_link(engine.layout().bin.join(&stem)).unwrap();
    assert!(target.ends_with(&other), "{}", target.display());

    let err = engine
        .inspect(
            tarball.to_str().unwrap(),
            AddOpts {
                entry: Some(PathBuf::from("bin/does-not-exist")),
                ..AddOpts::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[test]
fn invalid_ids_are_refused() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = tool_tarball(src.path());
    let engine = engine(root.path());
    for id in ["", "Bad Id", "-x"] {
        let err = engine
            .inspect(
                tarball.to_str().unwrap(),
                AddOpts {
                    id: Some(id.into()),
                    ..AddOpts::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{id:?}: {err:?}");
    }
}

#[test]
fn missing_input_is_not_found() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(root.path());
    let err = engine
        .inspect("/definitely/not/here.AppImage", AddOpts::default())
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[test]
fn install_never_runs_a_package_posing_as_a_helper() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let sys = tempfile::tempdir().unwrap();
    let marker = src.path().join("marker");
    let script = common::marker_script(&marker);
    let tarball = build_tarball(
        src.path(),
        "posing-1.0.tar.gz",
        Comp::Gz,
        &[E::File("posing/bin/posing", &script, 0o755)],
    );
    let layout = Layout::under(root.path());
    let path_var = std::env::join_paths([sys.path(), layout.bin.as_path()]).unwrap();
    let mut engine = Engine::new(layout, Host::detect())
        .unwrap()
        .with_path_var(path_var);
    let plan = engine
        .inspect(
            tarball.to_str().unwrap(),
            AddOpts {
                command: Some("update-desktop-database".into()),
                ..AddOpts::default()
            },
        )
        .unwrap();
    engine.install(plan).unwrap();
    assert!(!marker.exists(), "the installed package was executed");
    let check = engine
        .doctor()
        .into_iter()
        .find(|c| c.name == "update-desktop-database")
        .unwrap();
    assert!(!check.ok, "{}", check.detail);
}

#[test]
fn install_rolls_back_when_db_write_fails() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = tool_tarball(src.path());
    let mut engine = engine(root.path());
    let baseline = tree_snapshot(root.path());

    let lock = rusqlite::Connection::open(&engine.layout().db).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert!(engine.install(plan).is_err());
    assert_eq!(tree_snapshot(root.path()), baseline);
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(engine.list().unwrap().is_empty());
}

#[test]
fn doctor_reports_problems() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = tool_tarball(src.path());
    let mut engine = engine(root.path());
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    engine.install(plan).unwrap();
    let id = "unpakt-fixture-tool";
    let name = format!("app:{id}");
    let find =
        |engine: &Engine, n: &str| engine.doctor().into_iter().find(|c| c.name == n).unwrap();

    assert!(find(&engine, "bin-in-path").ok);
    assert!(find(&engine, &name).ok);
    std::fs::remove_file(
        engine
            .layout()
            .applications
            .join(format!("unpakt-{id}.desktop")),
    )
    .unwrap();
    assert!(!find(&engine, &name).ok);

    let other = Layout::under(root.path());
    let engine = Engine::new(other, Host::detect())
        .unwrap()
        .with_path_var(OsString::from("/usr/bin"));
    assert!(!find(&engine, "bin-in-path").ok);
}

#[test]
fn hostile_command_is_refused_at_inspect() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = tool_tarball(src.path());
    let engine = engine(root.path());
    let before = tree_snapshot(root.path());
    for command in ["../../Documents", "/tmp/x", "a/b", "..", ""] {
        let opts = AddOpts {
            command: Some(command.into()),
            force: true,
            ..AddOpts::default()
        };
        let err = engine.inspect(tarball.to_str().unwrap(), opts).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{command:?}: {err:?}");
    }
    assert_eq!(tree_snapshot(root.path()), before);
}

fn bundled_desktop(exec: &str) -> Vec<u8> {
    format!("[Desktop Entry]\nType=Application\nName=Unpakt Fixture Real\nExec={exec}\n")
        .into_bytes()
}

#[test]
fn appimage_command_follows_the_bundled_desktop_exec() {
    let src = tempfile::tempdir().unwrap();
    let plan_for = |exec: &str, opts: AddOpts| {
        let root = tempfile::tempdir().unwrap();
        let image = build_appimage(
            src.path(),
            "Unpakt-Fixture-Real-1.0.AppImage",
            &[
                E::File("AppRun", ELF, 0o755),
                E::File("real.desktop", &bundled_desktop(exec), 0o644),
            ],
        );
        let plan = engine(root.path())
            .inspect(image.to_str().unwrap(), opts)
            .unwrap();
        (plan.plan.id.clone(), plan.command.clone())
    };
    let (id, command) = plan_for("unpakt-fixture-nvim %F", AddOpts::default());
    assert_eq!(id, "unpakt-fixture-real");
    assert_eq!(command.as_deref(), Some("unpakt-fixture-nvim"));
    assert_eq!(
        plan_for("/usr/bin/unpakt-fixture-nvim --x", AddOpts::default())
            .1
            .as_deref(),
        Some("unpakt-fixture-nvim")
    );
    assert_eq!(
        plan_for("\"unpakt fixture nvim\" %U", AddOpts::default())
            .1
            .as_deref(),
        Some("unpakt fixture nvim")
    );
    for useless in ["AppRun %U", "env FOO=1 tool", "..", "\"\""] {
        assert_eq!(
            plan_for(useless, AddOpts::default()).1.as_deref(),
            Some("unpakt-fixture-real"),
            "{useless}"
        );
    }
    let explicit = AddOpts {
        command: Some("chosen".into()),
        ..AddOpts::default()
    };
    assert_eq!(
        plan_for("unpakt-fixture-nvim", explicit).1.as_deref(),
        Some("chosen")
    );
}

#[test]
fn tarball_command_is_the_entry_file_name() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let tarball = build_tarball(
        src.path(),
        "unpakt-fixture-pkg-1.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-pkg/bin/unpakt-fixture-real.sh",
            b"#!/bin/sh\n",
            0o755,
        )],
    );
    let engine = engine(root.path());
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    assert_eq!(plan.plan.id, "unpakt-fixture-pkg");
    assert_eq!(plan.command.as_deref(), Some("unpakt-fixture-real.sh"));
    engine.discard(plan);
}

#[test]
fn remove_with_cancel_set_is_undone() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cancel = Cancel::new();
    let mut engine = engine(root.path()).with_cancel(cancel.clone());
    let plan = engine
        .inspect(
            tool_tarball(src.path()).to_str().unwrap(),
            AddOpts::default(),
        )
        .unwrap();
    let id = engine.install(plan).unwrap().id;
    let installed = tree_snapshot(root.path());

    cancel.cancel();
    assert!(matches!(engine.remove(&id), Err(Error::Interrupted)));
    assert_eq!(tree_snapshot(root.path()), installed);
    assert!(engine.get(&id).is_ok(), "the app is still recorded");
}
