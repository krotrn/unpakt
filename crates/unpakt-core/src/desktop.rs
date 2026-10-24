//! Desktop entry files: lossless parsing, editing, rendering and `Exec` quoting.

use std::path::Path;

use crate::{Error, Result};

const MAIN_GROUP: &str = "Desktop Entry";

/// One physical line of the file, terminator included when present.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// Comment, blank or otherwise unrecognised line, kept verbatim.
    Other(String),
    /// `key=value`; `raw` is the exact original text so untouched lines render unchanged.
    Pair {
        key: String,
        value: String,
        raw: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    /// Header line as written; `None` for lines before the first group.
    header: Option<String>,
    name: String,
    lines: Vec<Line>,
}

/// A parsed desktop entry that renders back byte-for-byte when unmodified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    groups: Vec<Group>,
}

fn group_name(line: &str) -> Option<&str> {
    let t = line.trim_end_matches(['\r', '\n']);
    t.strip_prefix('[')?.strip_suffix(']')
}

fn parse_pair(line: &str) -> Option<(String, String)> {
    let t = line.trim_end_matches(['\r', '\n']);
    if t.trim_start().starts_with('#') {
        return None;
    }
    let (key, value) = t.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), value.trim_start().to_string()))
}

impl DesktopEntry {
    /// Parses `text`; fails when it has no `[Desktop Entry]` group.
    pub fn parse(text: &str) -> Result<DesktopEntry> {
        let mut groups = vec![Group {
            header: None,
            name: String::new(),
            lines: Vec::new(),
        }];
        for raw in text.split_inclusive('\n') {
            if let Some(name) = group_name(raw) {
                groups.push(Group {
                    header: Some(raw.to_string()),
                    name: name.to_string(),
                    lines: Vec::new(),
                });
                continue;
            }
            let line = match parse_pair(raw) {
                Some((key, value)) => Line::Pair {
                    key,
                    value,
                    raw: raw.to_string(),
                },
                None => Line::Other(raw.to_string()),
            };
            groups
                .last_mut()
                .expect("preamble group exists")
                .lines
                .push(line);
        }
        if !groups
            .iter()
            .any(|g| g.header.is_some() && g.name == MAIN_GROUP)
        {
            return Err(Error::Parse("missing [Desktop Entry] group".into()));
        }
        Ok(DesktopEntry { groups })
    }

    /// A minimal application entry.
    pub fn new_application(name: &str) -> DesktopEntry {
        let mut entry = DesktopEntry {
            groups: vec![Group {
                header: Some(format!("[{MAIN_GROUP}]\n")),
                name: MAIN_GROUP.to_string(),
                lines: Vec::new(),
            }],
        };
        entry.set("Type", "Application");
        entry.set("Name", &escape_string(name));
        entry.set("Terminal", "false");
        entry.set("Categories", "Utility;");
        entry
    }

    fn main_group(&self) -> &Group {
        self.groups
            .iter()
            .find(|g| g.header.is_some() && g.name == MAIN_GROUP)
            .expect("main group checked at construction")
    }

    fn main_group_mut(&mut self) -> &mut Group {
        self.groups
            .iter_mut()
            .find(|g| g.header.is_some() && g.name == MAIN_GROUP)
            .expect("main group checked at construction")
    }

    /// Value of `key` in the main group (exact key, no locale matching).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.main_group().lines.iter().find_map(|l| match l {
            Line::Pair { key: k, value, .. } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Sets `key` in the main group; `value` is the text as written in the file.
    /// A missing key is appended at the end of the group.
    pub fn set(&mut self, key: &str, value: &str) {
        set_in(self.main_group_mut(), key, value);
    }

