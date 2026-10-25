use std::path::Path;

use percent_encoding::percent_decode_str;
use unpakt_core::Error;
use unpakt_core::cancel::Cancel;
use unpakt_core::compat::Verdict;
use unpakt_core::engine::{AddOpts, InstallPlan, UpdateInfo};
use unpakt_core::manifest::AppRecord;
use unpakt_core::progress::Progress;

use crate::worker::{Cmd, Event, Op};

/// Turns pasted or typed input into the URL or path it names. Surrounding
/// whitespace is dropped, `file://` URIs are decoded to a path, and empty
/// input yields `None`.
pub fn normalize_input(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.strip_prefix("file://") {
        Some(path) => percent_decode_str(path)
            .decode_utf8()
            .ok()
            .map(|decoded| decoded.into_owned()),
        None => Some(trimmed.to_string()),
    }
}

/// Download progress as a share of the total, clamped to `0.0..=1.0`. `None`
/// when the total is unknown, so the view shows indeterminate progress.
pub fn fraction(done: u64, total: Option<u64>) -> Option<f64> {
    match total {
        None | Some(0) => None,
        Some(total) => Some((done as f64 / total as f64).clamp(0.0, 1.0)),
    }
}

/// The installed label, followed by the package's own version string in
/// parentheses when that differs. Same rule as the CLI summary.
fn version_text(label: &str, raw: Option<&str>) -> String {
    match raw {
        Some(raw) if raw != label => format!("{label} ({raw})"),
        _ => label.to_string(),
    }
}

/// The rows of the review screen, in display order. Warnings are not rows;
/// the view lists them separately.
pub fn review_rows(p: &InstallPlan) -> Vec<(&'static str, String)> {
    let plan = &p.plan;
    let mut rows = vec![
        ("Name", plan.name.clone()),
        (
            "Version",
            version_text(&plan.label, plan.payload.version.as_deref()),
        ),
        ("App id", plan.id.clone()),
        ("Source", p.source.location.clone()),
        ("File", plan.artifact.file_name.clone()),
        (
            "Format",
            format!("{} → {}", plan.payload.format.as_str(), p.backend),
        ),
        (
            "Command",
            p.command.clone().unwrap_or_else(|| "none".into()),
        ),
        (
            "Launcher entry",
            if p.want_desktop { "yes" } else { "no" }.into(),
        ),
    ];
    if let Some(verdict) = &p.compat {
        rows.push(("Compatibility", verdict.label().into()));
    }
    rows
}

