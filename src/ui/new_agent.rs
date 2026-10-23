//! The New Agent dialog: pick an agent, sign in, pick a project folder.
//!
//! It replaces a bare "new sandbox" form with the three things OpenShell
//! needs to run an agent usefully: what to run (the agent, installed by the
//! app once the sandbox is up), the credential it may use (an OpenShell
//! provider, whose key the gateway injects only into that agent's requests
//! to its service), and the files it works on (a host folder mounted under
//! the sandbox workspace).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use openshell_agents::{Agent, Launch};
use openshell_gateway_client::codex::CodexTokens;
use openshell_gateway_client::{AGENT_LABEL, CreateOptions, ProfileInfo, ProviderInfo};

use super::dialogs;

/// The credential the new sandbox gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    None,
    Existing(String),
    /// Store this key as a new provider first.
    New {
        name: String,
        kind: String,
        env_var: String,
        key: String,
    },
    /// Store this Codex sign-in as a new provider first.
    Codex {
        name: String,
        tokens: CodexTokens,
    },
}

impl Account {
    fn provider(&self) -> Option<&str> {
        match self {
            Account::None => None,
            Account::Existing(name) | Account::New { name, .. } | Account::Codex { name, .. } => {
                Some(name)
            }
        }
    }
}

/// Everything the window needs to create and launch the agent.
#[derive(Debug, Clone)]
pub struct NewAgent {
    pub name: String,
    pub agent: Option<&'static Agent>,
    pub account: Account,
    /// Host folder to mount at `/sandbox/<its name>` and start the agent in.
    pub folder: Option<PathBuf>,
    pub options: CreateOptions,
}

/// What the dialog shows; read from the window when it opens.
pub struct Context {
    pub profiles: Vec<ProfileInfo>,
    pub providers: Vec<ProviderInfo>,
    pub sandboxes: Vec<String>,
    pub gpu_supported: bool,
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    context: Context,
    done: impl Fn(NewAgent) + 'static,
) {
    let dialog = adw::Dialog::builder()
        .title("New Agent")
        .content_width(480)
        .content_height(560)
        .build();
    let nav = adw::NavigationView::new();
    dialog.set_child(Some(&nav));

    let context = Rc::new(context);
    let done: Rc<dyn Fn(NewAgent)> = Rc::new(done);

    let group = adw::PreferencesGroup::builder()
        .description(
            "The agent runs in its own LXD sandbox. OpenShell decides which files it \
             may touch and which hosts it may reach, and asks you about anything else.",
        )
        .build();
    for agent in openshell_agents::CATALOG {
        let row = nav_row(agent.name, agent.description);
        row.connect_activated(glib::clone!(
            #[weak]
            nav,
            #[strong]
            context,
            #[strong]
            done,
            #[weak]
            dialog,
            move |_| sign_in(&nav, &dialog, &context, agent, done.clone())
        ));
        group.add(&row);
    }
    let other = adw::PreferencesGroup::new();
    let shell = nav_row(
        "Plain Ubuntu Shell",
        "No agent: a sandboxed shell to set up yourself",
    );
    shell.connect_activated(glib::clone!(
        #[weak]
        nav,
        #[strong]
        context,
        #[strong]
        done,
        #[weak]
        dialog,
        move |_| project(&nav, &dialog, &context, None, Account::None, done.clone())
    ));
    other.add(&shell);

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&other);
    nav.add(&navigation_page("Choose an Agent", &page, None));
    dialog.present(Some(parent));
}

fn nav_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .use_markup(false)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

fn navigation_page(
    title: &str,
    content: &adw::PreferencesPage,
    action: Option<&gtk::Button>,
) -> adw::NavigationPage {
    let header = adw::HeaderBar::new();
    if let Some(action) = action {
        header.pack_end(action);
    }
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(content));
    adw::NavigationPage::new(&toolbar, title)
}

