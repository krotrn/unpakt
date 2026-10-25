use std::path::Path;

use crate::format::package::resolve_in_payload;
use crate::model::PayloadIndex;

const TRAILING: [&str; 4] = [
    " >/dev/null 2>&1",
    " || true",
    " >/dev/null",
    " 2>/dev/null",
];

const FIXED: [&str; 14] = [
    "set -e",
    "set -eu",
    "set -o errexit",
    "exit 0",
    "esac",
    ";;",
    "fi",
    "then",
    "else",
    "}",
    "case \"$1\" in",
    "case $1 in",
    "systemctl daemon-reload",
    "",
];

const FUNCTIONS: [&str; 6] = [
    "post_install() {",
    "post_upgrade() {",
    "pre_remove() {",
    "post_remove() {",
    "pre_install() {",
    "pre_upgrade() {",
];

const COMMANDS: [&str; 9] = [
    "update-desktop-database",
    "gtk-update-icon-cache",
    "xdg-icon-resource",
    "xdg-desktop-menu",
    "update-mime-database",
    "ldconfig",
    "glib-compile-schemas",
    "true",
    ":",
];

/// The first line of `text` that is not on the allowlist, trimmed. Scripts are
/// only ever read, never run.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn unrecognized_line(text: &str, index: &PayloadIndex) -> Option<String> {
    unrecognized_lines(text, index).into_iter().next()
}

fn unrecognized_lines(text: &str, index: &PayloadIndex) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line_ok(line, index))
        .map(str::to_string)
        .collect()
}

/// What a script's unrecognized lines amount to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScriptIssue {
    /// Creates accounts, capabilities, kernel settings or modules.
    System(String),
    /// Tries to set a setuid or setgid bit.
    Setuid(String),
    /// Anything else that is not on the allowlist (first such line).
    Other(String),
}

const SYSTEM_COMMANDS: [&str; 11] = [
    "useradd",
    "adduser",
    "groupadd",
    "addgroup",
    "usermod",
    "systemd-sysusers",
    "setcap",
    "sysctl",
    "modprobe",
    "insmod",
    "dkms",
];

const LEADING: [&str; 9] = [
    "if", "then", "else", "!", "elif", "while", "do", "exec", "sudo",
];

pub(crate) fn assess(text: &str, index: &PayloadIndex) -> Option<ScriptIssue> {
    let lines = unrecognized_lines(text, index);
    let first = lines.first()?.clone();
    if let Some(line) = lines.iter().find(|l| {
        simple_commands(l)
            .iter()
            .any(|c| SYSTEM_COMMANDS.contains(&c.as_str()))
    }) {
        return Some(ScriptIssue::System(line.clone()));
    }
    if let Some(line) = lines.iter().find(|l| sets_setuid(l)) {
        return Some(ScriptIssue::Setuid(line.clone()));
    }
    Some(ScriptIssue::Other(first))
}

/// First word of each simple command on the line.
fn simple_commands(line: &str) -> Vec<String> {
    let spaced: String = line
        .chars()
        .map(|c| {
            if matches!(c, ';' | '&' | '|' | '`' | '{' | '}' | '(' | ')') {
                '\n'
            } else {
                c
            }
        })
        .collect();
    spaced
        .lines()
        .filter_map(|segment| {
            let word = segment.split_whitespace().find(|w| {
                !LEADING.contains(w)
                    && !(w.contains('=') && !w.starts_with('=') && !w.contains('/'))
            })?;
            let word = word.trim_matches(|c| c == '\'' || c == '"');
            Some(word.rsplit('/').next().unwrap_or(word).to_string())
        })
        .collect()
}

fn sets_setuid(line: &str) -> bool {
    simple_chunks(line).any(|chunk| {
        let mut words = chunk
            .split_whitespace()
            .skip_while(|w| LEADING.contains(w) || is_assignment(w));
        if words.next().map(|w| w.rsplit('/').next().unwrap_or(w)) != Some("chmod") {
            return false;
        }
        words
            .filter(|w| !w.starts_with('-') || w.contains("+s"))
            .any(|w| {
                let w = w.trim_matches(|c| c == '\'' || c == '"');
                w.contains("+s")
                    || (w.len() == 4
                        && w.chars().all(|c| c.is_ascii_digit())
                        && matches!(w.as_bytes()[0], b'2' | b'4' | b'6'))
            })
    })
}

fn is_assignment(w: &str) -> bool {
    w.contains('=') && !w.starts_with('=') && !w.contains('/')
}

fn simple_chunks(line: &str) -> impl Iterator<Item = &str> {
    line.split([';', '&', '|', '`', '{', '}', '(', ')'])
}

fn strip_trailing(line: &str) -> &str {
    let mut s = line.trim();
    loop {
        let before = s;
        for suffix in TRAILING {
            if let Some(rest) = s.strip_suffix(suffix) {
                s = rest.trim_end();
            }
        }
        if s == before {
            return s;
        }
    }
}

