use relm4::adw;
use relm4::adw::prelude::*;
use unpakt_gui::APP_ID;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        adw::ApplicationWindow::builder()
            .application(app)
            .title("unpakt")
            .default_width(480)
            .default_height(640)
            .build()
            .present();
    });
    app.run();
}