/// Second page: which account the agent uses, or a key for a new one.
fn sign_in(
    nav: &adw::NavigationView,
    dialog: &adw::Dialog,
    context: &Rc<Context>,
    agent: &'static Agent,
    done: Rc<dyn Fn(NewAgent)>,
) {
    let profile = agent
        .profiles
        .iter()
        .find_map(|id| context.profiles.iter().find(|p| p.id == *id))
        .cloned();
    let Some(profile) = profile else {
        // The gateway has not imported the profile; without it the key
        // could never be injected, so skip straight on without one.
        project(nav, dialog, context, Some(agent), Account::None, done);
        return;
    };
    if agent.launch == Launch::CodexSignIn {
        codex_sign_in(nav, dialog, context, agent, &profile, done);
        return;
    }
    let existing: Vec<&ProviderInfo> = context
        .providers
        .iter()
        .filter(|p| agent.profiles.contains(&p.kind.as_str()))
        .collect();

    let group = adw::PreferencesGroup::builder()
        .description(format!(
            "{} signs in with an API key. The OpenShell gateway stores it and adds it \
             only to {}'s requests to {}; the agent itself never sees it.",
            agent.name,
            agent.name,
            profile.hosts.join(", ")
        ))
        .build();
    let continue_button = gtk::Button::builder()
        .label("Next")
        .css_classes(["suggested-action"])
        .build();

    // One check button per existing account, plus one for a new key.
    let choice: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let mut first_check: Option<gtk::CheckButton> = None;
    for provider in &existing {
        let check = gtk::CheckButton::new();
        if let Some(first) = &first_check {
            check.set_group(Some(first));
        } else {
            check.set_active(true);
            choice.replace(Some(provider.name.clone()));
            first_check = Some(check.clone());
        }
        let row = adw::ActionRow::builder()
            .title(&provider.name)
            .subtitle("Saved account")
            .activatable_widget(&check)
            .use_markup(false)
            .build();
        row.add_prefix(&check);
        let name = provider.name.clone();
        let choice = choice.clone();
        check.connect_toggled(move |check| {
            if check.is_active() {
                choice.replace(Some(name.clone()));
            }
        });
        group.add(&row);
    }
    let key = adw::PasswordEntryRow::builder()
        .title(match profile.credentials.first() {
            Some(c) if !c.description.is_empty() => c.description.clone(),
            _ => "API key".to_owned(),
        })
        .build();
    if let Some(first) = &first_check {
        // With saved accounts, typing a key selects "new key".
        let new_check = gtk::CheckButton::new();
        new_check.set_group(Some(first));
        key.add_prefix(&new_check);
        let choice = choice.clone();
        new_check.connect_toggled(move |check| {
            if check.is_active() {
                choice.replace(None);
            }
        });
        key.connect_changed(glib::clone!(
            #[weak]
            new_check,
            move |key| {
                if !key.text().is_empty() {
                    new_check.set_active(true);
                }
            }
        ));
    }
    group.add(&key);

    let validate = {
        let (choice, key, button) = (choice.clone(), key.clone(), continue_button.clone());
        move || button.set_sensitive(choice.borrow().is_some() || !key.text().trim().is_empty())
    };
    validate();
    key.connect_changed({
        let validate = validate.clone();
        move |_| validate()
    });
    if let Some(first) = &first_check {
        first.connect_toggled(move |_| validate());
    }

    continue_button.connect_clicked(glib::clone!(
        #[weak]
        nav,
        #[weak]
        dialog,
        #[weak]
        key,
        #[strong]
        context,
        move |_| {
            let account = match choice.borrow().clone() {
                Some(name) => Account::Existing(name),
                None => Account::New {
                    name: unique(&profile.id, |n| {
                        context.providers.iter().any(|p| p.name == n)
                    }),
                    kind: profile.id.clone(),
                    env_var: profile
                        .credentials
                        .first()
                        .and_then(|c| c.env_vars.first())
                        .cloned()
                        .unwrap_or_default(),
                    key: key.text().trim().to_owned(),
                },
            };
            project(&nav, &dialog, &context, Some(agent), account, done.clone());
        }
    ));

    let page = adw::PreferencesPage::new();
    page.add(&group);
    nav.push(&navigation_page(
        &format!("Sign In to {}", profile.display_name),
        &page,
        Some(&continue_button),
    ));
}