    /// Rewrites the `Exec` of every `[Desktop Action …]` group with `f`, which
    /// gets the value as written in the file. An action whose `Exec` is
    /// missing or for which `f` returns `None` is dropped, including its name
    /// in the main group's `Actions` list.
    pub fn rewrite_action_execs(&mut self, f: impl Fn(&str) -> Option<String>) {
        let mut dropped = Vec::new();
        for group in &mut self.groups {
            let Some(action) = group
                .header
                .is_some()
                .then(|| group.name.strip_prefix(ACTION_PREFIX))
                .flatten()
                .map(str::to_string)
            else {
                continue;
            };
            let current = group.lines.iter().find_map(|l| match l {
                Line::Pair { key, value, .. } if key == "Exec" => Some(value.as_str()),
                _ => None,
            });
            match current.and_then(&f) {
                Some(exec) => set_in(group, "Exec", &exec),
                None => dropped.push(action),
            }
        }
        if dropped.is_empty() {
            return;
        }
        self.groups.retain(|g| {
            g.header.is_none()
                || g.name
                    .strip_prefix(ACTION_PREFIX)
                    .is_none_or(|a| !dropped.iter().any(|d| d == a))
        });
        if let Some(list) = self.get("Actions").map(str::to_string) {
            let kept: Vec<&str> = list
                .split(';')
                .filter(|a| !a.is_empty() && !dropped.iter().any(|d| d == a))
                .collect();
            if kept.is_empty() {
                self.remove("Actions");
            } else {
                self.set("Actions", &format!("{};", kept.join(";")));
            }
        }
    }

    /// Removes every `key` line from the main group.
    pub fn remove(&mut self, key: &str) {
        self.main_group_mut()
            .lines
            .retain(|l| !matches!(l, Line::Pair { key: k, .. } if k == key));
    }

    /// The file text.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for group in &self.groups {
            if let Some(h) = &group.header {
                out.push_str(h);
            }
            for line in &group.lines {
                match line {
                    Line::Other(s) => out.push_str(s),
                    Line::Pair { raw, .. } => out.push_str(raw),
                }
            }
        }
        out
    }
}

const ACTION_PREFIX: &str = "Desktop Action ";

fn set_in(group: &mut Group, key: &str, value: &str) {
    // A raw line break would split the line and let a value inject extra keys.
    let value = value.replace(['\r', '\n'], " ");
    let value = value.as_str();
    let pair = |newline: &str| Line::Pair {
        key: key.to_string(),
        value: value.to_string(),
        raw: format!("{key}={value}{newline}"),
    };
    let existing = group
        .lines
        .iter()
        .position(|l| matches!(l, Line::Pair { key: k, .. } if k == key));
    if let Some(i) = existing {
        let newline = match &group.lines[i] {
            Line::Pair { raw, .. } if raw.ends_with("\r\n") => "\r\n",
            Line::Pair { raw, .. } if raw.ends_with('\n') => "\n",
            _ => "",
        };
        group.lines[i] = pair(newline);
        return;
    }
    // Insert after the last key line so trailing blanks and comments stay in place.
    let at = group
        .lines
        .iter()
        .rposition(|l| matches!(l, Line::Pair { .. }))
        .map_or(0, |i| i + 1);
    if at > 0 {
        let prev = match &mut group.lines[at - 1] {
            Line::Other(s) => s,
            Line::Pair { raw, .. } => raw,
        };
        if !prev.ends_with('\n') {
            prev.push('\n');
        }
    }
    group.lines.insert(at, pair("\n"));
}

/// The Desktop Entry string escape for a value.
fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

const QUOTE_TRIGGERS: &str = " \t\n\"'\\><~|&;$*?#()`";

/// Quotes one `Exec` argument, then applies the file-level string escapes.
fn exec_arg(arg: &str) -> String {
    let mut quoted = String::new();
    if arg.contains(|c| QUOTE_TRIGGERS.contains(c)) {
        quoted.push('"');
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
    } else {
        quoted.push_str(arg);
    }
    let mut out = String::new();
    for c in quoted.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out
}

/// The `Exec` value (as written in the file) that launches `program` followed by `field_codes`.
pub fn exec_line(program: &Path, field_codes: &[String]) -> String {
    let mut line = exec_arg(&program.to_string_lossy());
    for code in field_codes {
        line.push(' ');
        line.push_str(code);
    }
    line
}