/// The compatibility findings as `(blocking, detail)`; notes do not block.
pub fn compat_findings(p: &InstallPlan) -> Vec<(bool, String)> {
    p.compat
        .as_ref()
        .map(|v| {
            v.findings()
                .iter()
                .map(|f| (!f.note, f.detail.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// What the review page offers for a plan.
#[derive(Debug, PartialEq)]
pub enum ReviewAction {
    Install,
    InstallAnyway,
    Blocked(String),
}

pub fn review_action(p: &InstallPlan) -> ReviewAction {
    match &p.compat {
        Some(Verdict::Risky(_)) => ReviewAction::InstallAnyway,
        Some(Verdict::NeedsContainer(_)) => ReviewAction::Blocked(
            p.blocked()
                .unwrap_or_else(|| "this package cannot be installed here".into()),
        ),
        _ => ReviewAction::Install,
    }
}

/// One line of the installed-apps list.
#[derive(Debug, Clone, PartialEq)]
pub struct AppRow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub format: String,
    /// The latest version, when it differs from the installed one.
    pub update: Option<String>,
}

/// The installed apps for the list view, sorted by name ignoring case.
pub fn app_rows(apps: &[AppRecord], updates: &[UpdateInfo]) -> Vec<AppRow> {
    let mut rows: Vec<AppRow> = apps
        .iter()
        .map(|app| AppRow {
            id: app.id.clone(),
            name: app.name.clone(),
            version: app.current.clone(),
            format: app.format.as_str().to_string(),
            update: updates
                .iter()
                .find(|u| u.id == app.id)
                .map(|u| u.latest.clone()),
        })
        .collect();
    rows.sort_by_key(|row| row.name.to_lowercase());
    rows
}

/// What the engine is doing right now, for the progress view.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Busy {
    #[default]
    Idle,
    Working {
        op: Op,
        label: String,
        fraction: Option<f64>,
        waiting: bool,
    },
}

impl Busy {
    /// Whether Cancel can stop what is running. Rolling back and removing run
    /// with a token nothing can cancel.
    pub fn cancellable(&self) -> bool {
        matches!(self, Busy::Working { op, .. } if !matches!(op, Op::Rollback | Op::Remove))
    }
}

/// The window's state. The view sends the command returned by `start` and
/// applies the `Effect` returned by `on_event`.
#[derive(Debug, Default)]
pub struct Model {
    pub busy: Busy,
    pub close_requested: bool,
    /// Cancels the running operation.
    cancel: Option<Cancel>,
    /// A change succeeded and its fresh list has not arrived yet.
    list_pending: bool,
    /// The last inspected input and its options, for retries.
    last_inspect: Option<(String, AddOpts)>,
}

/// What the view must do after an event.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    ShowReview,
    ShowError {
        title: String,
        body: String,
        inspect_again: bool,
    },
    /// The install failed because the command name is taken on the system.
    OfferWithoutCommand {
        title: String,
        body: String,
    },
    Toast(String),
    Close,
}

impl Model {
    /// Marks `op` as running and returns the cancel handle to send with its command.
    pub fn start(&mut self, op: Op) -> Cancel {
        let cancel = Cancel::new();
        self.busy = Busy::Working {
            op,
            label: starting_label(op).into(),
            fraction: None,
            waiting: false,
        };
        self.cancel = Some(cancel.clone());
        self.list_pending = false;
        cancel
    }

    /// Starts inspecting `input` and returns the command to send. The caller
    /// checks that nothing else is running.
    pub fn inspect(&mut self, input: String) -> Cmd {
        self.inspect_with(input, AddOpts::default())
    }

    /// Inspects the last input again with the same options.
    pub fn inspect_again(&mut self) -> Option<Cmd> {
        let (input, opts) = self.last_inspect.clone()?;
        self.idle().then(|| self.inspect_with(input, opts))
    }

    /// Inspects the last input again, leaving out the command.
    pub fn inspect_without_command(&mut self) -> Option<Cmd> {
        let (input, opts) = self.last_inspect.clone()?;
        let opts = AddOpts {
            no_bin: true,
            ..opts
        };
        self.idle().then(|| self.inspect_with(input, opts))
    }

    /// A background reload of the installed apps, for when the window regains
    /// focus. It never makes the model busy, and is skipped while busy.
    pub fn refresh_list(&self) -> Option<Cmd> {
        self.idle().then_some(Cmd::List)
    }

    fn idle(&self) -> bool {
        self.busy == Busy::Idle
    }

    fn inspect_with(&mut self, input: String, opts: AddOpts) -> Cmd {
        self.last_inspect = Some((input.clone(), opts.clone()));
        let cancel = self.start(Op::Inspect);
        Cmd::Inspect {
            input,
            opts,
            cancel,
        }
    }

    /// Applies one worker event to the state and says what the view must do.
    pub fn on_event(&mut self, e: &Event) -> Effect {
        if let Event::Progress(progress) = e {
            if let Busy::Working {
                label,
                fraction: share,
                waiting,
                ..
            } = &mut self.busy
            {
                *waiting = matches!(progress, Progress::WaitingForLock);
                *share = None;
                *label = match progress {
                    Progress::WaitingForLock => {
                        "Waiting for another unpakt operation to finish…".into()
                    }
                    Progress::Fetching => "Looking up the release…".into(),
                    Progress::Downloading { file, done, total } => {
                        *share = fraction(*done, *total);
                        format!("Downloading {file}…")
                    }
                    Progress::Inspecting => "Inspecting…".into(),
                    Progress::Installing => "Installing…".into(),
                };
            }
            return Effect::None;
        }

        // A successful change is followed by `Event::Apps` with the fresh list.
        // Stay busy until that arrives so no new operation can start in between,
        // unless the window is closing, which needs no further events.
        if matches!(self.busy, Busy::Working { .. })
            && matches!(
                e,
                Event::Installed(_)
                    | Event::Updated { .. }
                    | Event::RolledBack { .. }
                    | Event::Removed { .. }
            )
        {
            if self.close_requested {
                self.busy = Busy::Idle;
                self.cancel = None;
                return Effect::Close;
            }
            self.list_pending = true;
            return match e {
                Event::Installed(app) => {
                    Effect::Toast(format!("Installed {} {}", app.name, app.current))
                }
                _ => Effect::None,
            };
        }

        // Startup and background reads never go through `start`, so their
        // results must not end an operation the user is running. Only the list
        // that follows a successful change does.
        if let Busy::Working { op: running, .. } = &self.busy {
            match e {
                Event::Updates { .. } => return Effect::None,
                Event::Apps(_) if !self.list_pending => return Effect::None,
                Event::Failed { op, error }
                    if matches!(op, Op::List | Op::Check)
                        && op != running
                        && !(self.list_pending && *op == Op::List) =>
                {
                    return Effect::ShowError {
                        title: failure_title(*op).into(),
                        body: failure_body(error),
                        inspect_again: false,
                    };
                }
                _ => {}
            }
        }

        let was_working = matches!(self.busy, Busy::Working { .. });
        // A background reload that fails while nothing runs is not worth a dialog.
        if !was_working && matches!(e, Event::Failed { op: Op::List, .. }) {
            return Effect::Toast("Could not refresh the app list".into());
        }
        self.busy = Busy::Idle;
        self.cancel = None;
        self.list_pending = false;

        let effect = match e {
            Event::Failed {
                error: Error::Interrupted,
                ..
            } => Effect::Toast("Cancelled".into()),
            Event::Failed {
                op: Op::Install,
                error: Error::Shadowed { command, by },
            } => Effect::OfferWithoutCommand {
                title: "Command already exists".into(),
                body: shadowed_body(command, by),
            },
            Event::Failed { op, error } => Effect::ShowError {
                title: failure_title(*op).into(),
                body: failure_body(error),
                inspect_again: *op == Op::Install && matches!(error, Error::Conflict(_)),
            },
            Event::Inspected(_) => Effect::ShowReview,
            _ => Effect::None,
        };

        if was_working && self.close_requested {
            Effect::Close
        } else {
            effect
        }
    }

    /// Cancels the running operation, if any. The worker then reports the
    /// outcome as usual.
    pub fn cancel_op(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }

    /// Returns `true` when the window may close now. While an operation runs,
    /// it is cancelled instead and the close waits for its final event.
    pub fn request_close(&mut self) -> bool {
        if let Busy::Working { .. } = self.busy {
            self.close_requested = true;
            self.cancel_op();
            false
        } else {
            true
        }
    }
}

fn starting_label(op: Op) -> &'static str {
    match op {
        Op::Inspect => "Inspecting…",
        Op::Install => "Installing…",
        Op::Check => "Checking for updates…",
        Op::Update => "Updating…",
        Op::Rollback => "Rolling back…",
        Op::Remove => "Removing…",
        Op::List => "Loading…",
    }
}

/// The message for the dialog. The engine's wording for an app that is
/// already installed names the CLI; the window points at its own list.
fn failure_body(error: &Error) -> String {
    match error {
        Error::Conflict(msg) => match msg.split_once("; use `unpakt update") {
            Some((head, _)) => {
                format!("{head}. To get a newer version, use Update in the installed list.")
            }
            None => error.to_string(),
        },
        _ => error.to_string(),
    }
}

fn shadowed_body(command: &str, by: &Path) -> String {
    let dir = by.parent().unwrap_or(by).display();
    format!(
        "`{command}` is already provided by {dir}. Install without adding the `{command}` \
         command? The app still gets its launcher entry."
    )
}

fn failure_title(op: Op) -> &'static str {
    match op {
        Op::Inspect => "Could not read this input",
        Op::Install => "Could not install",
        Op::Check => "Could not check for updates",
        Op::Update => "Could not update",
        Op::Rollback => "Could not roll back",
        Op::Remove => "Could not remove",
        Op::List => "Could not load apps",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::{Path, PathBuf};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use unpakt_core::Layout;
    use unpakt_core::engine::Engine;
    use unpakt_core::host::Host;
    use unpakt_core::manifest::AppRecord;
    use unpakt_core::model::{FormatKind, SourceCfg, SourceKind};

    #[test]
    fn normalize_input_cases() {
        assert_eq!(
            normalize_input("  https://github.com/o/r \n"),
            Some("https://github.com/o/r".into())
        );
        assert_eq!(normalize_input("\n\t "), None);
        assert_eq!(
            normalize_input("file:///home/u/My%20Apps/x.AppImage"),
            Some("/home/u/My Apps/x.AppImage".into())
        );
        assert_eq!(
            normalize_input("/tmp/x.tar.gz"),
            Some("/tmp/x.tar.gz".into())
        );
    }

    #[test]
    fn fraction_without_total_is_none() {
        assert_eq!(fraction(10, None), None);
        assert_eq!(fraction(10, Some(0)), None);
        assert_eq!(fraction(5, Some(10)), Some(0.5));
        assert_eq!(fraction(20, Some(10)), Some(1.0));
    }

    fn fixture_tarball(dir: &Path) -> PathBuf {
        let path = dir.join("unpakt-fixture-tool-1.0.tar.gz");
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

    fn app(id: &str, name: &str, current: &str) -> AppRecord {
        AppRecord {
            id: id.into(),
            name: name.into(),
            format: FormatKind::Tarball,
            backend: "tarball".into(),
            source: SourceCfg {
                kind: SourceKind::LocalFile,
                location: "/x.tar.gz".into(),
                pattern: None,
            },
            current: current.into(),
            desktop: false,
            command: None,
        }
    }

    #[test]
    fn review_rows_follow_the_cli_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let tarball = fixture_tarball(tmp.path());
        let engine = Engine::new(
            Layout::under(tmp.path()),
            Host {
                fuse2: true,
                ..Host::detect()
            },
        )
        .unwrap();
        let mut plan = engine
            .inspect(tarball.to_str().unwrap(), AddOpts::default())
            .unwrap();

        let names: Vec<&str> = review_rows(&plan).iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "Name",
                "Version",
                "App id",
                "Source",
                "File",
                "Format",
                "Command",
                "Launcher entry",
            ]
        );
        let rows = review_rows(&plan);
        assert_eq!(rows[0].1, plan.plan.name);
        assert_eq!(rows[1].1, plan.plan.label);
        assert_eq!(rows[2].1, plan.plan.id);
        assert_eq!(rows[3].1, plan.source.location);
        assert_eq!(rows[4].1, plan.plan.artifact.file_name);
        assert_eq!(rows[5].1, format!("tarball → {}", plan.backend));
        assert_eq!(rows[6].1, "unpakt-fixture-tool");
        assert_eq!(rows[7], ("Launcher entry", "no".to_string()));

        plan.command = Some("unpakt-fixture-tool --run".into());
        plan.want_desktop = true;
        plan.plan.payload.version = Some("1.0.3-rc".into());
        let rows = review_rows(&plan);
        assert_eq!(rows[1].1, format!("{} (1.0.3-rc)", plan.plan.label));
        assert_eq!(rows[6].1, "unpakt-fixture-tool --run");
        assert_eq!(rows[7], ("Launcher entry", "yes".to_string()));
        plan.command = None;
        assert_eq!(review_rows(&plan)[6].1, "none");
    }

    fn finding(detail: &str, note: bool) -> unpakt_core::compat::Finding {
        unpakt_core::compat::Finding {
            tag: "glibc",
            detail: detail.into(),
            note,
        }
    }

    fn tarball_plan(tmp: &Path) -> InstallPlan {
        let tarball = fixture_tarball(tmp);
        let engine = Engine::new(
            Layout::under(tmp),
            Host {
                fuse2: true,
                ..Host::detect()
            },
        )
        .unwrap();
        engine
            .inspect(tarball.to_str().unwrap(), AddOpts::default())
            .unwrap()
    }

    #[test]
    fn review_rows_include_compatibility_for_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let mut plan = tarball_plan(tmp.path());
        assert!(
            review_rows(&plan)
                .iter()
                .all(|(n, _)| *n != "Compatibility")
        );
        plan.compat = Some(Verdict::Risky(vec![finding("needs glibc 2.99", false)]));
        let rows = review_rows(&plan);
        assert_eq!(
            rows.last().unwrap(),
            &("Compatibility", "risky".to_string())
        );
    }

    #[test]
    fn review_action_follows_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let mut plan = tarball_plan(tmp.path());
        assert_eq!(review_action(&plan), ReviewAction::Install);
        plan.compat = Some(Verdict::Ok(vec![finding("a note", true)]));
        assert_eq!(review_action(&plan), ReviewAction::Install);
        plan.compat = Some(Verdict::Risky(vec![finding("odd", false)]));
        assert_eq!(review_action(&plan), ReviewAction::InstallAnyway);
        plan.compat = Some(Verdict::NeedsContainer(vec![finding("too new", false)]));
        match review_action(&plan) {
            ReviewAction::Blocked(m) => assert!(m.contains("nothing was installed")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn compat_findings_mark_notes_non_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let mut plan = tarball_plan(tmp.path());
        assert!(compat_findings(&plan).is_empty());
        plan.compat = Some(Verdict::Risky(vec![
            finding("bad", false),
            finding("fyi", true),
        ]));
        assert_eq!(
            compat_findings(&plan),
            vec![(true, "bad".to_string()), (false, "fyi".to_string())]
        );
    }

    #[test]
    fn app_rows_mark_updates_and_sort() {
        let apps = [app("b", "b", "1.0"), app("a", "A", "0.9")];
        let updates = [UpdateInfo {
            id: "b".into(),
            current: "1.0".into(),
            latest: "2.0".into(),
        }];
        let rows = app_rows(&apps, &updates);
        assert_eq!(
            rows,
            [
                AppRow {
                    id: "a".into(),
                    name: "A".into(),
                    version: "0.9".into(),
                    format: "tarball".into(),
                    update: None,
                },
                AppRow {
                    id: "b".into(),
                    name: "b".into(),
                    version: "1.0".into(),
                    format: "tarball".into(),
                    update: Some("2.0".into()),
                },
            ]
        );
    }

    #[test]
    fn waiting_for_lock_sets_waiting_label() {
        let mut m = Model::default();
        m.start(Op::Remove);
        m.on_event(&Event::Progress(Progress::WaitingForLock));
        assert_eq!(
            m.busy,
            Busy::Working {
                op: Op::Remove,
                waiting: true,
                label: "Waiting for another unpakt operation to finish…".into(),
                fraction: None,
            }
        );
    }

    #[test]
    fn downloading_reports_file_and_fraction() {
        let mut m = Model::default();
        m.start(Op::Install);
        m.on_event(&Event::Progress(Progress::Downloading {
            file: "tool.tar.gz".into(),
            done: 25,
            total: Some(100),
        }));
        assert_eq!(
            m.busy,
            Busy::Working {
                op: Op::Install,
                waiting: false,
                label: "Downloading tool.tar.gz…".into(),
                fraction: Some(0.25),
            }
        );
    }

    #[test]
    fn conflict_on_install_offers_inspect_again() {
        let mut m = Model::default();
        m.start(Op::Install);
        let effect = m.on_event(&Event::Failed {
            op: Op::Install,
            error: Error::Conflict("x".into()),
        });
        assert!(matches!(
            effect,
            Effect::ShowError {
                inspect_again: true,
                ..
            }
        ));
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn close_while_busy_cancels_and_defers() {
        let mut m = Model::default();
        let c = m.start(Op::Install);
        assert!(!m.request_close());
        assert!(c.is_cancelled());
        assert_eq!(
            m.on_event(&Event::Failed {
                op: Op::Install,
                error: Error::Interrupted,
            }),
            Effect::Close
        );
    }

    #[test]
    fn close_after_commit_won_keeps_the_install() {
        let mut m = Model::default();
        let c = m.start(Op::Install);
        assert!(!m.request_close());
        assert!(c.is_cancelled());
        // The install had already committed, so the worker reports it and the
        // window closes without any rollback.
        assert_eq!(
            m.on_event(&Event::Installed(app("a", "A", "1.0"))),
            Effect::Close
        );
    }

    #[test]
    fn stays_busy_until_the_fresh_list_arrives() {
        let mut m = Model::default();
        m.start(Op::Install);
        assert_eq!(
            m.on_event(&Event::Installed(app("a", "A", "1.0"))),
            Effect::Toast("Installed A 1.0".into())
        );
        assert!(matches!(m.busy, Busy::Working { .. }));
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn background_list_keeps_a_running_op_busy() {
        let mut m = Model::default();
        let c = m.start(Op::Update);
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert!(matches!(m.busy, Busy::Working { op: Op::Update, .. }));
        m.cancel_op();
        assert!(c.is_cancelled());
        assert_eq!(
            m.on_event(&Event::Updates {
                available: vec![],
                failed: 0,
            }),
            Effect::None
        );
        assert!(matches!(m.busy, Busy::Working { .. }));
        // The mutation's own trailing list still ends it.
        m.on_event(&Event::Updated {
            id: "a".into(),
            label: None,
        });
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn list_from_before_the_change_does_not_end_it() {
        let mut m = Model::default();
        m.start(Op::Remove);
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert!(matches!(m.busy, Busy::Working { .. }));
        m.on_event(&Event::Removed { id: "a".into() });
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn background_list_does_not_end_an_inspect() {
        let mut m = Model::default();
        m.start(Op::Inspect);
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert!(matches!(m.busy, Busy::Working { .. }));
    }

    #[test]
    fn updates_while_idle_change_nothing() {
        let mut m = Model::default();
        let event = Event::Updates {
            available: vec![],
            failed: 1,
        };
        assert_eq!(m.on_event(&event), Effect::None);
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn background_failure_keeps_a_running_op_busy() {
        let mut m = Model::default();
        m.start(Op::Remove);
        let effect = m.on_event(&Event::Failed {
            op: Op::Check,
            error: Error::Unsupported("x".into()),
        });
        assert!(matches!(effect, Effect::ShowError { .. }));
        assert!(matches!(m.busy, Busy::Working { .. }));
    }

    #[test]
    fn cancel_op_cancels_without_closing() {
        let mut m = Model::default();
        let c = m.start(Op::Inspect);
        m.cancel_op();
        assert!(c.is_cancelled());
        assert!(!m.close_requested);
    }

    #[test]
    fn close_when_idle_is_immediate() {
        assert!(Model::default().request_close());
    }

    #[test]
    fn conflict_wording_points_at_the_installed_list() {
        let mut m = Model::default();
        m.start(Op::Install);
        let effect = m.on_event(&Event::Failed {
            op: Op::Install,
            error: Error::Conflict("tool is already installed; use `unpakt update tool`".into()),
        });
        let Effect::ShowError { body, .. } = effect else {
            panic!("expected an error dialog");
        };
        assert!(!body.contains("unpakt update"), "{body}");
        assert!(body.contains("tool is already installed"), "{body}");
        assert!(body.contains("Update"), "{body}");
    }

    #[test]
    fn failed_trailing_list_ends_the_change_as_a_list_failure() {
        let mut m = Model::default();
        m.start(Op::Install);
        assert!(matches!(
            m.on_event(&Event::Installed(app("a", "A", "1.0"))),
            Effect::Toast(_)
        ));
        let effect = m.on_event(&Event::Failed {
            op: Op::List,
            error: Error::Refused("db".into()),
        });
        assert!(
            matches!(&effect, Effect::ShowError { title, inspect_again: false, .. } if title == "Could not load apps"),
            "{effect:?}"
        );
        assert_eq!(m.busy, Busy::Idle);
        assert!(m.cancel.is_none());
    }

    #[test]
    fn rollback_and_remove_cannot_be_cancelled() {
        let mut m = Model::default();
        assert!(!m.busy.cancellable());
        for op in [Op::Rollback, Op::Remove] {
            m.start(op);
            assert!(!m.busy.cancellable(), "{op:?}");
        }
        for op in [Op::Inspect, Op::Install, Op::Update] {
            m.start(op);
            assert!(m.busy.cancellable(), "{op:?}");
        }
    }

    fn shadowed() -> Error {
        Error::Shadowed {
            command: "nvim".into(),
            by: PathBuf::from("/usr/bin/nvim"),
        }
    }

    #[test]
    fn shadowed_install_offers_install_without_command() {
        let mut m = Model::default();
        m.start(Op::Install);
        let effect = m.on_event(&Event::Failed {
            op: Op::Install,
            error: shadowed(),
        });
        let Effect::OfferWithoutCommand { title, body } = effect else {
            panic!("expected the shadowed-command dialog, got {effect:?}");
        };
        assert_eq!(title, "Command already exists");
        assert_eq!(
            body,
            "`nvim` is already provided by /usr/bin. Install without adding the `nvim` command? \
             The app still gets its launcher entry."
        );
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn shadowed_elsewhere_is_a_plain_error() {
        let mut m = Model::default();
        m.start(Op::Update);
        let effect = m.on_event(&Event::Failed {
            op: Op::Update,
            error: shadowed(),
        });
        assert!(matches!(
            effect,
            Effect::ShowError {
                inspect_again: false,
                ..
            }
        ));
    }

    #[test]
    fn inspect_without_command_reuses_the_input() {
        let mut m = Model::default();
        let Cmd::Inspect { input, opts, .. } = m.inspect("https://github.com/o/r".into()) else {
            panic!("expected an inspect command");
        };
        assert_eq!(input, "https://github.com/o/r");
        assert!(!opts.no_bin);
        m.on_event(&Event::Failed {
            op: Op::Install,
            error: shadowed(),
        });
        let Some(Cmd::Inspect { input, opts, .. }) = m.inspect_without_command() else {
            panic!("expected an inspect command");
        };
        assert_eq!(input, "https://github.com/o/r");
        assert!(opts.no_bin);
        assert!(matches!(
            m.busy,
            Busy::Working {
                op: Op::Inspect,
                ..
            }
        ));
    }

    #[test]
    fn inspect_without_command_needs_a_stored_input_and_idle() {
        let mut m = Model::default();
        assert!(m.inspect_without_command().is_none());
        m.inspect("x".into());
        assert!(m.inspect_without_command().is_none());
    }

    #[test]
    fn inspect_again_keeps_the_options() {
        let mut m = Model::default();
        m.inspect("x".into());
        m.on_event(&Event::Failed {
            op: Op::Install,
            error: Error::Conflict("c".into()),
        });
        m.inspect_without_command();
        m.on_event(&Event::Failed {
            op: Op::Install,
            error: Error::Conflict("c".into()),
        });
        let Some(Cmd::Inspect { opts, .. }) = m.inspect_again() else {
            panic!("expected an inspect command");
        };
        assert!(opts.no_bin);
    }

    #[test]
    fn refresh_list_runs_in_the_background_only_when_idle() {
        let mut m = Model::default();
        assert!(matches!(m.refresh_list(), Some(Cmd::List)));
        assert_eq!(m.busy, Busy::Idle);
        assert_eq!(m.on_event(&Event::Apps(vec![])), Effect::None);
        assert_eq!(m.busy, Busy::Idle);
        m.start(Op::Update);
        assert!(m.refresh_list().is_none());
    }

    #[test]
    fn background_list_failure_while_idle_is_a_toast() {
        let mut m = Model::default();
        let effect = m.on_event(&Event::Failed {
            op: Op::List,
            error: Error::Refused("db".into()),
        });
        assert_eq!(
            effect,
            Effect::Toast("Could not refresh the app list".into())
        );
        assert_eq!(m.busy, Busy::Idle);
    }

    #[test]
    fn trailing_list_failure_still_shows_a_dialog() {
        let mut m = Model::default();
        m.start(Op::Remove);
        m.on_event(&Event::Removed { id: "a".into() });
        let effect = m.on_event(&Event::Failed {
            op: Op::List,
            error: Error::Refused("db".into()),
        });
        assert!(matches!(effect, Effect::ShowError { .. }), "{effect:?}");
    }
}
