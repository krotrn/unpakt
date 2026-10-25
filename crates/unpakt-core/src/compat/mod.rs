use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use crate::format::package::resolve_in_payload;
use crate::model::PackageMeta;

pub mod elf;
pub mod hostlibs;
pub mod scripts;

pub use hostlibs::HostLibs;

use elf::ElfFacts;

const SYSTEM_PREFIXES: [&str; 9] = [
    "usr/lib/systemd/system/",
    "lib/systemd/system/",
    "etc/systemd/system/",
    "usr/lib/udev/rules.d/",
    "lib/udev/rules.d/",
    "etc/udev/rules.d/",
    "lib/modules/",
    "usr/lib/modules/",
    "usr/share/polkit-1/actions/",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub tag: &'static str,
    pub detail: String,
    /// A note does not make the verdict risky.
    pub note: bool,
}

/// Whether an extracted package is expected to run on this host. In `Ok` the
/// findings are notes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ok(Vec<Finding>),
    Risky(Vec<Finding>),
    NeedsContainer(Vec<Finding>),
}

impl Verdict {
    pub fn tags(&self) -> BTreeSet<&'static str> {
        self.findings().iter().map(|f| f.tag).collect()
    }

    /// Tags of the findings that count against the verdict; notes are left out.
    pub fn risk_tags(&self) -> BTreeSet<&'static str> {
        self.findings()
            .iter()
            .filter(|f| !f.note)
            .map(|f| f.tag)
            .collect()
    }

    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Ok(_) => "ok",
            Verdict::Risky(_) => "risky",
            Verdict::NeedsContainer(_) => "needs-container",
        }
    }

    pub fn findings(&self) -> &[Finding] {
        match self {
            Verdict::Ok(f) | Verdict::Risky(f) | Verdict::NeedsContainer(f) => f,
        }
    }
}

struct Lookup {
    by_name: BTreeMap<String, Vec<PathBuf>>,
}

impl Lookup {
    fn new(meta: &PackageMeta) -> Lookup {
        let mut by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for p in meta.index.files.iter().chain(meta.index.links.keys()) {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                by_name.entry(name.to_string()).or_default().push(p.clone());
            }
        }
        Lookup { by_name }
    }

    /// Where `soname` resolves inside the payload for the ELF at `from`.
    fn resolve(&self, meta: &PackageMeta, from: &Path, soname: &str) -> Option<PathBuf> {
        let origin = from.parent().unwrap_or(Path::new(""));
        if let Some(facts) = meta.elf.get(from) {
            for entry in facts.runpath.iter().flat_map(|r| r.split(':')) {
                let dir = if entry.contains("ORIGIN") {
                    let replaced = entry
                        .replace("${ORIGIN}", &origin.to_string_lossy())
                        .replace("$ORIGIN", &origin.to_string_lossy());
                    PathBuf::from(replaced)
                } else {
                    PathBuf::from(entry.trim_start_matches('/'))
                };
                if let Some(found) = resolve_in_payload(&meta.index, &dir.join(soname)) {
                    return Some(found);
                }
            }
        }
        if let Some(found) = resolve_in_payload(&meta.index, &origin.join(soname)) {
            return Some(found);
        }
        self.by_name
            .get(soname)?
            .iter()
            .find_map(|p| resolve_in_payload(&meta.index, p))
    }
}

fn exec_target(script: &str) -> Option<PathBuf> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("exec ").or((l == "exec").then_some("")))
        .find_map(|rest| {
            rest.split_whitespace()
                .map(|t| t.trim_matches(|c| c == '"' || c == '\'' || c == ';'))
                .find(|t| t.starts_with('/'))
                .map(|t| PathBuf::from(t.trim_start_matches('/')))
        })
}