/// Codex's second page: a saved account, or a sign-in imported from Codex
/// on this computer. Codex signs in with ChatGPT, so there is no key to paste.
fn codex_sign_in(
    nav: &adw::NavigationView,
    dialog: &adw::Dialog,
    context: &Rc<Context>,
    agent: &'static Agent,
    profile: &ProfileInfo,
    done: Rc<dyn Fn(NewAgent)>,
) {
    let group = adw::PreferencesGroup::builder()
        .description(
            "Codex uses your ChatGPT account. Sign in with \"codex login\" on this \
             computer, then import that sign-in here. The OpenShell gateway keeps the \
             tokens and renews them; Codex in the sandbox only gets stand-ins that \
             work for OpenAI's servers.",
        )
        .build();
    let next = gtk::Button::builder()
        .label("Next")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();

    // The chosen saved account, or the imported sign-in.
    let saved: Rc<RefCell<Option<String>>> = Rc::default();
    let imported: Rc<RefCell<Option<CodexTokens>>> = Rc::default();
    let import_check = gtk::CheckButton::new();
    let accounts: Vec<&ProviderInfo> = context
        .providers
        .iter()
        .filter(|p| p.kind == profile.id)
        .collect();
    for (i, provider) in accounts.iter().enumerate() {
        let check = gtk::CheckButton::new();
        check.set_group(Some(&import_check));
        let row = adw::ActionRow::builder()
            .title(&provider.name)
            .subtitle("Saved account")
            .activatable_widget(&check)
            .use_markup(false)
            .build();
        row.add_prefix(&check);
        let name = provider.name.clone();
        check.connect_toggled(glib::clone!(
            #[strong]
            saved,
            #[weak]
            next,
            move |check| {
                if check.is_active() {
                    saved.replace(Some(name.clone()));
                    next.set_sensitive(true);
                }
            }
        ));
        if i == 0 {
            check.set_active(true);
        }
        group.add(&row);
    }

    let import = adw::ActionRow::builder()
        .title("Import Sign-In")
        .subtitle("Choose the auth.json file in the .codex folder in your home folder")
        .use_markup(false)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose…")
        .valign(gtk::Align::Center)
        .build();
    import.add_suffix(&choose);
    if !accounts.is_empty() {
        import.add_prefix(&import_check);
        import.set_activatable_widget(Some(&import_check));
    }
    import_check.connect_toggled(glib::clone!(
        #[strong]
        saved,
        #[strong]
        imported,
        #[weak]
        next,
        move |check| {
            if check.is_active() {
                saved.replace(None);
                next.set_sensitive(imported.borrow().is_some());
            }
        }
    ));
    choose.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        import,
        #[weak]
        import_check,
        #[weak]
        next,
        #[strong]
        imported,
        move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Import Codex Sign-In")
                .modal(true)
                .initial_file(&gio::File::for_path(codex_auth_file()))
                .build();
            let window = dialog.root().and_downcast::<gtk::Window>();
            let imported = imported.clone();
            chooser.open(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else {
                    return;
                };
                let read = file
                    .load_contents(gio::Cancellable::NONE)
                    .map_err(|e| e.message().to_owned())
                    .and_then(|(bytes, _)| {
                        CodexTokens::from_auth_json(&String::from_utf8_lossy(&bytes))
                    });
                match read {
                    Ok(tokens) => {
                        import.set_subtitle(&format!(
                            "Imported the ChatGPT sign-in for account {}",
                            tokens.account_id
                        ));
                        import.remove_css_class("error");
                        imported.replace(Some(tokens));
                        import_check.set_active(true);
                        next.set_sensitive(true);
                    }
                    Err(error) => {
                        import.set_subtitle(&error);
                        import.add_css_class("error");
                    }
                }
            });
        }
    ));
    group.add(&import);

    let note = adw::PreferencesGroup::builder()
        .description(
            "When the gateway renews the tokens, OpenAI may sign Codex out on this \
             computer. If that happens, run \"codex login\" there again.",
        )
        .build();

    let kind = profile.id.clone();
    next.connect_clicked(glib::clone!(
        #[weak]
        nav,
        #[weak]
        dialog,
        #[strong]
        context,
        move |_| {
            let account = match (saved.borrow().clone(), imported.borrow().clone()) {
                (Some(name), _) => Account::Existing(name),
                (None, Some(tokens)) => Account::Codex {
                    name: unique(&kind, |n| context.providers.iter().any(|p| p.name == n)),
                    tokens,
                },
                (None, None) => return,
            };
            project(&nav, &dialog, &context, Some(agent), account, done.clone());
        }
    ));

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&note);
    nav.push(&navigation_page(
        &format!("Sign In to {}", profile.display_name),
        &page,
        Some(&next),
    ));
}

