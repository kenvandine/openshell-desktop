//! Terminal windows drawn by the libghostty-vt terminal widget: a shell run
//! through the OpenShell gateway, under the sandbox policy, or the LXD debug
//! console, which runs outside it.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use openshell_agents::Agent;
use openshell_gateway_client::ExecOptions;

use super::approvals::ApprovalBar;
use super::window::Window;
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
pub fn open_shell(parent: &Window, sandbox: &str) {
    open(
        parent,
        &format!("{sandbox} — Terminal"),
        Kind::Gateway(sandbox),
        |cols, rows| {
            let options = ExecOptions {
                command: GATEWAY_SHELL.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            };
            Session::gateway(sandbox.to_owned(), options, cols, rows)
        },
    );
}

/// `agent` in `sandbox`, started in `workdir` (blank for the sandbox's own),
/// signed in to ChatGPT account `account_id` if it is Codex.
pub fn open_agent(
    parent: &Window,
    sandbox: &str,
    agent: &Agent,
    workdir: &str,
    account_id: Option<&str>,
) {
    let options = ExecOptions {
        command: agent.command(account_id),
        workdir: workdir.to_owned(),
        environment: agent
            .environment
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    open(
        parent,
        &format!("{sandbox} — {}", agent.name),
        Kind::Gateway(sandbox),
        |cols, rows| Session::gateway(sandbox.to_owned(), options, cols, rows),
    );
}

/// A shell in LXD `instance` itself, for when the sandbox will not start.
pub fn open_debug(parent: &Window, instance: &str) {
    open(
        parent,
        &format!("{instance} — Debug Console"),
        Kind::Debug,
        |cols, rows| {
            let command = LXD_SHELL.iter().map(|s| s.to_string()).collect();
            Session::lxd(instance.to_owned(), command, cols, rows)
        },
    );
}

/// Where a terminal's session runs, which decides what sits above it.
enum Kind<'a> {
    /// Through the gateway in this sandbox, whose blocked requests it shows.
    Gateway(&'a str),
    /// In the LXD instance, outside the policy, which it warns about.
    Debug,
}

fn open(
    parent: &Window,
    title: &str,
    kind: Kind,
    start: impl FnOnce(u16, u16) -> (Session, async_channel::Receiver<SessionEvent>),
) {
    let terminal = TerminalView::new();
    let terminal_handle_focus = terminal.widget().clone();
    let stack = gtk::Stack::new();
    stack.add_named(terminal.widget(), Some("terminal"));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    if let Kind::Debug = kind {
        let warning = "Debug console: this shell runs outside OpenShell's sandbox policy";
        toolbar.add_top_bar(&adw::Banner::builder().title(warning).revealed(true).build());
    }
    toolbar.set_content(Some(&stack));
    // Once the session ends its output stays, since that is often where the
    // reason is, with this above it.
    let ended = adw::Banner::builder().button_label("Close").build();
    toolbar.add_top_bar(&ended);

    let window = adw::Window::builder()
        .title(title)
        .default_width(820)
        .default_height(520)
        .content(&toolbar)
        .build();
    if let Some(app) = parent.application() {
        window.set_application(Some(&app));
    }
    if let Kind::Gateway(sandbox) = kind {
        let bar = ApprovalBar::new(parent, &window, sandbox);
        toolbar.add_top_bar(bar.widget());
        parent.add_approval_bar(bar);
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
    let close = window.downgrade();
    ended.connect_button_clicked(move |_| {
        if let Some(window) = close.upgrade() {
            window.close();
        }
    });
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::Output(bytes) => terminal.feed(&bytes),
                SessionEvent::Ended(code) => {
                    ended.set_title(&ended_title(code));
                    ended.set_revealed(true);
                    break;
                }
                SessionEvent::Error(error) => {
                    show_status(
                        &stack_weak,
                        &window_weak,
                        "Couldn't Open Terminal",
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

fn ended_title(code: Option<i32>) -> String {
    match code {
        Some(code) if code != 0 => format!("Session ended: the command exited with status {code}"),
        _ => "Session ended".to_owned(),
    }
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
        .icon_name("dialog-error-symbolic")
        .title(title)
        .child(&close)
        .build();
    status.set_description(description);
    stack.add_named(&status, Some("status"));
    stack.set_visible_child(&status);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_the_session_ended() {
        assert_eq!(ended_title(Some(0)), "Session ended");
        assert_eq!(ended_title(None), "Session ended");
        assert_eq!(
            ended_title(Some(1)),
            "Session ended: the command exited with status 1"
        );
    }
}
