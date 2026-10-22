//! Terminal windows drawn by the libghostty-vt terminal widget: a shell run
//! through the OpenShell gateway, under the sandbox policy, or the LXD debug
//! console, which runs outside it.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::shell::{Session, SessionEvent};
use crate::terminal::TerminalView;

/// Prefer bash, fall back to sh: not every image ships bash.
const LXD_SHELL: &[&str] = &[
    "/bin/sh",
    "-c",
    "if command -v bash >/dev/null; then exec bash -l; else exec sh -l; fi",
];

/// Gateway shells run the command through `bash -lc`, so bash is there.
const GATEWAY_SHELL: &[&str] = &["/bin/bash", "-l"];

/// A login shell in `sandbox`, under its policy and with its providers.
pub fn open_shell(parent: &impl IsA<gtk::Window>, sandbox: &str) {
    open(
        parent,
        &format!("{sandbox} — Terminal"),
        None,
        |cols, rows| {
            let command = GATEWAY_SHELL.iter().map(|s| s.to_string()).collect();
            Session::gateway(sandbox.to_owned(), command, cols, rows)
        },
    );
}

/// A shell in LXD `instance` itself, for when the sandbox will not start.
pub fn open_debug(parent: &impl IsA<gtk::Window>, instance: &str) {
    open(
        parent,
        &format!("{instance} — Debug Console"),
        Some("Debug console: this shell runs outside OpenShell's sandbox policy"),
        |cols, rows| {
            let command = LXD_SHELL.iter().map(|s| s.to_string()).collect();
            Session::lxd(instance.to_owned(), command, cols, rows)
        },
    );
}

fn open(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    warning: Option<&str>,
    start: impl FnOnce(u16, u16) -> (Session, async_channel::Receiver<SessionEvent>),
) {
    let terminal = TerminalView::new();
    let terminal_handle_focus = terminal.widget().clone();
    let stack = gtk::Stack::new();
    stack.add_named(terminal.widget(), Some("terminal"));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    if let Some(warning) = warning {
        toolbar.add_top_bar(&adw::Banner::builder().title(warning).revealed(true).build());
    }
    toolbar.set_content(Some(&stack));

    let window = adw::Window::builder()
        .title(title)
        .default_width(820)
        .default_height(520)
        .content(&toolbar)
        .build();
    if let Some(app) = parent.as_ref().application() {
        window.set_application(Some(&app));
    }

    let (cols, rows) = terminal.size();
    let (console, events) = start(cols, rows);
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
                SessionEvent::Output(bytes) => terminal.feed(&bytes),
                SessionEvent::Ended(code) => {
                    let detail = code
                        .filter(|&code| code != 0)
                        .map(|code| format!("The command exited with status {code}."));
                    show_status(
                        &stack_weak,
                        &window_weak,
                        "Session Ended",
                        detail.as_deref(),
                        false,
                    );
                    break;
                }
                SessionEvent::Error(error) => {
                    show_status(
                        &stack_weak,
                        &window_weak,
                        "Couldn't Open Terminal",
                        Some(&error),
                        true,
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
    failed: bool,
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
        .icon_name(if failed {
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
