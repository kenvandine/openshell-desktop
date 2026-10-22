//! The right-hand pane: status, Policy Advisor, mounts, network and resources
//! for the selected sandbox, laid out as an `AdwPreferencesPage`.

use adw::prelude::*;

use super::window::Window;
use crate::model::{Access, Sandbox, SandboxState, format_mb};

#[derive(Debug, Clone)]
pub enum SandboxAction {
    OpenTerminal,
    PolicyRules,
    ToggleRunning,
    AllowDomain(usize),
    IgnorePrompt(usize),
    AddMount,
    AddRule,
    TerminateAndPurge,
}

pub fn build(sandbox: &Sandbox, window: &Window) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.add(&status_group(sandbox, window));
    page.add(&advisor_group(sandbox, window));
    page.add(&mounts_group(sandbox, window));
    page.add(&network_group(sandbox, window));
    page.add(&resources_group(sandbox));
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
    group.add(
        &adw::ActionRow::builder()
            .title("Status")
            .subtitle(status)
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
            .title(format!("Blocked: {} {}", prompt.method, prompt.url))
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
        .build();
    group.set_header_suffix(Some(&icon_button(
        "list-add-symbolic",
        "Add mount",
        SandboxAction::AddMount,
        window,
    )));

    for mount in &sandbox.mounts {
        let icon = match mount.access {
            Access::ReadWrite => "folder-symbolic",
            Access::ReadOnly => "changes-prevent-symbolic",
            Access::Hidden => "view-conceal-symbolic",
        };
        let row = adw::ActionRow::builder()
            .title(&mount.host)
            .subtitle(format!("→ {}", mount.sandbox))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        let access = gtk::Label::new(Some(mount.access.short_label()));
        access.add_css_class("dim-label");
        row.add_suffix(&access);
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
    for rule in &sandbox.rules {
        let row = adw::ActionRow::builder()
            .title(format!("{}:{}", rule.host, rule.port))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("object-select-symbolic"));
        group.add(&row);
    }
    group
}

fn resources_group(sandbox: &Sandbox) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Resources").build();

    let cpu = adw::SpinRow::with_range(1.0, 64.0, 1.0);
    cpu.set_title("CPU limit");
    cpu.set_subtitle("limits.cpu");
    cpu.set_value(sandbox.cpu_limit as f64);
    group.add(&cpu);

    let memory = adw::SpinRow::with_range(256.0, 65536.0, 256.0);
    memory.set_title("Memory limit (MB)");
    memory.set_subtitle("limits.memory");
    memory.set_value(sandbox.memory_limit_mb as f64);
    group.add(&memory);

    let gpu = adw::SwitchRow::builder()
        .title("GPU passthrough")
        .subtitle("Pass host GPU (nvidia.com/gpu or /dev/dri) into the sandbox")
        .active(sandbox.gpu_passthrough)
        .build();
    group.add(&gpu);
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
