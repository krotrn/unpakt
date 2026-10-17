use unpakt_core::engine::{Check, InstallPlan, UpdateInfo};
use unpakt_core::manifest::{AppRecord, FileRecord, VersionRecord};

const RUNNER_UPS: usize = 3;

pub fn print_summary(p: &InstallPlan) {
    let plan = &p.plan;
    println!("name:    {}", plan.name);
    println!(
        "version: {}",
        plan.payload.version.as_deref().unwrap_or(&plan.label)
    );
    println!("id:      {}", plan.id);
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
        println!("{}  {}  {}", app.id, app.current, app.format.as_str());
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
        match &v.version {
            Some(version) => println!("version: {} ({version})", v.label),
            None => println!("version: {}", v.label),
        }
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
