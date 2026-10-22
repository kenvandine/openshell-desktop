mod gateway;
mod indicator;
mod install;
mod model;
mod settings;
mod shell;
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
    // Target of desktop notifications: bring the window forward.
    let show_window = gtk::gio::ActionEntry::builder("show-window")
        .activate(|app: &adw::Application, _, _| app.activate())
        .build();
    // Targets of a blocked-request notification's buttons, with the sandbox
    // name and draft chunk ID as parameter.
    // Target of a sandbox-ready notification's button, with the sandbox name.
    let open_terminal = gtk::gio::ActionEntry::builder("open-terminal")
        .parameter_type(Some(&String::static_variant_type()))
        .activate(|app: &adw::Application, _, target| {
            let Some(sandbox) = target.and_then(|t| t.get::<String>()) else {
                return;
            };
            if let Some(window) = app.active_window().and_downcast::<ui::window::Window>() {
                ui::console::open_shell(&window, &sandbox);
            }
        })
        .build();
    let answer = |name: &str, allow: bool| {
        gtk::gio::ActionEntry::builder(name)
            .parameter_type(Some(&<(String, String)>::static_variant_type()))
            .activate(move |app: &adw::Application, _, target| {
                let Some((sandbox, chunk_id)) = target.and_then(|t| t.get::<(String, String)>())
                else {
                    return;
                };
                app.activate();
                if let Some(window) = app.active_window().and_downcast::<ui::window::Window>() {
                    window.answer_prompt(&sandbox, &chunk_id, allow);
                }
            })
            .build()
    };
    app.add_action_entries([
        about,
        quit,
        show_window,
        open_terminal,
        answer("allow-request", true),
        answer("ignore-request", false),
    ]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
}
