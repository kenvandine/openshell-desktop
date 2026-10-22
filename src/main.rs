mod backend;
mod model;
mod ui;

use relm4::RelmApp;

const APP_ID: &str = "io.github.kenvandine.OpenShellDesktop";

fn main() {
    let app = RelmApp::new(APP_ID);
    relm4::set_global_css(include_str!("../data/style.css"));
    app.run::<ui::window::App>(());
}
