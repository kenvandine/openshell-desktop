use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::console;
use super::detail::{self, SandboxAction};
use super::sidebar;
use crate::gateway::{self, Command, Event};
use crate::model::{NetworkRule, Sandbox, SandboxState, sample_sandboxes};
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
        /// Whether the sandboxes are live LXD instances rather than samples.
        pub live: Cell<bool>,
        /// Commands for the gateway thread, once it has been started.
        pub commands: RefCell<Option<async_channel::Sender<Command>>>,
        /// Whether the last gateway poll succeeded.
        pub connected: Cell<bool>,
        /// Set after the first successful poll, so existing prompts don't notify.
        pub connected_once: Cell<bool>,
        /// Draft chunks already notified about.
        pub seen_prompts: RefCell<std::collections::HashSet<String>>,
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
            .activate(|win: &Self, _, _| win.toast("Preferences are not wired up yet"))
            .build();
        self.add_action_entries([new_sandbox, preferences]);
    }

    fn setup_sidebar(&self) {
        let imp = self.imp();
        let live: Vec<Sandbox> = lxd::Client::default()
            .instances()
            .map(|instances| {
                lxd::list_sandboxes(&instances)
                    .iter()
                    .map(Sandbox::from_listed)
                    .collect()
            })
            .unwrap_or_default();
        imp.live.set(!live.is_empty());
        imp.sandboxes.replace(if live.is_empty() {
            sample_sandboxes()
        } else {
            live
        });
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
                imp.connected.set(true);
                imp.gateway_error.borrow_mut().clear();
                let lxd_view: Vec<Sandbox> = lxd::Client::default()
                    .instances()
                    .map(|instances| {
                        lxd::list_sandboxes(&instances)
                            .iter()
                            .map(Sandbox::from_listed)
                            .collect()
                    })
                    .unwrap_or_default();
                let new: Vec<Sandbox> = snapshots
                    .iter()
                    .map(|snapshot| {
                        let lxd = lxd_view.iter().find(|s| s.name == snapshot.info.name);
                        Sandbox::from_gateway(snapshot, lxd)
                    })
                    .collect();
                self.notify_new_prompts(&new);
                imp.live.set(true);
                self.replace_sandboxes(new);
            }
            Event::Unavailable(reason) => {
                imp.connected.set(false);
                imp.gateway_error.replace(reason);
            }
            Event::Done(message) => self.toast(&message),
            Event::Failed(message) => self.toast(&message),
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
                    (
                        s.name.clone(),
                        s.state,
                        s.phase.clone(),
                        s.memory_used_mb,
                        s.rules.len(),
                        chunks,
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
    }

    /// Raises a desktop notification for each blocked request not seen before.
    /// The first poll only records what is already pending.
    fn notify_new_prompts(&self, sandboxes: &[Sandbox]) {
        let imp = self.imp();
        let first_poll = !imp.connected_once.replace(true);
        let mut seen = imp.seen_prompts.borrow_mut();
        for sandbox in sandboxes {
            for prompt in &sandbox.prompts {
                if !seen.insert(prompt.chunk_id.clone()) || first_poll {
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
                app.send_notification(Some(&format!("blocked-{}", prompt.chunk_id)), &notification);
            }
        }
    }

    fn new_sandbox_dialog(&self) {
        if !self.imp().connected.get() {
            self.toast("The OpenShell gateway is not running");
            return;
        }
        let name = adw::EntryRow::builder().title("Name").build();
        let image = adw::EntryRow::builder()
            .title("Image (blank for the gateway default)")
            .build();
        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        fields.append(&name);
        fields.append(&image);

        let dialog = adw::AlertDialog::new(Some("New Sandbox"), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        dialog.set_extra_child(Some(&fields));
        dialog.choose(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |response| {
                    let name = name.text().trim().to_owned();
                    if response == "create" && !name.is_empty() {
                        win.send(Command::Create {
                            name,
                            image: image.text().trim().to_owned(),
                        });
                    }
                }
            ),
        );
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

    fn update_status(&self) {
        let status = lxd::Client::default().status();
        let imp = self.imp();
        let gateway = if imp.connected.get() {
            "OpenShell gateway: connected".to_owned()
        } else {
            let error = imp.gateway_error.borrow();
            if error.is_empty() {
                "OpenShell gateway: connecting".to_owned()
            } else {
                format!("OpenShell gateway: {error}")
            }
        };
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
        let source = if self.imp().connected.get() {
            "Sandbox list comes from the gateway"
        } else if self.imp().live.get() {
            "Sandbox list shows LXD instances"
        } else {
            "Sandbox list shows sample data"
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
            let page = adw::StatusPage::builder()
                .icon_name("utilities-terminal-symbolic")
                .title("No Sandboxes")
                .description("Create one with the New Sandbox button.")
                .build();
            imp.detail.set_child(Some(&page));
            return;
        }
        let sandbox = &sandboxes[imp.selected.get()];
        imp.content_page.set_title(&sandbox.name);
        imp.detail.set_child(Some(&detail::build(sandbox, self)));
    }

    fn toast(&self, message: &str) {
        self.imp().toasts.add_toast(adw::Toast::new(message));
    }

    pub fn handle_action(&self, action: SandboxAction) {
        let imp = self.imp();
        let selected = imp.selected.get();
        let mut message = None;
        if imp.sandboxes.borrow().is_empty() {
            return;
        }
        if imp.connected.get() {
            let sandbox = imp.sandboxes.borrow()[selected].clone();
            match action {
                SandboxAction::ToggleRunning => match sandbox.state {
                    SandboxState::Running => self.send(Command::Stop(sandbox.name)),
                    SandboxState::Stopped => self.send(Command::Start(sandbox.name)),
                    SandboxState::Busy => self.toast("Wait for the current operation to finish"),
                },
                SandboxAction::TerminateAndPurge => self.confirm_delete(sandbox.name),
                SandboxAction::OpenTerminal if !sandbox.instance.is_empty() => {
                    console::open(self, &sandbox.instance)
                }
                SandboxAction::OpenTerminal => self.toast("No LXD instance backs this sandbox yet"),
                SandboxAction::AllowDomain(i) => {
                    if let Some(prompt) = sandbox.prompts.get(i) {
                        self.send(Command::Approve {
                            sandbox: sandbox.name.clone(),
                            proposal: prompt.to_proposal(),
                        });
                    }
                }
                SandboxAction::IgnorePrompt(i) => {
                    if let Some(prompt) = sandbox.prompts.get(i) {
                        self.send(Command::Reject {
                            sandbox: sandbox.name.clone(),
                            chunk_id: prompt.chunk_id.clone(),
                        });
                    }
                }
                SandboxAction::PolicyRules => self.toast("Policy editor is not wired up yet"),
                SandboxAction::AddMount => self.toast("Mount picker is not wired up yet"),
                SandboxAction::AddRule => self.toast("Rule editor is not wired up yet"),
            }
            return;
        }
        {
            let mut sandboxes = imp.sandboxes.borrow_mut();
            let sandbox = &mut sandboxes[selected];
            match action {
                SandboxAction::ToggleRunning => {
                    sandbox.state = match sandbox.state {
                        SandboxState::Running => SandboxState::Stopped,
                        SandboxState::Stopped | SandboxState::Busy => SandboxState::Running,
                    };
                }
                SandboxAction::AllowDomain(i) => {
                    let prompt = sandbox.prompts.remove(i);
                    message = Some(format!("Allowed {}:{}", prompt.host, prompt.port));
                    sandbox.rules.push(NetworkRule {
                        host: prompt.host,
                        port: prompt.port,
                    });
                }
                SandboxAction::IgnorePrompt(i) => {
                    sandbox.prompts.remove(i);
                }
                SandboxAction::OpenTerminal if imp.live.get() => {
                    console::open(self, &sandbox.instance)
                }
                SandboxAction::OpenTerminal => {
                    message = Some("The console needs LXD; this is sample data".into())
                }
                SandboxAction::PolicyRules => {
                    message = Some("Policy editor is not wired up yet".into())
                }
                SandboxAction::AddMount => {
                    message = Some("Mount picker is not wired up yet".into())
                }
                SandboxAction::AddRule => message = Some("Rule editor is not wired up yet".into()),
                SandboxAction::TerminateAndPurge => {
                    message = Some("Terminate & Purge is not wired up yet".into())
                }
            }
        }

        // Repopulating reselects the current row, which also refreshes the
        // detail pane through `select`.
        sidebar::populate(&imp.sidebar_list, &imp.sandboxes.borrow(), selected);
        if let Some(message) = message {
            self.toast(&message);
        }
    }
}
