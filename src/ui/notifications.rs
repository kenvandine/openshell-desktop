//! Desktop notifications for sandbox changes that need attention while the
//! window is in the background. Blocked requests have their own, in the
//! window, since they carry Allow and Ignore buttons.

use gtk::gio;
use gtk::prelude::*;

use crate::model::{Sandbox, SandboxState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Finished creating or starting.
    Ready(String),
    /// Entered the gateway's error phase, with why if it says.
    Failed(String, String),
}

/// What changed between two polls of the same gateway. Sandboxes seen for
/// the first time are left out, so connecting does not announce everything.
pub fn status_changes(old: &[Sandbox], new: &[Sandbox]) -> Vec<Change> {
    new.iter()
        .filter_map(|sandbox| {
            let before = old.iter().find(|o| o.name == sandbox.name)?;
            if sandbox.phase == "error" && before.phase != "error" {
                Some(Change::Failed(
                    sandbox.name.clone(),
                    sandbox.status_detail.clone(),
                ))
            } else if sandbox.state == SandboxState::Running
                && before.state != SandboxState::Running
            {
                Some(Change::Ready(sandbox.name.clone()))
            } else {
                None
            }
        })
        .collect()
}

pub fn send(app: &gio::Application, change: &Change) {
    let (id, notification) = match change {
        Change::Ready(name) => {
            let notification = gio::Notification::new(&format!("{name} is ready"));
            notification.set_body(Some("The sandbox is running."));
            notification.set_default_action("app.show-window");
            notification.add_button_with_target_value(
                "Open Terminal",
                "app.open-terminal",
                Some(&name.to_variant()),
            );
            (format!("status-{name}"), notification)
        }
        Change::Failed(name, detail) => {
            let notification = gio::Notification::new(&format!("{name} failed"));
            if !detail.is_empty() {
                notification.set_body(Some(detail));
            }
            notification.set_priority(gio::NotificationPriority::High);
            notification.set_default_action("app.show-window");
            (format!("status-{name}"), notification)
        }
    };
    app.send_notification(Some(&id), &notification);
}

/// Sent when the gateway stops answering after it had been reachable.
pub fn gateway_lost(app: &gio::Application, reason: &str) {
    let notification = gio::Notification::new("OpenShell gateway stopped");
    notification.set_body(Some(reason));
    notification.set_default_action("app.show-window");
    app.send_notification(Some("gateway"), &notification);
}

pub fn gateway_back(app: &gio::Application) {
    app.withdraw_notification("gateway");
}

/// Sent once, the first time closing the window leaves the app running.
pub fn still_running(app: &gio::Application) {
    let notification = gio::Notification::new("OpenShell Desktop is still running");
    notification.set_body(Some(
        "Sandboxes keep running, and blocked requests still reach you. \
         Open the app again from its icon in the top bar.",
    ));
    notification.set_default_action("app.show-window");
    app.send_notification(Some("still-running"), &notification);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str, state: SandboxState, phase: &str) -> Sandbox {
        Sandbox {
            name: name.into(),
            instance: String::new(),
            runtime: String::new(),
            profile: String::new(),
            state,
            phase: phase.into(),
            status_detail: "image pull failed".into(),
            instance_type: String::new(),
            uptime_mins: None,
            memory_used_mb: 0,
            memory_limit_mb: 0,
            cpu_limit: 0,
            gpu_passthrough: false,
            mounts: Vec::new(),
            rules: Vec::new(),
            prompts: Vec::new(),
            policy: None,
            agent: None,
            auto_approve: Default::default(),
        }
    }

    #[test]
    fn reports_ready_and_failed_transitions_only() {
        let old = [
            sandbox("a", SandboxState::Busy, "provisioning"),
            sandbox("b", SandboxState::Busy, "provisioning"),
            sandbox("c", SandboxState::Running, "ready"),
        ];
        let new = [
            sandbox("a", SandboxState::Running, "ready"),
            sandbox("b", SandboxState::Busy, "error"),
            sandbox("c", SandboxState::Running, "ready"),
            sandbox("d", SandboxState::Running, "ready"),
        ];
        assert_eq!(
            status_changes(&old, &new),
            [
                Change::Ready("a".into()),
                Change::Failed("b".into(), "image pull failed".into())
            ]
        );
    }
}
