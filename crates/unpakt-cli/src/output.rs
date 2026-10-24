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

pub fn print_summary(p: &InstallPlan) {
    let plan = &p.plan;
    println!("name:    {}", plan.name);
    println!(
        "version: {}",
        version_text(&plan.label, plan.payload.version.as_deref())
    );
    println!("id:      {}", plan.id);
    println!("source:  {}", p.source.location);
    println!("asset:   {}", plan.artifact.file_name);
    println!("format:  {} -> {}", plan.payload.format.as_str(), p.backend);
    match &plan.entry {
        Some(entry) => println!("entry:   {}", entry.path.display()),
        None => println!("entry:   (the file itself)"),
    }
    let chosen = plan.entry.as_ref().map(|e| &e.path);
    for other in plan
        .payload
        .entries
        .iter()
        .filter(|e| Some(&e.path) != chosen)
        .take(RUNNER_UPS)
    {
        println!("  other: {}", other.path.display());
    }
    println!("command: {}", p.command.as_deref().unwrap_or("(none)"));
    println!("desktop: {}", if p.want_desktop { "yes" } else { "no" });
    for warning in &plan.payload.warnings {
        println!("warning: {warning}");
    }
}

pub fn print_list(apps: &[AppRecord]) {
    for app in apps {
        println!(
            "{}  {}  {}  {}  {}",
            app.id,
            app.current,
            app.format.as_str(),
            app.name,
            app.source.location
        );
    }
}

pub fn print_info(app: &AppRecord, versions: &[VersionRecord], files: &[FileRecord]) {
    println!("id:      {}", app.id);
    println!("name:    {}", app.name);
    println!("format:  {}", app.format.as_str());
    println!("backend: {}", app.backend);
    println!("source:  {}", app.source.location);
    println!("current: {}", app.current);
    for v in versions {
        println!("version: {}", version_text(&v.label, v.version.as_deref()));
    }
    for f in files {
        println!("file:    {}", f.path.display());
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
    use super::version_text;

    #[test]
    fn raw_version_follows_a_different_label() {
        assert_eq!(version_text("0.12.6", Some("v0.12.6")), "0.12.6 (v0.12.6)");
    }

    #[test]
    fn matching_or_missing_raw_version_shows_the_label_only() {
        assert_eq!(version_text("0.12.6", Some("0.12.6")), "0.12.6");
        assert_eq!(version_text("ab12cd34ef56", None), "ab12cd34ef56");
    }
}
