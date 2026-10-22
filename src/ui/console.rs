//! Debug console window: a shell inside an LXD instance, opened through
//! LXD's REST exec API and drawn by the libghostty-vt terminal widget.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use openshell_lxd::Client;
use openshell_lxd::console::{Console, ConsoleEvent};

use crate::terminal::TerminalView;

/// Prefer bash, fall back to sh: not every image ships bash.
const SHELL: &[&str] = &[
    "/bin/sh",
    "-c",
    "if command -v bash >/dev/null; then exec bash -l; else exec sh -l; fi",
];

pub fn open(parent: &impl IsA<gtk::Window>, instance: &str) {
    let terminal = TerminalView::new();
    let terminal_handle_focus = terminal.widget().clone();
    let stack = gtk::Stack::new();
    stack.add_named(terminal.widget(), Some("terminal"));

    let banner = adw::Banner::builder()
        .title("Debug console: this shell runs outside OpenShell's sandbox policy")
        .revealed(true)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));

    let window = adw::Window::builder()
        .title(format!("{instance} — Console"))
        .default_width(820)
        .default_height(520)
        .content(&toolbar)
        .build();
    if let Some(app) = parent.as_ref().application() {
        window.set_application(Some(&app));
    }

    let (cols, rows) = terminal.size();
    let command = SHELL.iter().map(|s| s.to_string()).collect();
    let (console, events) =
        Console::spawn(Client::default(), instance.to_owned(), command, cols, rows);
    let console = Rc::new(console);

    let input = console.clone();
    terminal.connect_input(move |bytes| input.write(bytes));
    let resize = console.clone();
    terminal.connect_resize(move |cols, rows| resize.resize(cols, rows));

    let terminal_handle = terminal.clone();
    let stack_weak = stack.downgrade();
    let window_weak = window.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            match event {
                ConsoleEvent::Output(bytes) => terminal.feed(&bytes),
                ConsoleEvent::Closed => {
                    show_status(&stack_weak, &window_weak, "Session Ended", None);
                    break;
                }
                ConsoleEvent::Error(error) => {
                    show_status(
                        &stack_weak,
                        &window_weak,
                        "Couldn't Open Console",
                        Some(&error),
                    );
                    break;
                }
            }
        }
    });

    // Only the terminal's handlers hold the console. Dropping them on close
    // hangs up the session, which also ends the event loop above.
    drop(console);
    let handlers = terminal_handle;
    window.connect_close_request(move |_| {
        handlers.connect_input(|_| {});
        handlers.connect_resize(|_, _| {});
        glib::Propagation::Proceed
    });
    window.present();
    terminal_handle_focus.grab_focus();
}

fn show_status(
    stack: &glib::WeakRef<gtk::Stack>,
    window: &glib::WeakRef<adw::Window>,
    title: &str,
    description: Option<&str>,
) {
    let (Some(stack), Some(window)) = (stack.upgrade(), window.upgrade()) else {
        return;
    };
    let close = gtk::Button::builder()
        .label("Close")
        .halign(gtk::Align::Center)
        .build();
    close.connect_clicked(move |_| window.close());

    let status = adw::StatusPage::builder()
        .icon_name(if description.is_some() {
            "dialog-error-symbolic"
        } else {
            "utilities-terminal-symbolic"
        })
        .title(title)
        .child(&close)
        .build();
    status.set_description(description);
    stack.add_named(&status, Some("status"));
    stack.set_visible_child(&status);
}