/// Where `codex login` keeps its sign-in. A strict snap's `HOME` is its own
/// data folder, so prefer the user's real one.
fn codex_auth_file() -> PathBuf {
    let home = std::env::var_os("SNAP_REAL_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(glib::home_dir);
    home.join(".codex").join("auth.json")
}

/// Last page: the project folder, a name, and resources.
fn project(
    nav: &adw::NavigationView,
    dialog: &adw::Dialog,
    context: &Rc<Context>,
    agent: Option<&'static Agent>,
    account: Account,
    done: Rc<dyn Fn(NewAgent)>,
) {
    let folder: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));

    let files = adw::PreferencesGroup::builder()
        .title("Project")
        .description(
            "The folder is mounted read-write under /sandbox, and the agent starts \
             there. It can't see the rest of your files.",
        )
        .build();
    let folder_row = adw::ActionRow::builder()
        .title("Folder")
        .subtitle("None")
        .use_markup(false)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose…")
        .valign(gtk::Align::Center)
        .build();
    folder_row.add_suffix(&choose);
    folder_row.set_activatable_widget(Some(&choose));
    files.add(&folder_row);

    let name = adw::EntryRow::builder().title("Sandbox name").build();
    let default_name = unique(agent.map_or("shell", |a| a.id), |n| {
        context.sandboxes.iter().any(|s| s == n)
    });
    name.set_text(&default_name);
    files.add(&name);

    let advanced = adw::PreferencesGroup::new();
    let expander = adw::ExpanderRow::builder()
        .title("Advanced")
        .subtitle("Image and resource limits")
        .build();
    let image = adw::EntryRow::builder()
        .title("Image (blank for the gateway default)")
        .build();
    let cpus = adw::SpinRow::with_range(0.0, 64.0, 1.0);
    cpus.set_title("CPU limit");
    cpus.set_subtitle("Cores; 0 for no limit");
    let memory = adw::SpinRow::with_range(0.0, 262_144.0, 256.0);
    memory.set_title("Memory limit");
    memory.set_subtitle("MB; 0 for no limit");
    let gpu = adw::SwitchRow::builder()
        .title("GPU passthrough")
        .subtitle(if context.gpu_supported {
            "Pass the GPU configured for the LXD driver into the sandbox"
        } else {
            "No GPU is configured for the LXD driver"
        })
        .sensitive(context.gpu_supported)
        .build();
    expander.add_row(&image);
    expander.add_row(&cpus);
    expander.add_row(&memory);
    expander.add_row(&gpu);
    advanced.add(&expander);

    let create = gtk::Button::builder()
        .label("Create")
        .css_classes(["suggested-action"])
        .build();
    let validate = {
        let (name, create, context) = (name.clone(), create.clone(), context.clone());
        move || {
            let text = name.text();
            let text = text.trim();
            let valid = valid_name(text) && !context.sandboxes.iter().any(|s| s == text);
            create.set_sensitive(valid);
            if valid {
                name.remove_css_class("error");
            } else {
                name.add_css_class("error");
            }
        }
    };
    validate();
    name.connect_changed(move |_| validate());

    // Name the sandbox after the folder until the user names it.
    let suggested = Rc::new(RefCell::new(default_name));
    choose.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        folder_row,
        #[weak]
        name,
        #[strong]
        folder,
        #[strong]
        context,
        move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose the Project Folder")
                .modal(true)
                .build();
            let window = dialog.root().and_downcast::<gtk::Window>();
            let suggested = suggested.clone();
            let folder = folder.clone();
            let context = context.clone();
            chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Some(path) = result.ok().and_then(|file| dialogs::host_path(&file)) else {
                    return;
                };
                folder_row.set_subtitle(&path.to_string_lossy());
                if *name.text() == *suggested.borrow() {
                    let base = path
                        .file_name()
                        .map(|n| sanitize(&n.to_string_lossy()))
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "project".into());
                    let fresh = unique(&base, |n| context.sandboxes.iter().any(|s| s == n));
                    suggested.replace(fresh.clone());
                    name.set_text(&fresh);
                }
                folder.replace(Some(path));
            });
        }
    ));

    create.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        name,
        #[weak]
        image,
        #[weak]
        cpus,
        #[weak]
        memory,
        #[weak]
        gpu,
        move |_| {
            let mut options = CreateOptions {
                image: image.text().trim().to_owned(),
                cpus: cpus.value() as u32,
                memory_mb: memory.value() as u32,
                gpu: gpu.is_active(),
                providers: account.provider().map(str::to_owned).into_iter().collect(),
                ..Default::default()
            };
            if let Some(agent) = agent {
                options
                    .labels
                    .push((AGENT_LABEL.to_owned(), agent.id.to_owned()));
            }
            done(NewAgent {
                name: name.text().trim().to_owned(),
                agent,
                account: account.clone(),
                folder: folder.borrow().clone(),
                options,
            });
            dialog.close();
        }
    ));

    let page = adw::PreferencesPage::new();
    page.add(&files);
    page.add(&advanced);
    nav.push(&navigation_page(
        agent.map_or("Plain Ubuntu Shell", |a| a.name),
        &page,
        Some(&create),
    ));
}

/// Gateway sandbox names: lowercase letters, digits and dashes.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(50).collect()
}

/// `base`, or `base-2`, `base-3`… when `taken` says it is in use.
pub fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !taken(name))
        .expect("an unused name")
}

/// Where a mounted project folder appears in the sandbox.
pub fn mount_path(folder: &std::path::Path) -> String {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "project".into());
    format!("/sandbox/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_gateway_rules() {
        assert!(valid_name("claude-code-2"));
        assert!(!valid_name("Claude"));
        assert!(!valid_name("-x"));
        assert!(!valid_name(""));
        assert_eq!(sanitize("My Project (v2)"), "my-project-v2");
    }

    #[test]
    fn unique_skips_taken_names() {
        let taken = ["claude-code", "claude-code-2"];
        assert_eq!(
            unique("claude-code", |n| taken.contains(&n)),
            "claude-code-3"
        );
        assert_eq!(unique("opencode", |n| taken.contains(&n)), "opencode");
    }

    #[test]
    fn projects_mount_under_the_workspace() {
        assert_eq!(
            mount_path(std::path::Path::new("/home/me/src/app")),
            "/sandbox/app"
        );
    }
}
