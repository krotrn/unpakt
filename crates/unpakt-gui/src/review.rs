//! The review page shown before an install is confirmed.

use relm4::ComponentSender;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk::{self, gdk, glib};
use unpakt_core::engine::InstallPlan;

use crate::app::{App, Msg};
use crate::state::review_rows;

/// Builds the page. Only its two header buttons can leave it, so the plan is
/// always either installed or discarded.
pub fn page(plan: &InstallPlan, sender: &ComponentSender<App>) -> adw::NavigationPage {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    for (title, value) in review_rows(plan) {
        let row = adw::ActionRow::new();
        row.set_use_markup(false);
        row.set_title(title);
        row.set_subtitle(&value);
        row.set_subtitle_selectable(true);
        list.append(&row);
    }
    for warning in &plan.plan.payload.warnings {
        let row = adw::ActionRow::new();
        row.set_use_markup(false);
        row.set_title(warning);
        row.set_title_lines(0);
        row.add_css_class("warning");
        row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        list.append(&row);
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&icon(plan));
    content.append(&list);

    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .child(&content)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    cancel.connect_clicked({
        let sender = sender.clone();
        move |_| sender.input(Msg::CancelReview)
    });
    let install = gtk::Button::with_label("Install");
    install.add_css_class("suggested-action");
    install.connect_clicked({
        let sender = sender.clone();
        move |_| sender.input(Msg::Confirm)
    });
    let header = adw::HeaderBar::new();
    header.pack_start(&cancel);
    header.pack_end(&install);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));

    let page = adw::NavigationPage::new(&toolbar, "Review");
    page.set_can_pop(false);
    page
}

/// The package's own icon, or a generic one when it has none or the image
/// format cannot be decoded.
fn icon(plan: &InstallPlan) -> gtk::Image {
    let texture = plan
        .plan
        .payload
        .icon
        .as_ref()
        .and_then(|icon| gdk::Texture::from_bytes(&glib::Bytes::from(&icon.data[..])).ok());
    let image = match texture {
        Some(texture) => gtk::Image::from_paintable(Some(&texture)),
        None => gtk::Image::from_icon_name("application-x-executable"),
    };
    image.set_pixel_size(64);
    image
}
