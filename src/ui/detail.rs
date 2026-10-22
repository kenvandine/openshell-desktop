//! The right-hand pane: status, Policy Advisor, mounts, network and resources
//! for the selected sandbox, laid out as an `AdwPreferencesPage`.

use adw::prelude::*;

use super::window::Window;
use crate::model::{Access, Sandbox, SandboxState, format_mb};

#[derive(Debug, Clone)]
pub enum SandboxAction {
    OpenTerminal,
    OpenAgent,
    DebugConsole,
    PolicyRules,
    ToggleRunning,
    AllowDomain(usize),
    IgnorePrompt(usize),
    AddMount,
    RemoveMount(usize),
    AddRule,
    RemoveRule(usize),
    TerminateAndPurge,
}

pub fn build(sandbox: &Sandbox, window: &Window) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.add(&status_group(sandbox, window));
    page.add(&advisor_group(sandbox, window));
    page.add(&mounts_group(sandbox, window));
    page.add(&network_group(sandbox, window));
    page.add(&resources_group(sandbox));
    page.add(&troubleshooting_group(sandbox, window));
    page.add(&danger_group(window));
    page
}

fn action_button(label: &str, action: SandboxAction, window: &Window) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.set_valign(gtk::Align::Center);
    let window = window.downgrade();
    button.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            window.handle_action(action.clone());
        }
    });
    button
}

fn icon_button(icon: &str, tooltip: &str, action: SandboxAction, window: &Window) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    let window = window.downgrade();
    button.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            window.handle_action(action.clone());
        }
    });
    button
}

fn status_group(sandbox: &Sandbox, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(format!("Active Sandbox: {}", sandbox.name))
        .description(format!("Profile: {}", sandbox.profile))
        .build();

    let running = sandbox.state == SandboxState::Running;
    let status = match sandbox.state {
        SandboxState::Running => format!("Running (LXD {})", sandbox.instance_type),
        SandboxState::Stopped => "Stopped".to_owned(),
        SandboxState::Busy => capitalize(&sandbox.phase),
    };
    let status = if sandbox.status_detail.is_empty() {
        status
    } else {
        format!("{status}: {}", sandbox.status_detail)
    };
    group.add(
        &adw::ActionRow::builder()
            .title("Status")
            .subtitle(status)
            .subtitle_selectable(true)
            .build(),
    );

    if running {
        let memory = if sandbox.memory_limit_mb > 0 {
            format!(
                "Memory: {} / {}",
                format_mb(sandbox.memory_used_mb),
                format_mb(sandbox.memory_limit_mb)
            )
        } else {
            format!("Memory: {}", format_mb(sandbox.memory_used_mb))
        };
        let usage = adw::ActionRow::builder().title(memory).build();
        if let Some(mins) = sandbox.uptime_mins {
            usage.set_subtitle(&format!("Uptime: {mins}m"));
        }
        if sandbox.memory_limit_mb > 0 {
            let level = gtk::LevelBar::for_interval(0.0, sandbox.memory_limit_mb as f64);
            level.set_value(sandbox.memory_used_mb as f64);
            level.set_valign(gtk::Align::Center);
            level.set_width_request(160);
            usage.add_suffix(&level);
        }
        group.add(&usage);
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(12);
    if let Some(agent) = sandbox.agent.as_deref().and_then(openshell_agents::find) {
        let open = action_button(
            &format!("Open {}", agent.name),
            SandboxAction::OpenAgent,
            window,
        );
        open.add_css_class("suggested-action");
        open.set_sensitive(running && !sandbox.instance.is_empty());
        buttons.append(&open);
    }
    let terminal = action_button("Open Terminal", SandboxAction::OpenTerminal, window);
    terminal.set_sensitive(running);
    buttons.append(&terminal);
    let rules = action_button("Policy Rules", SandboxAction::PolicyRules, window);
    buttons.append(&rules);
    let toggle = action_button(
        if running { "Stop" } else { "Start" },
        SandboxAction::ToggleRunning,
        window,
    );
    toggle.add_css_class(if running {
        "destructive-action"
    } else {
        "suggested-action"
    });
    buttons.append(&toggle);
    group.add(&buttons);
    group
}

fn advisor_group(sandbox: &Sandbox, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Policy Advisor")
        .description("Recent outbound requests blocked by the OpenShell proxy")
        .build();

    if sandbox.prompts.is_empty() {
        group.add(
            &adw::ActionRow::builder()
                .title("No blocked requests")
                .build(),
        );
    }
    for (i, prompt) in sandbox.prompts.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(format!("Blocked: {} {}", prompt.method, prompt.url).replace("  ", " "))
            .subtitle(format!("Requested by: {}", prompt.binary))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        let allow = action_button("Allow Domain", SandboxAction::AllowDomain(i), window);
        allow.add_css_class("suggested-action");
        row.add_suffix(&allow);
        row.add_suffix(&action_button(
            "Ignore",
            SandboxAction::IgnorePrompt(i),
            window,
        ));
        group.add(&row);
    }
    group
}

