use std::sync::OnceLock;

use regex::Regex;

const EXTENSIONS: &[&str] = &[
    ".appimage",
    ".tar.gz",
    ".tgz",
    ".tar.xz",
    ".txz",
    ".tar.zst",
    ".tar.bz2",
    ".tbz2",
    ".tar",
];
const NOISE: &[&str] = &[
    "x86_64", "amd64", "x64", "aarch64", "arm64", "linux", "gnu", "musl", "x86", "64", "static",
    "portable",
];

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^v?\d+(\.\d+)+[0-9A-Za-z.+-]*$").expect("static regex"))
}

/// Splits a release file name into an app name and an optional version.
pub fn parse_file_name(name: &str) -> (String, Option<String>) {
    let lower = name.to_ascii_lowercase();
    let stem = EXTENSIONS
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map_or(name, |ext| &name[..name.len() - ext.len()]);

    let tokens: Vec<&str> = stem
        .split(['-', '_'])
        .filter(|t| !t.is_empty() && !NOISE.contains(&t.to_ascii_lowercase().as_str()))
        .collect();

    let version_at = tokens.iter().position(|t| version_re().is_match(t));
    let (name_tokens, version) = match version_at {
        Some(i) => (
            &tokens[..i],
            Some(tokens[i].trim_start_matches('v').to_string()),
        ),
        None => (&tokens[..], None),
    };

    let app = if name_tokens.is_empty() {
        stem.to_string()
    } else {
        name_tokens.join("-")
    };
    (app, version)
}

/// Lowercase ASCII identifier: runs of other characters become `-`.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Makes a raw version string safe to use as a single path component.
pub fn version_label(raw: &str) -> String {
    let stripped = match raw.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => raw,
    };
    let clean: String = stripped
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._+-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() || clean.starts_with('.') {
        format!("_{clean}")
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_version() {
        assert_eq!(
            parse_file_name("Obsidian-1.6.7.AppImage"),
            ("Obsidian".into(), Some("1.6.7".into()))
        );
        assert_eq!(
            parse_file_name("nvim-linux-x86_64.tar.gz"),
            ("nvim".into(), None)
        );
        assert_eq!(
            parse_file_name("tool_v2.0.1_linux_amd64.tar.xz"),
            ("tool".into(), Some("2.0.1".into()))
        );
        assert_eq!(parse_file_name("App-x86_64.AppImage"), ("App".into(), None));
    }
    #[test]
    fn slugs() {
        assert_eq!(slug("Obsidian"), "obsidian");
        assert_eq!(slug("My App!"), "my-app");
        assert_eq!(slug("--Foo__Bar--"), "foo-bar");
    }
    #[test]
    fn version_label_is_safe() {
        assert_eq!(version_label("v1.6.7"), "1.6.7");
        assert_eq!(version_label("../etc"), "_.._etc");
        assert_eq!(version_label(""), "_");
        for raw in [".x", "..", "/a", "v", "a/b"] {
            let l = version_label(raw);
            assert!(
                !l.starts_with('.') && !l.contains('/') && !l.is_empty(),
                "{raw} -> {l}"
            );
        }
    }
}
