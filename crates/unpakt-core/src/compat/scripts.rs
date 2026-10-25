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
pub(crate) fn unrecognized_line(text: &str, index: &PayloadIndex) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line_ok(line, index))
        .map(str::to_string)
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
