use unpakt_core::compat::Verdict;
use unpakt_core::engine::{Check, InstallPlan, UpdateInfo};
use unpakt_core::manifest::{AppRecord, FileRecord, VersionRecord};

const RUNNER_UPS: usize = 3;

/// The installed label, followed by the package's own version string in
/// parentheses when that differs.
fn version_text(label: &str, raw: Option<&str>) -> String {
    match raw {
        Some(raw) if raw != label => format!("{label} ({raw})"),
        _ => label.to_string(),
    }
}

/// Replaces control characters in text that came from a package, so it cannot
/// move the cursor or inject terminal escape sequences.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

pub fn print_summary(p: &InstallPlan) {
    for line in summary_lines(p) {
        println!("{line}");
    }
}

pub fn summary_lines(p: &InstallPlan) -> Vec<String> {
    let plan = &p.plan;
    let mut out = vec![
        format!("name:    {}", plan.name),
        format!(
            "version: {}",
            version_text(&plan.label, plan.payload.version.as_deref())
        ),
        format!("id:      {}", plan.id),
        format!("source:  {}", p.source.location),
        format!("asset:   {}", plan.artifact.file_name),
        format!("format:  {} -> {}", plan.payload.format.as_str(), p.backend),
    ];
    out.push(match &plan.entry {
        Some(entry) => format!("entry:   {}", entry.path.display()),
        None => "entry:   (the file itself)".to_string(),
    });
    let chosen = plan.entry.as_ref().map(|e| &e.path);
    out.extend(
        plan.payload
            .entries
            .iter()
            .filter(|e| Some(&e.path) != chosen)
            .take(RUNNER_UPS)
            .map(|other| format!("  other: {}", other.path.display())),
    );
    out.push(format!(
        "command: {}",
        p.command.as_deref().unwrap_or("(none)")
    ));
    out.push(format!(
        "desktop: {}",
        if p.want_desktop { "yes" } else { "no" }
    ));
    out.extend(
        plan.payload
            .warnings
            .iter()
            .map(|w| format!("warning: {w}")),
    );
    out.extend(compat_lines(p.compat.as_ref(), p.native_hint.as_deref()));
    out.iter().map(|line| clean(line)).collect()
}

fn compat_lines(compat: Option<&Verdict>, native_hint: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(verdict) = compat {
        out.push(format!("Compatibility: {}", verdict.label()));
        for finding in verdict.findings() {
            if finding.note {
                out.push(format!("  note: {}", finding.detail));
            } else {
                out.push(format!("  - {}", finding.detail));
            }
        }
    }
    if let Some(hint) = native_hint {
        out.push(format!(
            "You can also install it system-wide yourself: {hint}"
        ));
    }
    out
}

pub fn print_list(apps: &[AppRecord]) {
    for app in apps {
        println!(
            "{}  {}  {}  {}  {}",
            clean(&app.id),
            clean(&app.current),
            app.format.as_str(),
            clean(&app.name),
            clean(&app.source.location)
        );
    }
}

pub fn print_info(app: &AppRecord, versions: &[VersionRecord], files: &[FileRecord]) {
    println!("id:      {}", app.id);
    println!("name:    {}", clean(&app.name));
    println!("format:  {}", app.format.as_str());
    println!("backend: {}", app.backend);
    println!("source:  {}", app.source.location);
    println!("current: {}", app.current);
    for v in versions {
        println!(
            "version: {}",
            clean(&version_text(&v.label, v.version.as_deref()))
        );
    }
    for f in files {
        println!("file:    {}", clean(&f.path.display().to_string()));
    }
}

pub fn print_checks(checks: &[Check]) {
    for c in checks {
        let mark = if c.ok { "[ok]" } else { "[!!]" };
        println!("{mark} {}: {}", c.name, c.detail);
    }
}

pub fn print_updates(updates: &[UpdateInfo]) {
    for u in updates {
        println!("{}  {}  ->  {}", u.id, u.current, u.latest);
    }
}

#[cfg(test)]
mod tests {
    use super::{clean, compat_lines, version_text};
    use unpakt_core::compat::{Finding, Verdict};

    fn finding(tag: &'static str, detail: &str, note: bool) -> Finding {
        Finding {
            tag,
            detail: detail.into(),
            note,
        }
    }

    #[test]
    fn summary_shows_risky_findings() {
        let v = Verdict::Risky(vec![finding(
            "missing-library",
            "missing libraries: libfoo.so.1",
            false,
        )]);
        let lines = compat_lines(Some(&v), None);
        assert!(lines.contains(&"Compatibility: risky".to_string()));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("  - ") && l.contains("libfoo.so.1"))
        );
    }

    #[test]
    fn summary_shows_setuid_note() {
        let v = Verdict::Ok(vec![finding("setuid", "contains a setuid file", true)]);
        let lines = compat_lines(Some(&v), None);
        assert_eq!(lines[0], "Compatibility: ok");
        assert!(lines.iter().any(|l| l.starts_with("  note: ")));
    }

    #[test]
    fn summary_shows_native_hint() {
        let lines = compat_lines(None, Some("sudo apt install ./a.deb"));
        assert_eq!(
            lines,
            ["You can also install it system-wide yourself: sudo apt install ./a.deb"]
        );
    }

    #[test]
    fn summary_without_package_has_no_compat_line() {
        assert!(compat_lines(None, None).is_empty());
    }

    #[test]
    fn raw_version_follows_a_different_label() {
        assert_eq!(version_text("0.12.6", Some("v0.12.6")), "0.12.6 (v0.12.6)");
    }

    #[test]
    fn matching_or_missing_raw_version_shows_the_label_only() {
        assert_eq!(version_text("0.12.6", Some("0.12.6")), "0.12.6");
        assert_eq!(version_text("ab12cd34ef56", None), "ab12cd34ef56");
    }

    #[test]
    fn control_characters_are_replaced() {
        assert_eq!(
            clean("a\x1b[31mb\r\n\x07c"),
            "a\u{FFFD}[31mb\u{FFFD}\u{FFFD}\u{FFFD}c"
        );
        assert_eq!(clean("plain ünï"), "plain ünï");
    }
}
