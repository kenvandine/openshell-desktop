mod gateway;
mod model;
mod terminal;
mod ui;

use adw::prelude::*;

const APP_ID: &str = "io.github.kenvandine.OpenShellDesktop";

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|app| {
        load_css();
        install_actions(app);
    });
    app.connect_activate(|app| {
        let window = app
            .active_window()
            .unwrap_or_else(|| ui::window::Window::new(app).upcast());
        window.present();
    });

    app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../data/style.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn install_actions(app: &adw::Application) {
    let about = gtk::gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| {
            let dialog = adw::AboutDialog::builder()
                .application_name("OpenShell Manager")
                .application_icon("utilities-terminal")
                .version(env!("CARGO_PKG_VERSION"))
                .website(env!("CARGO_PKG_REPOSITORY"))
                .license_type(gtk::License::Apache20)
                .comments("Configure, launch and monitor NVIDIA OpenShell sandboxes on LXD")
                .build();
            dialog.present(app.active_window().as_ref());
        })
        .build();
    let quit = gtk::gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build();
    app.add_action_entries([about, quit]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
}
