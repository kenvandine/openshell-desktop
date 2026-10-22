//! The Accounts page of Preferences: the API keys and tokens agents use.
//!
//! Each account is an OpenShell provider. The gateway stores the secret and
//! the sandbox supervisor injects it only into requests to that service's
//! own hosts, so agents never see the real value.

use std::collections::BTreeMap;

use adw::prelude::*;
use gtk::glib;
use openshell_gateway_client::{ProfileInfo, ProviderInfo};

use super::window::Window;
use crate::gateway::{Command, Credentials};

/// The page and the list the window refills as accounts change.
pub fn page(
    window: &Window,
    dialog: &adw::PreferencesDialog,
) -> (adw::PreferencesPage, gtk::ListBox) {
    let page = adw::PreferencesPage::builder()
        .title("Accounts")
        .icon_name("dialog-password-symbolic")
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("Accounts")
        .description(
            "Keys for the services agents use. The OpenShell gateway stores them and \
             adds them only to requests bound for each service's own hosts, so agents \
             never see them.",
        )
        .build();
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add Account")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    add.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        dialog,
        move |_| choose_service(&window, &dialog)
    ));
    group.set_header_suffix(Some(&add));
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    group.add(&list);
    page.add(&group);
    fill(&list, window, dialog);
    (page, list)
}

/// Rebuilds the account rows from what the window last heard.
pub fn fill(list: &gtk::ListBox, window: &Window, dialog: &adw::PreferencesDialog) {
    list.remove_all();
    let (profiles, providers) = window.accounts();
    if !window.is_connected() {
        list.append(&placeholder(
            "Accounts are stored by the OpenShell gateway, which is not running",
        ));
        return;
    }
    if providers.is_empty() {
        list.append(&placeholder(
            "No accounts yet. Add one to give agents an API key.",
        ));
        return;
    }
    for provider in providers {
        let profile = profiles.iter().find(|p| p.id == provider.kind);
        let service = profile.map_or(provider.kind.as_str(), |p| p.display_name.as_str());
        let row = adw::ActionRow::builder()
            .title(&provider.name)
            .subtitle(match profile {
                Some(profile) if !profile.hosts.is_empty() => {
                    format!("{service} · sent only to {}", profile.hosts.join(", "))
                }
                _ => service.to_owned(),
            })
            .use_markup(false)
            .build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove Account")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let name = provider.name.clone();
        remove.connect_clicked(glib::clone!(
            #[weak]
            window,
            #[weak]
            dialog,
            move |_| confirm_remove(&window, &dialog, &name)
        ));
        row.add_suffix(&remove);
        list.append(&row);
    }
}

fn placeholder(text: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(text)
        .css_classes(["dim-label"])
        .activatable(false)
        .build()
}

fn confirm_remove(window: &Window, dialog: &adw::PreferencesDialog, name: &str) {
    let alert = adw::AlertDialog::new(
        Some(&format!("Remove {name}?")),
        Some(
            "The gateway deletes the stored key. Sandboxes that use this account lose access to its service.",
        ),
    );
    alert.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    alert.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");
    let name = name.to_owned();
    alert.connect_response(
        Some("remove"),
        glib::clone!(
            #[weak]
            window,
            move |_, _| window.send(Command::DeleteProvider(name.clone()))
        ),
    );
    alert.present(Some(dialog));
}

/// First step of adding an account: which service.
fn choose_service(window: &Window, dialog: &adw::PreferencesDialog) {
    if !window.is_connected() {
        dialog.add_toast(adw::Toast::new(
            "Adding an account needs the OpenShell gateway",
        ));
        return;
    }
    let (profiles, _) = window.accounts();
    let group = adw::PreferencesGroup::builder()
        .description("Choose the service the key is for.")
        .build();
    for profile in profiles.into_iter().filter(ProfileInfo::takes_single_key) {
        let row = adw::ActionRow::builder()
            .title(&profile.display_name)
            .subtitle(&profile.description)
            .activatable(true)
            .use_markup(false)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.connect_activated(glib::clone!(
            #[weak]
            window,
            #[weak]
            dialog,
            move |_| enter_key(&window, &dialog, &profile)
        ));
        group.add(&row);
    }
    let content = adw::PreferencesPage::new();
    content.add(&group);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));
    dialog.push_subpage(&adw::NavigationPage::new(&toolbar, "Add Account"));
}

/// Second step: the account name and its key.
fn enter_key(window: &Window, dialog: &adw::PreferencesDialog, profile: &ProfileInfo) {
    let (_, providers) = window.accounts();
    let group = adw::PreferencesGroup::builder()
        .description(format!(
            "The key is sent only to {}.",
            profile.hosts.join(", ")
        ))
        .build();
    let name = adw::EntryRow::builder().title("Account name").build();
    name.set_text(&unique_name(&profile.id, &providers));
    group.add(&name);

    let mut fields = Vec::new();
    for credential in &profile.credentials {
        let Some(key) = credential.env_vars.first() else {
            continue;
        };
        let title = if credential.description.is_empty() {
            key.clone()
        } else {
            credential.description.clone()
        };
        let row = adw::PasswordEntryRow::builder().title(title).build();
        row.set_tooltip_text(Some(key));
        group.add(&row);
        fields.push((key.clone(), credential.required, row));
    }

    let add = gtk::Button::builder()
        .label("Add")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let validate = {
        let name = name.clone();
        let fields = fields.clone();
        let add = add.clone();
        move || {
            let ready = !name.text().trim().is_empty()
                && fields
                    .iter()
                    .all(|(_, required, row)| !required || !row.text().trim().is_empty());
            add.set_sensitive(ready);
        }
    };
    name.connect_changed({
        let validate = validate.clone();
        move |_| validate()
    });
    for (_, _, row) in &fields {
        let validate = validate.clone();
        row.connect_changed(move |_| validate());
    }

    let kind = profile.id.clone();
    add.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        dialog,
        #[weak]
        name,
        move |_| {
            let credentials: BTreeMap<String, String> = fields
                .iter()
                .filter(|(_, _, row)| !row.text().trim().is_empty())
                .map(|(key, _, row)| (key.clone(), row.text().trim().to_owned()))
                .collect();
            window.send(Command::CreateProvider {
                name: name.text().trim().to_owned(),
                kind: kind.clone(),
                credentials: Credentials(credentials),
            });
            // Back past the service list to the accounts.
            dialog.pop_subpage();
            dialog.pop_subpage();
        }
    ));

    let content = adw::PreferencesPage::new();
    content.add(&group);
    let header = adw::HeaderBar::new();
    header.pack_end(&add);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    dialog.push_subpage(&adw::NavigationPage::new(&toolbar, &profile.display_name));
}

/// `base`, or `base-2`, `base-3`… when an account already has that name.
fn unique_name(base: &str, providers: &[ProviderInfo]) -> String {
    let taken = |name: &str| providers.iter().any(|p| p.name == name);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !taken(name))
        .expect("an unused name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> ProviderInfo {
        ProviderInfo {
            name: name.into(),
            kind: "claude-code".into(),
            credential_keys: Vec::new(),
        }
    }

    #[test]
    fn unique_name_skips_taken_names() {
        assert_eq!(unique_name("claude-code", &[]), "claude-code");
        assert_eq!(
            unique_name(
                "claude-code",
                &[provider("claude-code"), provider("claude-code-2")]
            ),
            "claude-code-3"
        );
    }
}
