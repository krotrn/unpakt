//! The main window: install flow.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk::{self, gdk, gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, SimpleComponent, WorkerController};
use unpakt_core::engine::{InstallPlan, UpdateInfo};
use unpakt_core::manifest::AppRecord;

use crate::apps::{self, Detail};
use crate::review;
use crate::state::{Busy, Effect, Model, app_rows, normalize_input};
use crate::worker::{Cmd, EngineWorker, Event, Op};

#[derive(Debug)]
pub enum Msg {
    Submit(String),
    FileChosen(PathBuf),
    BrowseClicked,
    Confirm,
    CancelReview,
    CancelOp,
    InspectAgain,
    Worker(Event),
    CloseRequested,
    OpenApp(String),
    DetailClosed,
    UpdateApp(String),
    RollbackApp(String),
    RemoveClicked(String),
    RemoveApp(String),
}

pub struct App {
    model: Model,
    worker: WorkerController<EngineWorker>,
    /// The inspected plan waiting on the review page.
    plan: Option<Box<InstallPlan>>,
    /// The last input sent to inspect, for "Inspect again".
    last_input: Option<String>,
    /// Set once the window may close without asking the model again.
    may_close: Rc<Cell<bool>>,
    /// The installed apps and known updates behind the list.
    apps: Vec<AppRecord>,
    updates: Vec<UpdateInfo>,
    /// The detail page on top of the list, if one is open.
    detail: Option<Detail>,
    ui: Ui,
}

/// Widgets the update step drives directly.
struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    entry: gtk::Entry,
    inspect: gtk::Button,
    browse: gtk::Button,
    list_slot: gtk::Box,
    busy_box: gtk::Box,
    busy_label: gtk::Label,
    progress: gtk::ProgressBar,
    cancel: gtk::Button,
}

