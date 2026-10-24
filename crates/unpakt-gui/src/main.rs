use relm4::RelmApp;
use unpakt_gui::APP_ID;
use unpakt_gui::app::App;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    RelmApp::new(APP_ID).run::<App>(());
}
