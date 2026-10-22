//! The Policy Rules dialog: a sandbox's whole OpenShell policy, with its
//! network rules editable. The gateway fixes everything else at creation.

use adw::prelude::*;
use openshell_gateway_client::PolicyView;

use super::window::Window;

/// Something the user asked to change in the policy.
#[derive(Debug, Clone)]
pub enum PolicyEdit {
    AddRule,
    RemoveRule(String),
    RemoveEndpoint {
        rule: String,
        host: String,
        port: u16,
    },
}

pub fn dialog(sandbox: &str) -> adw::PreferencesDialog {
    adw::PreferencesDialog::builder()
        .title(format!("{sandbox} Policy"))
        .content_width(560)
        .search_enabled(false)
        .build()
}

/// Replaces the dialog's page with one for `policy`.
pub fn show(dialog: &adw::PreferencesDialog, policy: Option<&PolicyView>, window: &Window) {
    if let Some(old) = dialog.visible_page() {
        dialog.remove(&old.downcast::<adw::PreferencesPage>().expect("a page"));
    }
    dialog.add(&page(policy, window));
}

fn page(policy: Option<&PolicyView>, window: &Window) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let Some(policy) = policy else {
        let group = adw::PreferencesGroup::new();
        group.add(
            &adw::ActionRow::builder()
                .title("No policy yet")
                .subtitle("The gateway reports a policy once the sandbox is provisioned")
                .build(),
        );
        page.add(&group);
        return page;
    };
    page.set_description(&format!(
        "Version {}. Only network rules can change after the sandbox is created.",
        policy.version
    ));
    page.add(&network_group(policy, window));
    page.add(&filesystem_group(policy));
    page.add(&process_group(policy));
    page
}

fn edit_button(icon: &str, tooltip: &str, edit: PolicyEdit, window: &Window) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    let window = window.downgrade();
    button.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            window.edit_policy(edit.clone());
        }
    });
    button
}

fn network_group(policy: &PolicyView, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Network Rules")
        .description("Destinations the sandbox may connect to, and which programs may")
        .build();
    group.set_header_suffix(Some(&edit_button(
        "list-add-symbolic",
        "Add rule",
        PolicyEdit::AddRule,
        window,
    )));
    if policy.rules.is_empty() {
        group.add(
            &adw::ActionRow::builder()
                .title("All network egress blocked")
                .build(),
        );
    }
    for rule in &policy.rules {
        let binaries = if rule.binaries.is_empty() {
            "Any program".to_owned()
        } else {
            rule.binaries.join(", ")
        };
        let expander = adw::ExpanderRow::builder()
            .title(&rule.name)
            .subtitle(binaries)
            .build();
        expander.add_suffix(&edit_button(
            "user-trash-symbolic",
            "Remove rule",
            PolicyEdit::RemoveRule(rule.name.clone()),
            window,
        ));
        for endpoint in &rule.endpoints {
            let row = adw::ActionRow::builder()
                .title(format!("{}:{}", endpoint.host, endpoint.port))
                .build();
            row.add_suffix(&edit_button(
                "list-remove-symbolic",
                "Remove destination",
                PolicyEdit::RemoveEndpoint {
                    rule: rule.name.clone(),
                    host: endpoint.host.clone(),
                    port: endpoint.port,
                },
                window,
            ));
            expander.add_row(&row);
        }
        group.add(&expander);
    }
    group
}

fn filesystem_group(policy: &PolicyView) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Filesystem")
        .description("Paths the sandbox can reach; everything else is hidden by Landlock")
        .build();
    group.add(&value_row(
        "Workspace",
        if policy.include_workdir {
            "Read-write"
        } else {
            "Not included"
        },
    ));
    for path in &policy.read_write {
        group.add(&value_row(path, "Read-write"));
    }
    for path in &policy.read_only {
        group.add(&value_row(path, "Read-only"));
    }
    group
}

fn process_group(policy: &PolicyView) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Process").build();
    let or_default = |value: &str| {
        if value.is_empty() {
            "Image default".to_owned()
        } else {
            value.to_owned()
        }
    };
    group.add(&value_row("Run as user", &or_default(&policy.run_as_user)));
    group.add(&value_row(
        "Run as group",
        &or_default(&policy.run_as_group),
    ));
    group.add(&value_row(
        "Landlock",
        &if policy.landlock.is_empty() {
            "Default".to_owned()
        } else {
            policy.landlock.replace('_', " ")
        },
    ));
    group
}

fn value_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::Label::new(Some(value));
    label.add_css_class("dim-label");
    row.add_suffix(&label);
    row
}