impl SimpleComponent for App {
    type Init = ();
    type Input = Msg;
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title("unpakt")
            .default_width(480)
            .default_height(640)
            .build()
    }

    fn init((): (), window: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let worker = EngineWorker::builder()
            .detach_worker(())
            .forward(sender.input_sender(), Msg::Worker);

        let entry = gtk::Entry::builder()
            .placeholder_text("Paste a release URL or a file path")
            .hexpand(true)
            .build();
        entry.connect_activate({
            let sender = sender.clone();
            move |entry| sender.input(Msg::Submit(entry.text().into()))
        });
        let inspect = gtk::Button::with_label("Inspect");
        inspect.add_css_class("suggested-action");
        inspect.connect_clicked({
            let sender = sender.clone();
            let entry = entry.clone();
            move |_| sender.input(Msg::Submit(entry.text().into()))
        });
        let browse = gtk::Button::with_label("Choose a local file");
        browse.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(Msg::BrowseClicked)
        });

        let input_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        input_row.append(&entry);
        input_row.append(&inspect);

        let busy_label = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let progress = gtk::ProgressBar::new();
        progress.set_pulse_step(0.1);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.set_halign(gtk::Align::End);
        cancel.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(Msg::CancelOp)
        });
        let busy_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        busy_box.set_visible(false);
        busy_box.append(&busy_label);
        busy_box.append(&progress);
        busy_box.append(&cancel);

        let list_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.append(&input_row);
        content.append(&browse);
        content.append(&busy_box);
        content.append(&list_slot);
        let clamp = adw::Clamp::builder()
            .maximum_size(560)
            .child(&content)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();

        let header = adw::HeaderBar::new();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroll));

        let drop = gtk::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);
        drop.connect_drop({
            let sender = sender.clone();
            move |_, value, _, _| match value.get::<gio::File>().ok().and_then(|f| f.path()) {
                Some(path) => {
                    sender.input(Msg::FileChosen(path));
                    true
                }
                None => false,
            }
        });
        toolbar.add_controller(drop);

        let root_page = adw::NavigationPage::new(&toolbar, "unpakt");
        let nav = adw::NavigationView::new();
        nav.add(&root_page);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));
        window.set_content(Some(&toasts));

        // The model decides whether the window may close; this handler only
        // forwards the request and keeps the window open until it says so.
        let may_close = Rc::new(Cell::new(false));
        window.connect_close_request({
            let sender = sender.clone();
            let may_close = may_close.clone();
            move |_| {
                if may_close.get() {
                    glib::Propagation::Proceed
                } else {
                    sender.input(Msg::CloseRequested);
                    glib::Propagation::Stop
                }
            }
        });

        let app = App {
            model: Model::default(),
            worker,
            plan: None,
            last_input: None,
            may_close,
            apps: Vec::new(),
            updates: Vec::new(),
            detail: None,
            ui: Ui {
                window: window.clone(),
                toasts,
                nav,
                entry,
                inspect,
                browse,
                list_slot,
                busy_box,
                busy_label,
                progress,
                cancel,
            },
        };
        app.worker.emit(Cmd::List);
        app.worker.emit(Cmd::CheckUpdates);
        ComponentParts {
            model: app,
            widgets: (),
        }
    }

    fn update(&mut self, msg: Msg, sender: ComponentSender<Self>) {
        match msg {
            Msg::Submit(raw) => self.inspect(&raw),
            Msg::FileChosen(path) => self.inspect(&path.to_string_lossy()),
            Msg::BrowseClicked => {
                let sender = sender.clone();
                gtk::FileDialog::new().open(
                    Some(&self.ui.window),
                    None::<&gio::Cancellable>,
                    move |chosen| {
                        if let Some(path) = chosen.ok().and_then(|f| f.path()) {
                            sender.input(Msg::FileChosen(path));
                        }
                    },
                );
            }
            Msg::InspectAgain => {
                if let Some(input) = self.last_input.clone() {
                    self.inspect(&input);
                }
            }
            Msg::Confirm => {
                if self.model.busy != Busy::Idle {
                    return;
                }
                if let Some(plan) = self.plan.take() {
                    let cancel = self.model.start(Op::Install);
                    self.worker.emit(Cmd::Install { plan, cancel });
                    self.ui.nav.pop();
                }
            }
            Msg::CancelReview => {
                self.discard_plan();
                self.ui.nav.pop();
            }
            Msg::CancelOp => self.model.cancel_op(),
            Msg::Worker(event) => self.on_event(event, &sender),
            Msg::OpenApp(id) => self.open_detail(&id, &sender),
            Msg::DetailClosed => self.detail = None,
            Msg::UpdateApp(id) => {
                if self.model.busy == Busy::Idle {
                    let cancel = self.model.start(Op::Update);
                    self.worker.emit(Cmd::Update { id, cancel });
                }
            }
            Msg::RollbackApp(id) => {
                if self.model.busy == Busy::Idle {
                    self.model.start(Op::Rollback);
                    self.worker.emit(Cmd::Rollback { id });
                }
            }
            Msg::RemoveClicked(id) => {
                if self.model.busy == Busy::Idle {
                    let name = self
                        .apps
                        .iter()
                        .find(|a| a.id == id)
                        .map_or_else(|| id.clone(), |a| a.name.clone());
                    apps::confirm_remove(&self.ui.window, &id, &name, &sender);
                }
            }
            Msg::RemoveApp(id) => {
                if self.model.busy == Busy::Idle {
                    self.model.start(Op::Remove);
                    self.worker.emit(Cmd::Remove { id });
                }
            }
            Msg::CloseRequested => {
                if self.model.request_close() {
                    self.discard_plan();
                    self.close();
                }
            }
        }
        self.refresh();
    }
}

impl App {
    fn inspect(&mut self, raw: &str) {
        if self.model.busy != Busy::Idle {
            return;
        }
        let Some(input) = normalize_input(raw) else {
            return;
        };
        self.last_input = Some(input.clone());
        let cancel = self.model.start(Op::Inspect);
        self.worker.emit(Cmd::Inspect { input, cancel });
    }

    fn discard_plan(&mut self) {
        if let Some(plan) = self.plan.take() {
            self.worker.emit(Cmd::Discard(plan));
        }
    }

    fn close(&self) {
        self.may_close.set(true);
        self.ui.window.close();
    }

    fn open_detail(&mut self, id: &str, sender: &ComponentSender<Self>) {
        if self.detail.is_some() {
            return;
        }
        if let Some(app) = self.apps.iter().find(|a| a.id == id) {
            let detail = Detail::new(app, self.has_update(id), sender);
            self.ui.nav.push(&detail.page);
            self.detail = Some(detail);
        }
    }

