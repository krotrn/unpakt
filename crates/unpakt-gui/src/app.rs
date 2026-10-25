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

use crate::apps::{self, BusyArea, Detail};
use crate::review;
use crate::state::{Busy, Effect, Model, app_rows, compat_findings, normalize_input};
use crate::worker::{Cmd, EngineWorker, Event, Op};

#[derive(Debug)]
pub enum Msg {
    Submit(String),
    FileChosen(PathBuf),
    BrowseClicked,
    Confirm,
    ConfirmRisky,
    RiskyAccepted,
    CancelReview,
    CancelOp,
    InspectAgain,
    InspectWithoutCommand,
    WindowActivated,
    Worker(Event),
    CloseRequested,
    OpenApp(String),
    DetailClosed,
    UpdateApp(String),
    RollbackApp(String),
    UpdateAnyway,
    RemoveClicked(String),
    RemoveApp(String),
}

pub struct App {
    model: Model,
    worker: WorkerController<EngineWorker>,
    /// The inspected plan waiting on the review page.
    plan: Option<Box<InstallPlan>>,
    /// Set once the window may close without asking the model again.
    may_close: Rc<Cell<bool>>,
    /// The installed apps and known updates behind the list.
    apps: Vec<AppRecord>,
    updates: Vec<UpdateInfo>,
    /// The app of the last update started, for accepting its risks.
    last_update: Option<String>,
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
    busy: BusyArea,
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

        let busy = BusyArea::new(&sender);

        let list_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.append(&input_row);
        content.append(&browse);
        content.append(&busy.root);
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

        // The first activation is the window appearing; startup loads the list itself.
        let first_activation = Cell::new(true);
        window.connect_is_active_notify({
            let sender = sender.clone();
            move |window| {
                if window.is_active() && !first_activation.replace(false) {
                    sender.input(Msg::WindowActivated);
                }
            }
        });

        let app = App {
            model: Model::default(),
            worker,
            plan: None,
            may_close,
            apps: Vec::new(),
            updates: Vec::new(),
            last_update: None,
            detail: None,
            ui: Ui {
                window: window.clone(),
                toasts,
                nav,
                entry,
                inspect,
                browse,
                list_slot,
                busy,
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
                if let Some(cmd) = self.model.inspect_again() {
                    self.worker.emit(cmd);
                }
            }
            Msg::InspectWithoutCommand => {
                if let Some(cmd) = self.model.inspect_without_command() {
                    self.worker.emit(cmd);
                }
            }
            Msg::WindowActivated => {
                if let Some(cmd) = self.model.refresh_list() {
                    self.worker.emit(cmd);
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
            Msg::ConfirmRisky => {
                if self.model.busy != Busy::Idle {
                    return;
                }
                if let Some(plan) = &self.plan {
                    let body = compat_findings(plan)
                        .into_iter()
                        .filter(|(blocking, _)| *blocking)
                        .map(|(_, detail)| detail)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let dialog = adw::AlertDialog::new(Some("Install anyway?"), Some(&body));
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("install", "Install anyway");
                    dialog.set_response_appearance("install", adw::ResponseAppearance::Destructive);
                    dialog.set_close_response("cancel");
                    let sender = sender.clone();
                    dialog.connect_response(Some("install"), move |_, _| {
                        sender.input(Msg::RiskyAccepted);
                    });
                    dialog.present(Some(&self.ui.window));
                }
            }
            Msg::RiskyAccepted => {
                if let Some(plan) = self.plan.as_mut() {
                    plan.opts.allow_risky = true;
                }
                sender.input(Msg::Confirm);
            }
            Msg::CancelReview => {
                self.discard_plan();
                self.ui.nav.pop();
            }
            Msg::CancelOp => self.model.cancel_op(),
            Msg::Worker(event) => self.on_event(event, &sender),
            Msg::OpenApp(id) => self.open_detail(&id, &sender),
            Msg::DetailClosed => self.detail = None,
            Msg::UpdateApp(id) => self.start_update(id, false),
            Msg::UpdateAnyway => {
                if let Some(id) = self.last_update.clone() {
                    self.start_update(id, true);
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
        let cmd = self.model.inspect(input);
        self.worker.emit(cmd);
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
            detail.set_idle(self.model.busy == Busy::Idle);
            detail
                .busy
                .show(&self.model.busy, self.model.close_requested);
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
            self.ui.busy.pulse();
            if let Some(detail) = &self.detail {
                detail.busy.pulse();
            }
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
            Effect::OfferWithoutCommand { title, body } => {
                self.show_shadowed(&title, &body, sender);
            }
            Effect::OfferUpdateAnyway { body } => self.offer_update_anyway(&body, sender),
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

    fn start_update(&mut self, id: String, accept_risks: bool) {
        if self.model.busy == Busy::Idle {
            let cancel = self.model.start(Op::Update);
            self.last_update = Some(id.clone());
            self.worker.emit(Cmd::Update {
                id,
                cancel,
                accept_risks,
            });
        }
    }

    fn offer_update_anyway(&self, body: &str, sender: &ComponentSender<Self>) {
        let dialog = adw::AlertDialog::new(Some("Update anyway?"), Some(body));
        dialog.add_response("cancel", "Keep current version");
        dialog.add_response("update", "Update anyway");
        dialog.set_response_appearance("update", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let sender = sender.clone();
        dialog.connect_response(Some("update"), move |_, _| {
            sender.input(Msg::UpdateAnyway);
        });
        dialog.present(Some(&self.ui.window));
    }

    fn show_shadowed(&self, title: &str, body: &str, sender: &ComponentSender<Self>) {
        let dialog = adw::AlertDialog::new(Some(title), Some(body));
        dialog.add_response("close", "Close");
        dialog.add_response("without", "Install without command");
        dialog.set_response_appearance("without", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("without"));
        dialog.set_close_response("close");
        let sender = sender.clone();
        dialog.connect_response(Some("without"), move |_, _| {
            sender.input(Msg::InspectWithoutCommand);
        });
        dialog.present(Some(&self.ui.window));
    }

    /// Shows the model's busy state: progress area and input sensitivity.
    fn refresh(&self) {
        let ui = &self.ui;
        let idle = self.model.busy == Busy::Idle;
        ui.entry.set_sensitive(idle);
        ui.inspect.set_sensitive(idle);
        ui.browse.set_sensitive(idle);
        let close_requested = self.model.close_requested;
        ui.busy.show(&self.model.busy, close_requested);
        if let Some(detail) = &self.detail {
            detail.set_idle(idle);
            detail.busy.show(&self.model.busy, close_requested);
        }
    }
}