/// `exec` (as written in the file) with its program replaced by `program`.
/// Arguments and field codes after it stay as they were. `None` when the
/// value has no program or an unterminated quote.
pub fn replace_argv0(exec: &str, program: &Path) -> Option<String> {
    // Undo the file-level escapes, remembering where each char ended in the
    // raw text, so the untouched remainder can be copied verbatim.
    let mut chars: Vec<(char, usize)> = Vec::new();
    let mut it = exec.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c != '\\' {
            chars.push((c, i + c.len_utf8()));
            continue;
        }
        match it.next() {
            Some((j, n)) => {
                let end = j + n.len_utf8();
                let real = match n {
                    's' => ' ',
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                };
                chars.push((real, end));
            }
            None => chars.push(('\\', exec.len())),
        }
    }

    let mut idx = 0;
    while chars.get(idx).is_some_and(|(c, _)| c.is_whitespace()) {
        idx += 1;
    }
    let start = idx;
    let mut in_quotes = false;
    while let Some(&(c, _)) = chars.get(idx) {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' if in_quotes => idx += 1,
            c if c.is_whitespace() && !in_quotes => break,
            _ => {}
        }
        idx += 1;
    }
    if in_quotes || idx == start {
        return None;
    }
    let rest_from = chars[idx - 1].1;
    let mut line = exec_arg(&program.to_string_lossy());
    line.push_str(&exec[rest_from..]);
    Some(line)
}