    fn has_update(&self, id: &str) -> bool {
        self.updates.iter().any(|u| u.id == id)
    }

    /// Redraws the list and the open detail page from the current data.
    fn render_apps(&mut self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.ui.list_slot.first_child() {
            self.ui.list_slot.remove(&child);
        }
        let rows = app_rows(&self.apps, &self.updates);
        self.ui.list_slot.append(&apps::list(&rows, sender));
        if let Some(detail) = &self.detail {
            match self.apps.iter().find(|a| a.id == detail.id) {
                Some(app) => detail.refresh(app, self.has_update(&detail.id)),
                None => {
                    self.ui.nav.pop();
                }
            }
        }
    }

    fn toast(&self, text: &str) {
        self.ui.toasts.add_toast(adw::Toast::new(text));
    }

    fn on_event(&mut self, event: Event, sender: &ComponentSender<Self>) {
        if let Event::Inspected(plan) = &event {
            self.plan = Some(plan.clone());
        }
        let effect = self.model.on_event(&event);
        match &event {
            Event::Apps(apps) => {
                self.apps = apps.clone();
                self.render_apps(sender);
            }
            Event::Updates { available, failed } => {
                self.updates = available.clone();
                self.render_apps(sender);
                if *failed > 0 {
                    self.toast(&format!("Could not check {failed} app(s) for updates"));
                }
            }
            Event::Updated { id, label } => match label {
                Some(label) => {
                    self.toast(&format!("Updated {id} to {label}"));
                    self.worker.emit(Cmd::CheckUpdates);
                }
                None => self.toast(&format!("{id} is up to date")),
            },
            Event::RolledBack { id, label } => {
                self.toast(&format!("Rolled back {id} to {label}"));
            }
            Event::Removed { id } => {
                self.toast(&format!("Removed {id}"));
                if self.detail.as_ref().is_some_and(|d| &d.id == id) {
                    self.detail = None;
                    self.ui.nav.pop();
                }
            }
            _ => {}
        }
        if matches!(event, Event::Progress(_))
            && matches!(self.model.busy, Busy::Working { fraction: None, .. })
        {
            self.ui.progress.pulse();
        }
        match effect {
            Effect::None => {}
            Effect::ShowReview => {
                if let Some(plan) = &self.plan {
                    self.ui.nav.push(&review::page(plan, sender));
                }
            }
            Effect::ShowError {
                title,
                body,
                inspect_again,
            } => self.show_error(&title, &body, inspect_again, sender),
            Effect::Toast(text) => self.toast(&text),
            Effect::Close => {
                self.discard_plan();
                self.close();
            }
        }
    }

    fn show_error(
        &self,
        title: &str,
        body: &str,
        inspect_again: bool,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::AlertDialog::new(Some(title), Some(body));
        dialog.add_response("close", "Close");
        dialog.set_close_response("close");
        if inspect_again {
            dialog.add_response("again", "Inspect again");
            dialog.set_response_appearance("again", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("again"));
            let sender = sender.clone();
            dialog.connect_response(Some("again"), move |_, _| sender.input(Msg::InspectAgain));
        }
        dialog.present(Some(&self.ui.window));
    }

    /// Shows the model's busy state: progress area and input sensitivity.
    fn refresh(&self) {
        let ui = &self.ui;
        let idle = self.model.busy == Busy::Idle;
        ui.entry.set_sensitive(idle);
        ui.inspect.set_sensitive(idle);
        ui.browse.set_sensitive(idle);
        ui.busy_box.set_visible(!idle);
        if let Busy::Working {
            label, fraction, ..
        } = &self.model.busy
        {
            ui.busy_label.set_label(label);
            if let Some(share) = fraction {
                ui.progress.set_fraction(*share);
            }
        }
        if let Some(detail) = &self.detail {
            detail.set_idle(idle);
        }
        ui.cancel.set_visible(self.model.busy.cancellable());
        ui.cancel
            .set_sensitive(self.model.busy.cancellable() && !self.model.close_requested);
    }
}