fn line_ok(line: &str, index: &PayloadIndex) -> bool {
    let s = strip_trailing(line);
    if s.starts_with('#') || FIXED.contains(&s) || FUNCTIONS.contains(&s) {
        return true;
    }
    if let Some(pattern) = s.strip_suffix(')')
        && is_pattern(pattern)
    {
        return true;
    }
    if is_if_line(s) {
        return true;
    }
    if let Some((pattern, rest)) = s.split_once(')')
        && is_pattern(pattern)
        && let Some(cmd) = rest.trim().strip_suffix(";;")
    {
        return command_ok(strip_trailing(cmd), index);
    }
    command_ok(s, index)
}

fn is_pattern(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '|' | '*' | '-'))
}

fn is_if_line(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("if [ \"") else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix("$1\" = ")
        .or_else(|| rest.strip_prefix("$2\" = "))
    else {
        return false;
    };
    let Some(word) = rest.strip_suffix(" ]; then") else {
        return false;
    };
    let word = word
        .strip_prefix('"')
        .and_then(|w| w.strip_suffix('"'))
        .unwrap_or(word);
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn command_ok(cmd: &str, index: &PayloadIndex) -> bool {
    let cmd = cmd.trim();
    if cmd.contains([';', '&', '|', '`', '>', '<']) || cmd.contains("$(") {
        return false;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let Some(first) = words.first() else {
        return true;
    };
    if COMMANDS.contains(first) {
        return true;
    }
    if *first != "update-alternatives" {
        return false;
    }
    let unquote = |w: &str| w.trim_matches(|c| c == '\'' || c == '"').to_string();
    match words.as_slice() {
        [_, "--install", _link, _name, path, _prio] => {
            resolve_in_payload(index, Path::new(&unquote(path))).is_some()
        }
        [_, "--remove", _name, _path] => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> PayloadIndex {
        PayloadIndex::default()
    }

    #[test]
    fn script_allowlist() {
        let i = none();
        assert_eq!(
            unrecognized_line(
                "#!/bin/sh\nset -e\ncase \"$1\" in\n  configure)\n    update-desktop-database -q || true\n  ;;\nesac\nexit 0\n",
                &i
            ),
            None
        );
        assert_eq!(
            unrecognized_line("#!/bin/sh\nuseradd foo\n", &i),
            Some("useradd foo".to_string())
        );
        assert!(
            unrecognized_line("echo deb http://x > /etc/apt/sources.list.d/x.list", &i).is_some()
        );
        assert_eq!(
            unrecognized_line(
                "if [ \"$1\" = configure ]; then\n  chmod 4755 /opt/Foo/chrome-sandbox\nfi",
                &i
            ),
            Some("chmod 4755 /opt/Foo/chrome-sandbox".to_string())
        );
        assert_eq!(
            unrecognized_line(
                "post_install() {\n  gtk-update-icon-cache -q -t -f usr/share/icons/hicolor\n}",
                &i
            ),
            None
        );
    }

    #[test]
    fn suffixes_one_liners_and_if_forms() {
        let i = none();
        assert_eq!(
            unrecognized_line("ldconfig >/dev/null 2>&1 || true", &i),
            None
        );
        assert_eq!(
            unrecognized_line("configure|upgrade) ldconfig ;;", &i),
            None
        );
        assert!(unrecognized_line("configure) rm -rf / ;;", &i).is_some());
        assert_eq!(
            unrecognized_line("if [ \"$2\" = \"abort-upgrade\" ]; then", &i),
            None
        );
        assert_eq!(unrecognized_line("systemctl daemon-reload", &i), None);
        assert!(unrecognized_line("systemctl enable foo", &i).is_some());
    }

    #[test]
    fn compound_lines_are_never_allowlisted() {
        let i = none();
        for line in [
            "ldconfig; useradd foo",
            "ldconfig && useradd foo",
            "ldconfig | tee x",
            "ldconfig $(id)",
            "ldconfig `id`",
            "ldconfig > /etc/x",
        ] {
            assert!(unrecognized_line(line, &i).is_some(), "{line}");
        }
    }

    #[test]
    fn assess_classifies() {
        let i = none();
        assert!(matches!(
            assess("ldconfig; useradd foo", &i),
            Some(ScriptIssue::System(_))
        ));
        assert!(matches!(
            assess("if ! getent passwd x; then\n  sudo adduser x\nfi", &i),
            Some(ScriptIssue::System(_))
        ));
        assert!(matches!(
            assess("FOO=1 chmod u+s /opt/x/sb || true", &i),
            Some(ScriptIssue::Setuid(_))
        ));
        assert!(matches!(
            assess("chmod 0755 /opt/x/sb", &i),
            Some(ScriptIssue::Other(_))
        ));
        assert_eq!(assess("ldconfig\n", &i), None);
    }

    #[test]
    fn update_alternatives_needs_a_payload_target() {
        let mut i = none();
        i.files.insert("opt/Foo/foo".into());
        i.dirs.insert("opt/Foo".into());
        i.dirs.insert("opt".into());
        assert_eq!(
            unrecognized_line(
                "update-alternatives --install /usr/bin/foo foo /opt/Foo/foo 100",
                &i
            ),
            None
        );
        assert!(
            unrecognized_line(
                "update-alternatives --install /usr/bin/foo foo /opt/Bar/foo 100",
                &i
            )
            .is_some()
        );
        assert_eq!(
            unrecognized_line("update-alternatives --remove foo /opt/Foo/foo", &i),
            None
        );
    }
}
