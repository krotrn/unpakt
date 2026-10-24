//! The installed-apps list and the per-app detail page.

use relm4::ComponentSender;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use unpakt_core::manifest::AppRecord;

use crate::app::{App, Msg};
use crate::state::{AppRow, Busy};

/// The label, progress bar and Cancel button shown while an operation runs.
/// The root page and the detail page each own one.
pub struct BusyArea {
    pub root: gtk::Box,
    label: gtk::Label,
    progress: gtk::ProgressBar,
    cancel: gtk::Button,
}

impl BusyArea {
    pub fn new(sender: &ComponentSender<App>) -> Self {
        let label = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let progress = gtk::ProgressBar::new();
        progress.set_pulse_step(0.1);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.set_halign(gtk::Align::End);
        cancel.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(Msg::CancelOp)
        });
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_visible(false);
        root.append(&label);
        root.append(&progress);
        root.append(&cancel);
        BusyArea {
            root,
            label,
            progress,
            cancel,
        }
    }

    /// Shows `busy`: hidden when idle, otherwise its label and progress, with
    /// Cancel only when the operation can be cancelled.
    pub fn show(&self, busy: &Busy, close_requested: bool) {
        self.root.set_visible(*busy != Busy::Idle);
        if let Busy::Working {
            label, fraction, ..
        } = busy
        {
            self.label.set_label(label);
            if let Some(share) = fraction {
                self.progress.set_fraction(*share);
            }
        }
        self.cancel.set_visible(busy.cancellable());
        self.cancel
            .set_sensitive(busy.cancellable() && !close_requested);
    }

    pub fn pulse(&self) {
        self.progress.pulse();
    }
}

/// The list group, or a status page when nothing is installed.
pub fn list(rows: &[AppRow], sender: &ComponentSender<App>) -> gtk::Widget {
    if rows.is_empty() {
        let status = adw::StatusPage::builder()
            .title("No applications installed yet")
            .icon_name("application-x-executable-symbolic")
            .build();
        return status.upcast();
    }
    let group = adw::PreferencesGroup::builder()
        .title("Installed applications")
        .build();
    for row in rows {
        let item = adw::ActionRow::new();
        item.set_use_markup(false);
        item.set_title(&row.name);
        item.set_subtitle(&format!("{} · {}", row.version, row.format));
        item.set_activatable(true);
        if row.update.is_some() {
            item.add_suffix(&update_pill());
        }
        item.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let id = row.id.clone();
        item.connect_activated({
            let sender = sender.clone();
            move |_| sender.input(Msg::OpenApp(id.clone()))
        });
        group.add(&item);
    }
    group.upcast()
}

fn update_pill() -> gtk::Label {
    let pill = gtk::Label::new(Some("Update available"));
    pill.add_css_class("caption");
    pill.add_css_class("accent");
    pill.set_valign(gtk::Align::Center);
    pill
}

/// The detail page of one installed app. Its widgets are updated in place
/// when the list changes.
pub struct Detail {
    pub id: String,
    pub page: adw::NavigationPage,
    version: adw::ActionRow,
    update: gtk::Button,
    actions: gtk::Box,
    pub busy: BusyArea,
}

impl Detail {
    pub fn new(app: &AppRecord, has_update: bool, sender: &ComponentSender<App>) -> Self {
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let row = |title: &str, value: &str| {
            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            row.set_title(title);
            row.set_subtitle(value);
            row.set_subtitle_selectable(true);
            list.append(&row);
            row
        };
        row("Name", &app.name);
        let version = row("Version", &app.current);
        row("Format", app.format.as_str());
        row("Source", &app.source.location);

        let update = gtk::Button::with_label("Update");
        update.connect_clicked({
            let sender = sender.clone();
            let id = app.id.clone();
            move |_| sender.input(Msg::UpdateApp(id.clone()))
        });
        let rollback = gtk::Button::with_label("Rollback");
        rollback.connect_clicked({
            let sender = sender.clone();
            let id = app.id.clone();
            move |_| sender.input(Msg::RollbackApp(id.clone()))
        });
        let remove = gtk::Button::with_label("Remove");
        remove.add_css_class("destructive-action");
        remove.connect_clicked({
            let sender = sender.clone();
            let id = app.id.clone();
            move |_| sender.input(Msg::RemoveClicked(id.clone()))
        });
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        actions.append(&update);
        actions.append(&rollback);
        actions.append(&remove);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(16);
        content.set_margin_end(16);
        let busy = BusyArea::new(sender);
        content.append(&list);
        content.append(&busy.root);
        content.append(&actions);
        let clamp = adw::Clamp::builder()
            .maximum_size(560)
            .child(&content)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroll));

        let page = adw::NavigationPage::new(&toolbar, &app.name);
        page.connect_hidden({
            let sender = sender.clone();
            move |_| sender.input(Msg::DetailClosed)
        });

        let detail = Detail {
            id: app.id.clone(),
            page,
            version,
            update,
            actions,
            busy,
        };
        detail.set_update_known(has_update);
        detail
    }

    /// Shows the installed version and whether an update is known.
    pub fn refresh(&self, app: &AppRecord, has_update: bool) {
        self.version.set_subtitle(&app.current);
        self.set_update_known(has_update);
    }

    /// The buttons are usable only while no operation runs.
    pub fn set_idle(&self, idle: bool) {
        self.actions.set_sensitive(idle);
    }

    fn set_update_known(&self, known: bool) {
        if known {
            self.update.add_css_class("suggested-action");
        } else {
            self.update.remove_css_class("suggested-action");
        }
    }
}

/// Asks for confirmation before removing `name`. Cancel is the default.
pub fn confirm_remove(
    window: &adw::ApplicationWindow,
    id: &str,
    name: &str,
    sender: &ComponentSender<App>,
) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Remove {name}?")),
        Some("The application and its launcher entry are removed."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let id = id.to_string();
    let sender = sender.clone();
    dialog.connect_response(Some("remove"), move |_, _| {
        sender.input(Msg::RemoveApp(id.clone()))
    });
    dialog.present(Some(window));
}
