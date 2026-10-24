use relm4::adw;
use relm4::gtk::{gio, prelude::*};
use relm4::{MessageBroker, RelmApp};
use unpakt_gui::APP_ID;
use unpakt_gui::app::{App, Msg};

static BROKER: MessageBroker<Msg> = MessageBroker::new();

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Files opened from a file manager arrive here instead of through activate,
    // so the window has to be shown by hand.
    let app = adw::Application::new(Some(APP_ID), gio::ApplicationFlags::HANDLES_OPEN);
    app.connect_open(|app, files, _hint| {
        if let Some(window) = app.active_window() {
            window.present();
        }
        if let Some(path) = files.first().and_then(|f| f.path()) {
            BROKER.send(Msg::FileChosen(path));
        }
    });

    RelmApp::from_app(app).with_broker(&BROKER).run::<App>(());
}