fn mounts_group(sandbox: &Sandbox, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Filesystem Mounts")
        .description("Host folders shared with the sandbox")
        .build();
    group.set_header_suffix(Some(&icon_button(
        "list-add-symbolic",
        "Add mount",
        SandboxAction::AddMount,
        window,
    )));

    if sandbox.mounts.is_empty() {
        group.add(
            &adw::ActionRow::builder()
                .title("No host folders mounted")
                .build(),
        );
    }
    for (i, mount) in sandbox.mounts.iter().enumerate() {
        let icon = match mount.access {
            Access::ReadWrite => "folder-symbolic",
            Access::ReadOnly => "changes-prevent-symbolic",
        };
        let row = adw::ActionRow::builder()
            .title(&mount.host)
            .subtitle(format!("→ {}", mount.sandbox))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        let access = gtk::Label::new(Some(mount.access.short_label()));
        access.add_css_class("dim-label");
        row.add_suffix(&access);
        row.add_suffix(&icon_button(
            "user-trash-symbolic",
            "Remove mount",
            SandboxAction::RemoveMount(i),
            window,
        ));
        group.add(&row);
    }
    group
}

fn network_group(sandbox: &Sandbox, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Network Rules")
        .description("Allowed egress destinations")
        .build();
    group.set_header_suffix(Some(&icon_button(
        "list-add-symbolic",
        "Add rule",
        SandboxAction::AddRule,
        window,
    )));

    if sandbox.rules.is_empty() {
        group.add(
            &adw::ActionRow::builder()
                .title("All network egress blocked")
                .build(),
        );
    }
    for (i, rule) in sandbox.rules.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(format!("{}:{}", rule.host, rule.port))
            .subtitle(&rule.rule)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("object-select-symbolic"));
        row.add_suffix(&icon_button(
            "user-trash-symbolic",
            "Remove rule",
            SandboxAction::RemoveRule(i),
            window,
        ));
        group.add(&row);
    }
    group
}

/// Limits are part of the sandbox's spec, which the gateway fixes at creation.
fn resources_group(sandbox: &Sandbox) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Resources")
        .description("Set when the sandbox is created")
        .build();
    let row = |title: &str, value: String| {
        let row = adw::ActionRow::builder().title(title).build();
        let label = gtk::Label::new(Some(&value));
        label.add_css_class("dim-label");
        row.add_suffix(&label);
        row
    };
    group.add(&row(
        "CPU limit",
        match sandbox.cpu_limit {
            0 => "No limit".to_owned(),
            1 => "1 core".to_owned(),
            n => format!("{n} cores"),
        },
    ));
    group.add(&row(
        "Memory limit",
        match sandbox.memory_limit_mb {
            0 => "No limit".to_owned(),
            mb => format_mb(mb),
        },
    ));
    group.add(&row(
        "GPU passthrough",
        if sandbox.gpu_passthrough { "On" } else { "Off" }.to_owned(),
    ));
    group
}

fn troubleshooting_group(sandbox: &Sandbox, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Troubleshooting")
        .build();
    let row = adw::ActionRow::builder()
        .title("Debug Console")
        .subtitle(
            "A shell in the LXD instance itself, outside the sandbox policy and \
             without its credentials",
        )
        .build();
    let open = action_button("Open", SandboxAction::DebugConsole, window);
    open.set_sensitive(sandbox.state == SandboxState::Running && !sandbox.instance.is_empty());
    row.add_suffix(&open);
    group.add(&row);
    group
}

fn danger_group(window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let purge = action_button(
        "Terminate & Purge",
        SandboxAction::TerminateAndPurge,
        window,
    );
    purge.add_css_class("destructive-action");
    purge.set_halign(gtk::Align::Center);
    group.add(&purge);
    group
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
