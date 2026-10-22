//! The bar above a gateway terminal that asks about requests the sandbox's
//! policy blocked, so the user can answer without leaving the agent.
//! AdwBanner has room for one button, and this needs Allow and Deny.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::window::Window;
use crate::model::AdvisorPrompt;

#[derive(Clone)]
pub struct ApprovalBar {
    sandbox: String,
    revealer: gtk::Revealer,
    title: gtk::Label,
    /// The terminal window the bar sits in; once it is gone, so is the bar.
    terminal: glib::WeakRef<adw::Window>,
    /// The request the buttons answer.
    shown: Rc<RefCell<Option<String>>>,
}

impl ApprovalBar {
    pub fn new(window: &Window, terminal: &adw::Window, sandbox: &str) -> Self {
        let title = gtk::Label::builder()
            .hexpand(true)
            .xalign(0.0)
            .wrap(true)
            .build();
        let review = gtk::Button::builder()
            .label("Review…")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let deny = gtk::Button::builder()
            .label("Deny")
            .valign(gtk::Align::Center)
            .build();
        let allow = gtk::Button::builder()
            .label("Allow")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();

        let content = gtk::Box::builder()
            .spacing(6)
            .css_classes(["approval-bar"])
            .build();
        content.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        content.append(&title);
        content.append(&review);
        content.append(&deny);
        content.append(&allow);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&content)
            .build();

        let bar = Self {
            sandbox: sandbox.to_owned(),
            revealer,
            title,
            terminal: terminal.downgrade(),
            shown: Rc::default(),
        };
        for (button, allow) in [(&allow, true), (&deny, false)] {
            let window = window.downgrade();
            let bar = bar.clone();
            button.connect_clicked(move |_| {
                let chunk = bar.shown.borrow().clone();
                if let (Some(window), Some(chunk)) = (window.upgrade(), chunk) {
                    window.answer_prompt(&bar.sandbox, &chunk, allow);
                    // Hide until the gateway confirms; the next request, if
                    // any, comes back with the refresh.
                    bar.revealer.set_reveal_child(false);
                }
            });
        }
        let weak = window.downgrade();
        let name = sandbox.to_owned();
        review.connect_clicked(move |_| {
            if let Some(window) = weak.upgrade() {
                window.review(&name);
            }
        });
        bar
    }

    pub fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    pub fn sandbox(&self) -> &str {
        &self.sandbox
    }

    /// Whether the terminal window it belongs to is still open.
    pub fn is_open(&self) -> bool {
        self.terminal.upgrade().is_some()
    }

    /// Shows the oldest of `prompts`, or hides when there are none.
    pub fn update(&self, prompts: &[AdvisorPrompt]) {
        let Some(first) = prompts.first() else {
            self.shown.replace(None);
            self.revealer.set_reveal_child(false);
            return;
        };
        self.title.set_label(&describe(first, prompts.len() - 1));
        self.shown.replace(Some(first.chunk_id.clone()));
        self.revealer.set_reveal_child(true);
    }
}

/// "claude wants to reach api.example.com:443", naming the program by its
/// file name since the full path rarely helps.
fn describe(prompt: &AdvisorPrompt, more: usize) -> String {
    let program = prompt
        .binary
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("A program");
    let mut text = format!("{program} wants to reach {}:{}", prompt.host, prompt.port);
    match more {
        0 => {}
        1 => text.push_str(" (1 more waiting)"),
        n => text.push_str(&format!(" ({n} more waiting)")),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(binary: &str) -> AdvisorPrompt {
        AdvisorPrompt {
            method: String::new(),
            url: String::new(),
            host: "registry.npmjs.org".into(),
            port: 443,
            binary: binary.into(),
            chunk_id: "c1".into(),
            review_token: String::new(),
        }
    }

    #[test]
    fn names_the_program_and_what_else_waits() {
        assert_eq!(
            describe(&prompt("/usr/bin/npm"), 0),
            "npm wants to reach registry.npmjs.org:443"
        );
        assert_eq!(
            describe(&prompt(""), 2),
            "A program wants to reach registry.npmjs.org:443 (2 more waiting)"
        );
    }
}