const SHARED_SHARE: [&str; 27] = [
    "applications",
    "icons",
    "pixmaps",
    "mime",
    "locale",
    "doc",
    "man",
    "info",
    "metainfo",
    "appdata",
    "licenses",
    "fonts",
    "themes",
    "sounds",
    "help",
    "dbus-1",
    "glib-2.0",
    "polkit-1",
    "bash-completion",
    "zsh",
    "fish",
    "gnome-shell",
    "X11",
    "menu",
    "lintian",
    "gtk-doc",
    "thumbnailers",
];

const SHARED_LIB: [&str; 17] = [
    "x86_64-linux-gnu",
    "aarch64-linux-gnu",
    "systemd",
    "udev",
    "sysusers.d",
    "tmpfiles.d",
    "modules",
    "girepository-1.0",
    "gtk-3.0",
    "gtk-4.0",
    "debug",
    "mime",
    "binfmt.d",
    "environment.d",
    "sysctl.d",
    "modules-load.d",
    "pkgconfig",
];

fn install_roots(meta: &PackageMeta) -> Vec<String> {
    meta.index
        .dirs
        .iter()
        .filter(|d| {
            let comps: Vec<Component> = d.components().collect();
            let first = comps.first().and_then(|c| c.as_os_str().to_str());
            match first {
                Some("opt") => comps.len() == 2,
                Some("usr") if comps.len() == 3 => {
                    let leaf = comps[2].as_os_str().to_str().unwrap_or("");
                    match comps[1].as_os_str().to_str() {
                        Some("share") => !SHARED_SHARE.contains(&leaf),
                        Some("lib") => !SHARED_LIB.contains(&leaf),
                        _ => false,
                    }
                }
                _ => false,
            }
        })
        .map(|d| format!("/{}", d.to_string_lossy()))
        .collect()
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn path_in_root(path: &str, root: &str) -> bool {
    path == root || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

fn text_mentions(text: &str, root: &str) -> Option<String> {
    for (at, _) in text.match_indices(root) {
        if text[..at].chars().next_back().is_some_and(is_name_char) {
            continue;
        }
        let tail = &text[at + root.len()..];
        match tail.chars().next() {
            None => return Some(root.to_string()),
            Some('/') => {
                let end = tail
                    .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ';' | ')' | '`'))
                    .unwrap_or(tail.len());
                return Some(format!("{root}{}", &tail[..end]));
            }
            Some(c) if !is_name_char(c) => return Some(root.to_string()),
            _ => {}
        }
    }
    None
}

