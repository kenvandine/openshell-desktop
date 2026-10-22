use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::console;
use super::detail::{self, SandboxAction};
use super::dialogs;
use super::policy::{self, PolicyEdit};
use super::preferences;
use super::sidebar;
use crate::gateway::{self, Command, Event};
use crate::model::{Sandbox, SandboxState};
use crate::settings;
use openshell_lxd::{self as lxd, LxdStatus};

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(file = "../../data/ui/window.ui")]
    pub struct Window {
        #[template_child]
        pub toasts: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub sidebar_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub content_page: TemplateChild<adw::NavigationPage>,
        #[template_child]
        pub status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub detail: TemplateChild<adw::Bin>,

        pub sandboxes: RefCell<Vec<Sandbox>>,
        pub selected: Cell<usize>,
        /// The open Policy Rules dialog and its sandbox, kept current as
        /// the gateway reports policy changes.
        pub policy_dialog: RefCell<Option<(glib::WeakRef<adw::PreferencesDialog>, String)>>,
        /// Commands for the gateway thread, once it has been started.
        pub commands: RefCell<Option<async_channel::Sender<Command>>>,
        /// Whether the last gateway poll succeeded.
        pub connected: Cell<bool>,
        /// Set after the first successful poll, so existing prompts don't notify.
        pub connected_once: Cell<bool>,
        /// Draft chunks already notified about.
        pub seen_prompts: RefCell<std::collections::HashSet<String>>,
        /// Whether the gateway's compute driver offers GPUs.
        pub gpu_supported: Cell<bool>,
        /// Why the gateway is unreachable, for the status tooltip.
        pub gateway_error: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "OsdWindow";
        type Type = super::Window;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Window {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_actions();
            obj.setup_sidebar();
            obj.update_status();
            obj.setup_gateway();
        }
    }

    impl WidgetImpl for Window {}
    impl WindowImpl for Window {}
    impl ApplicationWindowImpl for Window {}
    impl AdwApplicationWindowImpl for Window {}
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    fn setup_actions(&self) {
        let new_sandbox = gio::ActionEntry::builder("new-sandbox")
            .activate(|win: &Self, _, _| win.new_sandbox_dialog())
            .build();
        let preferences = gio::ActionEntry::builder("preferences")
            .activate(|win: &Self, _, _| {
                preferences::dialog(&win.gateway_status()).present(Some(win))
            })
            .build();
        self.add_action_entries([new_sandbox, preferences]);
    }

    fn setup_sidebar(&self) {
        let imp = self.imp();
        imp.sandboxes.replace(lxd_sandboxes());
        imp.sidebar_list.connect_row_selected(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, row| {
                if let Some(row) = row {
                    win.select(row.index() as usize);
                }
            }
        ));
        sidebar::populate(&imp.sidebar_list, &imp.sandboxes.borrow(), 0);
        if imp.sandboxes.borrow().is_empty() {
            self.refresh_detail();
        }
    }

    fn setup_gateway(&self) {
        let handle = gateway::spawn();
        self.imp().commands.replace(Some(handle.commands));
        let events = handle.events;
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                while let Ok(event) = events.recv().await {
                    win.on_gateway_event(event);
                }
            }
        ));
    }

    fn send(&self, command: Command) {
        if let Some(commands) = self.imp().commands.borrow().as_ref() {
            let _ = commands.try_send(command);
        }
    }

    fn on_gateway_event(&self, event: Event) {
        let imp = self.imp();
        match event {
            Event::Sandboxes(snapshots) => {
                let was_connected = imp.connected.replace(true);
                imp.gateway_error.borrow_mut().clear();
                let lxd_view = lxd_sandboxes();
                let new: Vec<Sandbox> = snapshots
                    .iter()
                    .map(|snapshot| {
                        let lxd = lxd_view.iter().find(|s| s.name == snapshot.info.name);
                        Sandbox::from_gateway(snapshot, lxd)
                    })
                    .collect();
                self.notify_new_prompts(&new);
                self.replace_sandboxes(new);
                // An unchanged empty list leaves the "not running" page up.
                if !was_connected && imp.sandboxes.borrow().is_empty() {
                    self.refresh_detail();
                }
            }
            Event::Unavailable(reason) => {
                let was_connected = imp.connected.replace(false);
                let changed = *imp.gateway_error.borrow() != reason;
                imp.gateway_error.replace(reason);
                if was_connected {
                    // Fall back to what LXD shows, without gateway data.
                    imp.sandboxes.replace(Vec::new());
                    self.replace_sandboxes(lxd_sandboxes());
                } else if changed && imp.sandboxes.borrow().is_empty() {
                    // The empty page explains why; keep its reason current.
                    // Rebuilding it on every poll would destroy its focused
                    // Copy button under GTK, which crashes.
                    self.refresh_detail();
                }
            }
            Event::Done(message) => self.toast(&message),
            // Failures stay until dismissed: a slow create can fail long
            // after the dialog closed.
            Event::Failed(message) => {
                let toast = adw::Toast::new(&message);
                toast.set_timeout(0);
                // Long errors are cut off in a toast; Details shows them whole.
                toast.set_button_label(Some("Details"));
                toast.connect_button_clicked(glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_| {
                        let dialog =
                            adw::AlertDialog::new(Some("Something Went Wrong"), Some(&message));
                        dialog.add_response("close", "Close");
                        dialog.present(Some(&win));
                    }
                ));
                self.show_toast(toast);
            }
            Event::GpuSupported(gpu) => imp.gpu_supported.set(gpu),
        }
        self.update_status();
    }

    /// Swaps in a new sandbox list, keeping the selection on the same sandbox
    /// and leaving the UI alone when nothing visible changed.
    fn replace_sandboxes(&self, new: Vec<Sandbox>) {
        let imp = self.imp();
        let key = |list: &[Sandbox]| {
            list.iter()
                .map(|s| {
                    let chunks: Vec<_> = s.prompts.iter().map(|p| p.chunk_id.clone()).collect();
                    let mounts: Vec<_> = s.mounts.iter().map(|m| m.device.clone()).collect();
                    (
                        s.name.clone(),
                        s.state,
                        s.phase.clone(),
                        s.memory_used_mb,
                        s.policy.as_ref().map(|p| p.version),
                        s.rules.len(),
                        chunks,
                        mounts,
                    )
                })
                .collect::<Vec<_>>()
        };
        if key(&imp.sandboxes.borrow()) == key(&new) {
            return;
        }
        let selected_name = imp
            .sandboxes
            .borrow()
            .get(imp.selected.get())
            .map(|s| s.name.clone());
        let selected = selected_name
            .and_then(|name| new.iter().position(|s| s.name == name))
            .unwrap_or(0);
        imp.sandboxes.replace(new);
        imp.selected.set(selected);
        sidebar::populate(&imp.sidebar_list, &imp.sandboxes.borrow(), selected);
        if imp.sandboxes.borrow().is_empty() {
            self.refresh_detail();
        }
        self.refresh_policy_dialog();
    }

    /// Raises a desktop notification for each blocked request not seen before.
    /// The first poll only records what is already pending.
    fn notify_new_prompts(&self, sandboxes: &[Sandbox]) {
        let imp = self.imp();
        let first_poll = !imp.connected_once.replace(true);
        let enabled = settings::notify_blocked_requests();
        let mut seen = imp.seen_prompts.borrow_mut();
        for sandbox in sandboxes {
            for prompt in &sandbox.prompts {
                if !seen.insert(prompt.chunk_id.clone()) || first_poll || !enabled {
                    continue;
                }
                let Some(app) = self.application() else {
                    continue;
                };
                let notification =
                    gio::Notification::new(&format!("{} blocked a request", sandbox.name));
                notification.set_body(Some(&format!(
                    "{} tried to reach {}:{}",
                    if prompt.binary.is_empty() {
                        "A process"
                    } else {
                        &prompt.binary
                    },
                    prompt.host,
                    prompt.port
                )));
                notification.set_default_action("app.show-window");
                let target = (sandbox.name.as_str(), prompt.chunk_id.as_str()).to_variant();
                notification.add_button_with_target_value(
                    "Allow",
                    "app.allow-request",
                    Some(&target),
                );
                notification.add_button_with_target_value(
                    "Ignore",
                    "app.ignore-request",
                    Some(&target),
                );
                app.send_notification(Some(&notification_id(&prompt.chunk_id)), &notification);
            }
        }
    }

    fn new_sandbox_dialog(&self) {
        if !self.imp().connected.get() {
            self.toast("The OpenShell gateway is not running");
            return;
        }
        dialogs::new_sandbox(
            self,
            self.imp().gpu_supported.get(),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |name, options| {
                    // Pulling images can take a while before the gateway answers.
                    win.toast(&format!("Creating {name}…"));
                    win.send(Command::Create { name, options })
                }
            ),
        );
    }

    /// Answers a blocked request from its desktop notification.
    pub fn answer_prompt(&self, sandbox: &str, chunk_id: &str, allow: bool) {
        if let Some(app) = self.application() {
            app.withdraw_notification(&notification_id(chunk_id));
        }
        let sandboxes = self.imp().sandboxes.borrow();
        let Some(prompt) = sandboxes
            .iter()
            .filter(|s| s.name == sandbox)
            .flat_map(|s| s.prompts.iter())
            .find(|p| p.chunk_id == chunk_id)
        else {
            drop(sandboxes);
            self.toast("That request was already answered");
            return;
        };
        let command = if allow {
            Command::Approve {
                sandbox: sandbox.to_owned(),
                proposal: prompt.to_proposal(),
            }
        } else {
            Command::Reject {
                sandbox: sandbox.to_owned(),
                chunk_id: chunk_id.to_owned(),
            }
        };
        drop(sandboxes);
        self.send(command);
    }

    fn confirm_delete(&self, name: String) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Terminate {name}?")),
            Some("The sandbox and its data are removed. This cannot be undone."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Terminate & Purge")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.choose(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |response| {
                    if response == "delete" {
                        win.send(Command::Delete(name));
                    }
                }
            ),
        );
    }

    /// One line on the gateway connection, for the tooltip and Preferences.
    fn gateway_status(&self) -> String {
        let imp = self.imp();
        if imp.connected.get() {
            return "Connected".to_owned();
        }
        let error = imp.gateway_error.borrow();
        if error.is_empty() {
            "Connecting".to_owned()
        } else {
            error.clone()
        }
    }

    fn update_status(&self) {
        let status = lxd::Client::default().status();
        let imp = self.imp();
        let gateway = format!("OpenShell gateway: {}", self.gateway_status());
        let label = &imp.status_label;
        let lxd = match &status {
            LxdStatus::Active { server_version } => {
                label.set_label("Local LXD: Active ●");
                format!(
                    "LXD {}",
                    server_version.as_deref().unwrap_or("(unknown version)")
                )
            }
            LxdStatus::Unavailable { reason } => {
                label.set_label("Local LXD: Unavailable");
                format!("LXD unreachable: {reason}")
            }
        };
        let source = if imp.connected.get() {
            "Sandbox list comes from the gateway"
        } else {
            "Sandbox list shows LXD instances"
        };
        label.set_tooltip_text(Some(&format!("{lxd}\n{gateway}\n{source}")));
        let (add, remove) = if status.is_active() {
            ("active", "inactive")
        } else {
            ("inactive", "active")
        };
        label.add_css_class(add);
        label.remove_css_class(remove);
    }

    fn select(&self, index: usize) {
        let imp = self.imp();
        if index >= imp.sandboxes.borrow().len() {
            return;
        }
        imp.selected.set(index);
        self.refresh_detail();
    }

    fn refresh_detail(&self) {
        let imp = self.imp();
        let sandboxes = imp.sandboxes.borrow();
        if sandboxes.is_empty() {
            imp.content_page.set_title("OpenShell");
            let page = if imp.connected.get() {
                adw::StatusPage::builder()
                    .icon_name("utilities-terminal-symbolic")
                    .title("No Sandboxes")
                    .description("Create one with the New Sandbox button.")
                    .build()
            } else {
                self.gateway_not_running_page()
            };
            imp.detail.set_child(Some(&page));
            return;
        }
        let sandbox = &sandboxes[imp.selected.get()];
        imp.content_page.set_title(&sandbox.name);
        imp.detail.set_child(Some(&detail::build(sandbox, self)));
    }

    /// Explains why the list is empty, with the command that starts the
    /// gateway and a button that copies it.
    fn gateway_not_running_page(&self) -> adw::StatusPage {
        let command = gtk::Label::builder()
            .label(START_GATEWAY)
            .selectable(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .justify(gtk::Justification::Center)
            .css_classes(["monospace"])
            .build();
        let copy = gtk::Button::builder()
            .label("Copy Command")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        copy.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |button| {
                button.clipboard().set_text(START_GATEWAY);
                win.toast("Command copied");
            }
        ));
        let child = gtk::Box::new(gtk::Orientation::Vertical, 18);
        child.append(&command);
        child.append(&copy);
        adw::StatusPage::builder()
            .icon_name("network-offline-symbolic")
            .title("OpenShell Gateway Not Running")
            .description(format!(
                "{}\n\nStart it in a terminal with",
                glib::markup_escape_text(&self.gateway_status())
            ))
            .child(&child)
            .build()
    }

    /// Shows a toast, over the Policy Rules dialog while it is open.
    fn toast(&self, message: &str) {
        self.show_toast(adw::Toast::new(message));
    }

    fn show_toast(&self, toast: adw::Toast) {
        let dialog = self
            .imp()
            .policy_dialog
            .borrow()
            .as_ref()
            .and_then(|(dialog, _)| dialog.upgrade());
        match dialog {
            Some(dialog) => dialog.add_toast(toast),
            None => self.imp().toasts.add_toast(toast),
        }
    }

    pub fn handle_action(&self, action: SandboxAction) {
        let imp = self.imp();
        let Some(sandbox) = imp.sandboxes.borrow().get(imp.selected.get()).cloned() else {
            return;
        };
        let connected = imp.connected.get();
        match action {
            // These work on the LXD instance directly, gateway or not.
            SandboxAction::OpenTerminal if sandbox.instance.is_empty() => {
                self.toast("No LXD instance backs this sandbox yet")
            }
            SandboxAction::OpenTerminal => console::open(self, &sandbox.instance),
            SandboxAction::AddMount if sandbox.instance.is_empty() => {
                self.toast("No LXD instance backs this sandbox yet")
            }
            SandboxAction::AddMount => self.add_mount_dialog(sandbox),
            SandboxAction::RemoveMount(i) => {
                if let Some(mount) = sandbox.mounts.get(i) {
                    self.lxd_change(
                        lxd::Client::default().remove_device(&sandbox.instance, &mount.device),
                        format!("Unmounted {}", mount.sandbox),
                    );
                }
            }

            // Everything else goes through the gateway.
            _ if !connected => self.toast("This needs the OpenShell gateway, which is not running"),
            SandboxAction::ToggleRunning => match sandbox.state {
                SandboxState::Running => self.send(Command::Stop(sandbox.name)),
                SandboxState::Stopped => self.send(Command::Start(sandbox.name)),
                SandboxState::Busy => self.toast("Wait for the current operation to finish"),
            },
            SandboxAction::TerminateAndPurge => self.confirm_delete(sandbox.name),
            SandboxAction::AllowDomain(i) => {
                if let Some(prompt) = sandbox.prompts.get(i) {
                    self.answer_prompt(&sandbox.name, &prompt.chunk_id, true);
                }
            }
            SandboxAction::IgnorePrompt(i) => {
                if let Some(prompt) = sandbox.prompts.get(i) {
                    self.answer_prompt(&sandbox.name, &prompt.chunk_id, false);
                }
            }
            SandboxAction::PolicyRules => self.policy_dialog(&sandbox),
            SandboxAction::AddRule => self.edit_policy(PolicyEdit::AddRule),
            SandboxAction::RemoveRule(i) => {
                if let Some(rule) = sandbox.rules.get(i) {
                    self.edit_policy(PolicyEdit::RemoveEndpoint {
                        rule: rule.rule.clone(),
                        host: rule.host.clone(),
                        port: rule.port,
                    });
                }
            }
        }
    }

    /// The sandbox the open Policy Rules dialog is about, or else the selected one.
    fn policy_target(&self) -> Option<String> {
        let imp = self.imp();
        if let Some((dialog, name)) = imp.policy_dialog.borrow().as_ref()
            && dialog.upgrade().is_some()
        {
            return Some(name.clone());
        }
        imp.sandboxes
            .borrow()
            .get(imp.selected.get())
            .map(|s| s.name.clone())
    }

    pub fn edit_policy(&self, edit: PolicyEdit) {
        if !self.imp().connected.get() {
            self.toast("This needs the OpenShell gateway, which is not running");
            return;
        }
        let Some(sandbox) = self.policy_target() else {
            return;
        };
        match edit {
            PolicyEdit::AddRule => {
                // Over the Policy Rules dialog when it is open.
                let open = self
                    .imp()
                    .policy_dialog
                    .borrow()
                    .as_ref()
                    .and_then(|(dialog, _)| dialog.upgrade());
                let done = glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    #[strong]
                    sandbox,
                    move |rule: dialogs::NewRule| win.send(Command::AddRule {
                        sandbox: sandbox.clone(),
                        host: rule.host,
                        port: rule.port,
                        binary: rule.binary,
                    })
                );
                match open {
                    Some(dialog) => dialogs::add_rule(&dialog, &sandbox, done),
                    None => dialogs::add_rule(self, &sandbox, done),
                }
            }
            PolicyEdit::RemoveRule(rule) => self.send(Command::RemoveRule { sandbox, rule }),
            PolicyEdit::RemoveEndpoint { rule, host, port } => self.send(Command::RemoveEndpoint {
                sandbox,
                rule,
                host,
                port,
            }),
        }
    }

    fn policy_dialog(&self, sandbox: &Sandbox) {
        let dialog = policy::dialog(&sandbox.name);
        policy::show(&dialog, sandbox.policy.as_ref(), self);
        self.imp()
            .policy_dialog
            .replace(Some((dialog.downgrade(), sandbox.name.clone())));
        dialog.present(Some(self));
    }

    /// Redraws the open Policy Rules dialog from the latest gateway data.
    fn refresh_policy_dialog(&self) {
        let imp = self.imp();
        let Some((dialog, name)) = imp.policy_dialog.borrow().clone() else {
            return;
        };
        let Some(dialog) = dialog.upgrade() else {
            imp.policy_dialog.replace(None);
            return;
        };
        let policy = imp
            .sandboxes
            .borrow()
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.policy.clone());
        policy::show(&dialog, policy.as_ref(), self);
    }

    fn add_mount_dialog(&self, sandbox: Sandbox) {
        dialogs::add_mount(
            self,
            &sandbox.name,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |disk: lxd::Disk| {
                    let device = lxd::mount_device_name(&disk.path);
                    if sandbox.mounts.iter().any(|m| m.device == device) {
                        win.toast(&format!("Something is already mounted at {}", disk.path));
                        return;
                    }
                    win.lxd_change(
                        lxd::Client::default().add_disk(&sandbox.instance, &device, &disk),
                        format!("Mounted {} at {}", disk.source, disk.path),
                    );
                }
            ),
        );
    }

    /// Reports an LXD change and redraws the sandbox with its new devices.
    fn lxd_change(&self, result: Result<(), String>, done: String) {
        match result {
            Ok(()) => {
                self.toast(&done);
                self.reload_lxd_details();
            }
            Err(error) => self.toast(&error),
        }
    }

    /// Re-reads LXD so device changes show without waiting for the gateway.
    fn reload_lxd_details(&self) {
        let imp = self.imp();
        let lxd_view = lxd_sandboxes();
        let current = imp.sandboxes.borrow().clone();
        let new = if imp.connected.get() {
            current
                .into_iter()
                .map(|mut sandbox| {
                    if let Some(fresh) = lxd_view.iter().find(|s| s.instance == sandbox.instance) {
                        sandbox.mounts = fresh.mounts.clone();
                    }
                    sandbox
                })
                .collect()
        } else {
            lxd_view
        };
        self.replace_sandboxes(new);
    }
}

/// Sandboxes as LXD shows them, without gateway data.
fn lxd_sandboxes() -> Vec<Sandbox> {
    lxd::Client::default()
        .instances()
        .map(|instances| {
            lxd::list_sandboxes(&instances)
                .iter()
                .map(Sandbox::from_listed)
                .collect()
        })
        .unwrap_or_default()
}

/// Enables the snap's gateway and LXD driver for the current user.
const START_GATEWAY: &str = "systemctl --user enable --now \
     snap.openshell-desktop.gateway.service snap.openshell-desktop.lxd-driver.service";

fn notification_id(chunk_id: &str) -> String {
    format!("blocked-{chunk_id}")
}