/// The file-argument field codes (`%f %F %u %U`) of `exec`, in order.
pub fn field_codes(exec: &str) -> Vec<String> {
    exec.split_whitespace()
        .filter(|t| matches!(*t, "%f" | "%F" | "%u" | "%U"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const SAMPLE: &str = "# leading comment\n[Desktop Entry]\nType=Application\nName=Foo\nName[de]=Fuu\n\
Exec=foo %U\nDBusActivatable=true\n\n# actions\n[Desktop Action new]\nName=New\nExec=foo --new\n";

    #[test]
    fn exec_quoting() {
        assert_eq!(
            exec_line(Path::new("/opt/app/bin/app"), &[]),
            "/opt/app/bin/app"
        );
        assert_eq!(
            exec_line(Path::new("/home/u/My Apps/app"), &["%U".into()]),
            r#""/home/u/My Apps/app" %U"#
        );
        assert_eq!(
            exec_line(Path::new("/home/u/$x/app"), &[]),
            r#""/home/u/\\$x/app""#
        );
        assert_eq!(
            exec_line(Path::new("/home/u/100%/app"), &[]),
            "/home/u/100%%/app"
        );
    }

    #[test]
    fn argv0_replacement_keeps_arguments() {
        let p = Path::new("/new/app");
        assert_eq!(replace_argv0("AppRun", p).unwrap(), "/new/app");
        assert_eq!(
            replace_argv0("AppRun --a %U", p).unwrap(),
            "/new/app --a %U"
        );
        assert_eq!(
            replace_argv0(r#""/old dir/app" x "y z""#, p).unwrap(),
            r#"/new/app x "y z""#
        );
        // `\\"` in the file is an escaped quote inside a quoted program name.
        assert_eq!(
            replace_argv0(r#""a\\"b" rest"#, p).unwrap(),
            "/new/app rest"
        );
        assert_eq!(replace_argv0("\"a\\sb\" arg", p).unwrap(), "/new/app arg");
        assert_eq!(replace_argv0("", p), None);
        assert_eq!(replace_argv0("   ", p), None);
        assert_eq!(replace_argv0("\"open", p), None);
    }

    #[test]
    fn action_execs_are_rewritten_or_dropped() {
        let mut entry = DesktopEntry::parse(
            "[Desktop Entry]\nName=A\nActions=one;two;\n[Desktop Action one]\nExec=x --1\n[Desktop Action two]\nName=No exec\n",
        )
        .unwrap();
        entry.rewrite_action_execs(|v| replace_argv0(v, Path::new("/n")));
        assert_eq!(
            entry.render(),
            "[Desktop Entry]\nName=A\nActions=one;\n[Desktop Action one]\nExec=/n --1\n"
        );
        entry.rewrite_action_execs(|_| None);
        assert_eq!(entry.render(), "[Desktop Entry]\nName=A\n");
    }

    #[test]
    fn field_codes_kept() {
        assert_eq!(field_codes("foo --bar %U %i"), vec!["%U"]);
    }

    #[test]
    fn round_trip_preserves_other_groups_and_locales() {
        let entry = DesktopEntry::parse(SAMPLE).unwrap();
        assert_eq!(entry.render(), SAMPLE);
        assert_eq!(entry.get("Name"), Some("Foo"));
        assert_eq!(entry.get("Name[de]"), Some("Fuu"));
        assert_eq!(entry.get("Exec"), Some("foo %U"));
    }

    #[test]
    fn set_and_remove() {
        let mut entry = DesktopEntry::parse(SAMPLE).unwrap();
        entry.set("Exec", "/new/path %U");
        entry.remove("DBusActivatable");
        let out = entry.render();
        assert!(!out.contains("Exec=foo %U\n"));
        assert!(!out.contains("DBusActivatable"));
        assert!(out.contains("Exec=/new/path %U\n"));
        assert!(out.contains("Name[de]=Fuu\n"));
        assert!(out.contains("[Desktop Action new]\nName=New\nExec=foo --new\n"));
        assert_eq!(entry.get("Exec"), Some("/new/path %U"));
    }

    #[test]
    fn set_missing_key_appends_to_main_group() {
        let mut entry = DesktopEntry::parse(SAMPLE).unwrap();
        entry.set("X-Unpakt-Id", "foo");
        let out = entry.render();
        assert!(out.contains("DBusActivatable=true\nX-Unpakt-Id=foo\n\n# actions\n"));
        assert_eq!(entry.get("X-Unpakt-Id"), Some("foo"));
    }

    #[test]
    fn set_appends_when_file_lacks_trailing_newline() {
        let mut entry = DesktopEntry::parse("[Desktop Entry]\nName=Foo").unwrap();
        entry.set("Type", "Application");
        assert_eq!(
            entry.render(),
            "[Desktop Entry]\nName=Foo\nType=Application\n"
        );
    }

    #[test]
    fn accessors_ignore_other_groups() {
        let mut entry = DesktopEntry::parse(SAMPLE).unwrap();
        entry.remove("Name");
        assert_eq!(entry.get("Name"), None);
        assert!(entry.render().contains("[Desktop Action new]\nName=New\n"));
    }

    #[test]
    fn missing_main_group_is_a_parse_error() {
        let err = DesktopEntry::parse("[Other]\nName=x\n").unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn new_application_has_defaults() {
        let entry = DesktopEntry::new_application("Foo");
        assert_eq!(
            entry.render(),
            "[Desktop Entry]\nType=Application\nName=Foo\nTerminal=false\nCategories=Utility;\n"
        );
    }

    #[test]
    fn new_application_name_cannot_inject_keys() {
        let entry = DesktopEntry::new_application("a\nExec=evil");
        let out = entry.render();
        let names: Vec<_> = out.lines().filter(|l| l.starts_with("Name=")).collect();
        assert_eq!(names, vec![r"Name=a\nExec=evil"]);
        assert_eq!(entry.get("Exec"), None);
        assert!(!out.lines().any(|l| l.starts_with("Exec=")));
    }

    #[test]
    fn set_never_writes_raw_line_breaks() {
        let mut entry = DesktopEntry::new_application("Foo");
        entry.set("Comment", "x\ny");
        entry.set("Other", "p\r\nq");
        let out = entry.render();
        assert!(out.contains("Comment=x y\n"));
        assert!(out.contains("Other=p  q\n"));
        assert!(!out.contains('\r'));
    }

    #[test]
    fn awkward_exec_passes_desktop_file_validate() {
        if Command::new("desktop-file-validate")
            .arg("--help")
            .output()
            .is_err()
        {
            eprintln!("desktop-file-validate not installed; skipping");
            return;
        }
        let mut entry = DesktopEntry::new_application("Odd");
        let exec = exec_line(Path::new("/home/u/My Apps/$x/100%/app"), &["%U".into()]);
        entry.set("Exec", &exec);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("odd.desktop");
        std::fs::write(&file, entry.render()).unwrap();
        let out = Command::new("desktop-file-validate")
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