/// Judges whether `meta`, extracted into the user's home with `entry` as its
/// launcher, is expected to run on this host. Reads nothing from disk.
pub fn check(meta: &PackageMeta, entry: &Path, libs: &HostLibs) -> Verdict {
    let lookup = Lookup::new(meta);

    let mut starts: Vec<PathBuf> = Vec::new();
    let entry_file = resolve_in_payload(&meta.index, entry).unwrap_or_else(|| entry.to_path_buf());
    if meta.elf.contains_key(&entry_file) {
        starts.push(entry_file);
    }
    if let Some(target) = meta.entry_script.as_deref().and_then(exec_target)
        && let Some(resolved) = resolve_in_payload(&meta.index, &target)
        && meta.elf.contains_key(&resolved)
        && !starts.contains(&resolved)
    {
        starts.push(resolved);
    }

    let mut closure: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for s in starts {
        if seen.insert(s.clone()) {
            queue.push_back(s);
        }
    }
    let mut missing: BTreeSet<String> = BTreeSet::new();
    let mut too_new: BTreeSet<String> = BTreeSet::new();
    while let Some(node) = queue.pop_front() {
        closure.push(node.clone());
        let facts: &ElfFacts = &meta.elf[&node];
        for soname in &facts.needed {
            match lookup.resolve(meta, &node, soname) {
                Some(found) => {
                    if meta.elf.contains_key(&found) && seen.insert(found.clone()) {
                        queue.push_back(found);
                    }
                }
                None => {
                    if !libs.has(soname) {
                        missing.insert(soname.clone());
                    }
                }
            }
        }
        for (soname, version) in &facts.version_needs {
            if lookup.resolve(meta, &node, soname).is_none()
                && libs.provides(soname, version) == Some(false)
            {
                too_new.insert(format!("{} needs {version} from {soname}", node.display()));
            }
        }
    }

    let mut findings: Vec<Finding> = Vec::new();
    let mut push = |tag: &'static str, detail: String, note: bool| {
        findings.push(Finding { tag, detail, note });
    };

    if !missing.is_empty() {
        let list: Vec<&str> = missing.iter().map(String::as_str).collect();
        push(
            "missing-library",
            format!("missing libraries: {}", list.join(", ")),
            false,
        );
    }
    let has_too_new = !too_new.is_empty();
    for detail in too_new {
        push("glibc-too-new", detail, false);
    }

    let roots = install_roots(meta);
    if !roots.is_empty() {
        for node in &closure {
            let hit = meta.elf[node]
                .abs_paths
                .iter()
                .find_map(|p| roots.iter().any(|r| path_in_root(p, r)).then(|| p.clone()));
            if let Some(p) = hit {
                push(
                    "hardcoded-path",
                    format!("{} refers to {p}", node.display()),
                    false,
                );
            }
        }
        if let Some(text) = meta.entry_script.as_deref()
            && let Some(p) = roots.iter().find_map(|r| text_mentions(text, r))
        {
            push(
                "hardcoded-path",
                format!("{} refers to {p}", entry.display()),
                false,
            );
        }
    }

    let system: Vec<String> = meta
        .index
        .files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| SYSTEM_PREFIXES.iter().any(|pre| p.starts_with(pre)))
        .collect();
    if !system.is_empty() {
        push(
            "system-integration",
            format!("installs system files: {}", system.join(", ")),
            false,
        );
    }

    if !meta.index.setuid.is_empty() {
        let files: Vec<String> = meta
            .index
            .setuid
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        if libs.userns() {
            push(
                "setuid",
                format!(
                    "setuid bit is dropped on {}; user namespaces are available instead",
                    files.join(", ")
                ),
                true,
            );
        } else {
            push(
                "setuid",
                format!(
                    "setuid bit is dropped on {} and user namespaces are unavailable",
                    files.join(", ")
                ),
                false,
            );
        }
    }

    for script in &meta.scripts {
        let name = &script.name;
        for issue in scripts::assess(&script.text, &meta.index) {
            match issue {
                scripts::ScriptIssue::System(line) => {
                    push("maintainer-script", format!("{name}: {line}"), false);
                }
                scripts::ScriptIssue::Setuid(line) => push(
                    "setuid",
                    format!("{name} would set the setuid bit ({line}); it is not set"),
                    libs.userns(),
                ),
                scripts::ScriptIssue::Other(line) => push(
                    "maintainer-script",
                    format!("{name} was not run; first unrecognized line: {line}"),
                    true,
                ),
            }
        }
    }

    if has_too_new {
        Verdict::NeedsContainer(findings)
    } else if findings.iter().any(|f| !f.note) {
        Verdict::Risky(findings)
    } else {
        Verdict::Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::stream::DataSource;
    use crate::model::{PackageFamily, PayloadIndex, Script};
    use std::collections::{BTreeMap, BTreeSet};

    const POSTINST: &str = "#!/bin/sh\nset -e\ncase \"$1\" in\n  configure)\n    update-desktop-database -q /usr/share/applications || true\n    update-mime-database /usr/share/mime || true\n  ;;\nesac\nexit 0\n";

    fn facts(needed: &[&str], runpath: &[&str]) -> ElfFacts {
        ElfFacts {
            needed: needed.iter().map(|s| s.to_string()).collect(),
            runpath: runpath.iter().map(|s| s.to_string()).collect(),
            ..ElfFacts::default()
        }
    }

    fn meta(files: &[&str], elf: Vec<(&str, ElfFacts)>) -> PackageMeta {
        let mut index = PayloadIndex::default();
        for f in files {
            let p = PathBuf::from(f);
            for a in p.ancestors().skip(1) {
                if !a.as_os_str().is_empty() {
                    index.dirs.insert(a.to_path_buf());
                }
            }
            index.files.insert(p);
        }
        PackageMeta {
            family: PackageFamily::Pacman,
            name: "foo".into(),
            version: "1".into(),
            arch: "x86_64".into(),
            depends: vec![],
            scripts: vec![],
            index,
            elf: elf
                .into_iter()
                .map(|(k, v)| (PathBuf::from(k), v))
                .collect(),
            entry_script: None,
            data: DataSource::Tar {
                path: PathBuf::new(),
                skip: vec![],
            },
        }
    }

    fn host(sonames: &[&str], versions: &[(&str, &[&str])], userns: bool) -> HostLibs {
        HostLibs::from_parts(
            sonames
                .iter()
                .map(|s| (s.to_string(), PathBuf::from("/x")))
                .collect(),
            versions
                .iter()
                .map(|(s, v)| {
                    (
                        s.to_string(),
                        v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>(),
            userns,
        )
    }

    const FOO: &str = "opt/Foo/foo";

    fn simple(needed: &[&str]) -> PackageMeta {
        meta(&[FOO], vec![(FOO, facts(needed, &[]))])
    }

    #[test]
    fn electron_shaped_package_is_ok_with_setuid_note() {
        let mut ef = facts(
            &["libffmpeg.so", "libgtk-3.so.0", "libc.so.6"],
            &["$ORIGIN"],
        );
        ef.version_needs = vec![("libc.so.6".into(), "GLIBC_2.17".into())];
        let mut m = meta(
            &[FOO, "opt/Foo/libffmpeg.so", "opt/Foo/chrome-sandbox"],
            vec![
                (FOO, ef),
                ("opt/Foo/libffmpeg.so", facts(&["libc.so.6"], &[])),
            ],
        );
        m.index.setuid.push("opt/Foo/chrome-sandbox".into());
        m.scripts.push(Script {
            name: "postinst".into(),
            text: POSTINST.into(),
        });
        let libs = host(
            &["libgtk-3.so.0", "libc.so.6"],
            &[("libc.so.6", &["GLIBC_2.17", "GLIBC_2.34"])],
            true,
        );
        match check(&m, Path::new(FOO), &libs) {
            Verdict::Ok(v) => assert!(v.len() == 1 && v[0].tag == "setuid", "{v:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn setuid_without_userns_is_risky() {
        let mut m = simple(&[]);
        m.index.setuid.push("opt/Foo/chrome-sandbox".into());
        let v = check(&m, Path::new(FOO), &host(&[], &[], false));
        assert_eq!(v.label(), "risky");
        assert!(v.tags().contains("setuid"));
    }

    #[test]
    fn missing_library_is_risky_and_named() {
        let m = simple(&["libfoo.so.1", "libbar.so.2"]);
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Risky(_)));
        assert_eq!(
            v.findings()[0].detail,
            "missing libraries: libbar.so.2, libfoo.so.1"
        );
    }

    #[test]
    fn transitive_missing_library_found() {
        let m = meta(
            &[FOO, "opt/Foo/libx.so"],
            vec![
                (FOO, facts(&["libx.so"], &["$ORIGIN"])),
                ("opt/Foo/libx.so", facts(&["libz.so.9"], &[])),
            ],
        );
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Risky(_)));
        assert!(v.findings()[0].detail.contains("libz.so.9"));
    }

    fn versioned(in_payload: bool) -> Verdict {
        let mut ef = facts(&["libc.so.6"], &[]);
        ef.version_needs = vec![("libc.so.6".into(), "GLIBC_9.99".into())];
        let files: Vec<&str> = if in_payload {
            vec![FOO, "opt/Foo/libc.so.6"]
        } else {
            vec![FOO]
        };
        let m = meta(&files, vec![(FOO, ef)]);
        let libs = host(
            &["libc.so.6"],
            &[("libc.so.6", &["GLIBC_2.17", "GLIBC_2.34"])],
            true,
        );
        check(&m, Path::new(FOO), &libs)
    }

    #[test]
    fn glibc_too_new_needs_container() {
        let v = versioned(false);
        assert!(matches!(v, Verdict::NeedsContainer(_)));
        assert_eq!(v.label(), "needs-container");
        assert!(v.tags().contains("glibc-too-new"));
    }

    #[test]
    fn bundled_lib_versions_not_checked() {
        assert!(!matches!(versioned(true), Verdict::NeedsContainer(_)));
    }

    #[test]
    fn hardcoded_path_in_entry_is_risky() {
        let mut ef = facts(&[], &[]);
        ef.abs_paths.insert("/opt/Foo/resources".into());
        let m = meta(&[FOO], vec![(FOO, ef)]);
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Risky(_)));
        assert_eq!(v.findings()[0].tag, "hardcoded-path");
    }

    #[test]
    fn hardcoded_path_in_entry_script_is_risky() {
        let mut m = meta(&["usr/bin/code", "usr/share/code/code"], vec![]);
        m.entry_script =
            Some("#!/bin/sh\nVSCODE_PATH=\"/usr/share/code\"\nexec \"$VSCODE_PATH/code\"\n".into());
        let v = check(&m, Path::new("usr/bin/code"), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Risky(_)));
        assert_eq!(v.findings()[0].tag, "hardcoded-path");
    }

    #[test]
    fn entry_script_exec_target_joins_the_closure() {
        let mut m = meta(
            &["usr/bin/foo", FOO],
            vec![(FOO, facts(&["libnope.so.1"], &[]))],
        );
        m.entry_script = Some("#!/bin/sh\nexec /opt/Foo/foo \"$@\"\n".into());
        let v = check(&m, Path::new("usr/bin/foo"), &host(&[], &[], true));
        assert!(v.findings()[0].detail.contains("libnope.so.1"));
    }

    #[test]
    fn hardcoded_path_only_in_desktop_is_ok() {
        let m = simple(&[]);
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert_eq!(v, Verdict::Ok(vec![]));
    }

    #[test]
    fn path_outside_install_roots_is_ok() {
        let mut ef = facts(&[], &[]);
        ef.abs_paths.insert("/usr/share/fonts/x".into());
        let m = meta(&[FOO], vec![(FOO, ef)]);
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Ok(_)));
    }

    #[test]
    fn systemd_unit_is_risky() {
        let m = meta(
            &[FOO, "usr/lib/systemd/system/foo.service"],
            vec![(FOO, facts(&[], &[]))],
        );
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Risky(_)));
        assert!(v.tags().contains("system-integration"));
    }

    #[test]
    fn unrecognized_script_line_is_risky() {
        let mut m = simple(&[]);
        m.scripts.push(Script {
            name: "postinst".into(),
            text: "useradd foo\n".into(),
        });
        let v = check(&m, Path::new(FOO), &host(&[], &[], true));
        assert!(matches!(v, Verdict::Risky(_)));
        assert_eq!(v.findings()[0].detail, "postinst: useradd foo");
    }

    const OBSIDIAN: &str = include_str!("obsidian_postinst.txt");

    fn electron(userns: bool, script: &str) -> Verdict {
        let mut m = meta(
            &[FOO, "opt/Foo/chrome-sandbox"],
            vec![(FOO, facts(&[], &[]))],
        );
        m.index.setuid.push("opt/Foo/chrome-sandbox".into());
        m.scripts.push(Script {
            name: "postinst".into(),
            text: script.into(),
        });
        check(&m, Path::new(FOO), &host(&[], &[], userns))
    }

    fn with_script(script: &str) -> Verdict {
        let mut m = simple(&[]);
        m.scripts.push(Script {
            name: "postinst".into(),
            text: script.into(),
        });
        check(&m, Path::new(FOO), &host(&[], &[], true))
    }

    #[test]
    fn real_electron_postinst_is_ok_with_notes_when_userns() {
        match electron(true, OBSIDIAN) {
            Verdict::Ok(v) => {
                assert!(v.iter().all(|f| f.note), "{v:?}");
                assert!(
                    v.iter()
                        .any(|f| f.tag == "setuid" && f.detail.starts_with("postinst"))
                );
                assert!(v.iter().any(|f| f.tag == "maintainer-script"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn real_electron_postinst_is_risky_without_userns() {
        let v = electron(false, OBSIDIAN);
        assert!(matches!(v, Verdict::Risky(_)));
        assert!(v.findings().iter().any(|f| f.tag == "setuid" && !f.note));
    }

    #[test]
    fn compound_line_hiding_useradd_is_risky() {
        assert!(matches!(
            with_script("ldconfig; useradd foo"),
            Verdict::Risky(_)
        ));
        assert!(matches!(
            with_script("#!/bin/sh\nuseradd foo\n"),
            Verdict::Risky(_)
        ));
    }

    #[test]
    fn plain_unrecognized_scripts_only_add_notes() {
        for text in [
            "echo deb http://x > /etc/apt/sources.list.d/x.list",
            "systemctl reload apparmor\npkill -f Discord\nrm -rf \"$HOME/.config/discord/Cache\"\n",
        ] {
            match with_script(text) {
                Verdict::Ok(v) => {
                    assert!(v.iter().all(|f| f.note && f.tag == "maintainer-script"));
                    assert_eq!(v.len(), 1);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn shared_dirs_are_not_install_roots() {
        let mut ef = facts(&[], &[]);
        ef.abs_paths.insert("/usr/share/locale".into());
        let mut m = meta(&["usr/bin/pavucontrol", "usr/share/locale/de/x.mo"], vec![]);
        m.elf.insert("usr/bin/pavucontrol".into(), ef);
        let libs = host(&[], &[], true);
        assert!(matches!(
            check(&m, Path::new("usr/bin/pavucontrol"), &libs),
            Verdict::Ok(_)
        ));

        let mut ef = facts(&[], &[]);
        ef.abs_paths.insert("/usr/share/pavucontrol".into());
        let mut m = meta(
            &["usr/bin/pavucontrol", "usr/share/pavucontrol/x.glade"],
            vec![],
        );
        m.elf.insert("usr/bin/pavucontrol".into(), ef);
        let v = check(&m, Path::new("usr/bin/pavucontrol"), &libs);
        assert!(matches!(v, Verdict::Risky(_)));
        assert_eq!(v.findings()[0].tag, "hardcoded-path");
    }

    #[test]
    fn entry_symlink_to_the_binary_is_followed() {
        let mut m = simple(&["libnope.so.1"]);
        m.index
            .links
            .insert("usr/bin/foo".into(), "/opt/Foo/foo".into());
        let v = check(&m, Path::new("usr/bin/foo"), &host(&[], &[], true));
        assert!(v.findings()[0].detail.contains("libnope.so.1"));
    }

    #[test]
    fn exec_target_ignores_comments_and_mid_line_exec() {
        assert_eq!(exec_target("# exec /opt/a/b\necho exec /opt/c/d\n"), None);
        assert_eq!(
            exec_target("#!/bin/sh\nexec /opt/Foo/foo --x /opt/other\n"),
            Some(PathBuf::from("opt/Foo/foo"))
        );
    }

    #[test]
    fn verdict_tags_and_label() {
        let f = |tag| Finding {
            tag,
            detail: String::new(),
            note: false,
        };
        let v = Verdict::Risky(vec![f("setuid"), f("hardcoded-path"), f("setuid")]);
        assert_eq!(v.label(), "risky");
        assert_eq!(v.findings().len(), 3);
        assert_eq!(
            v.tags().into_iter().collect::<Vec<_>>(),
            vec!["hardcoded-path", "setuid"]
        );
        assert_eq!(Verdict::Ok(vec![]).label(), "ok");
    }
}
