use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::detail::{self, SandboxAction};
use super::sidebar;
use crate::backend::lxd::{self, LxdStatus};
use crate::model::{NetworkRule, Sandbox, SandboxState, sample_sandboxes};

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
            obj.update_status(&lxd::probe());
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
            .activate(|win: &Self, _, _| win.toast("New Sandbox wizard is not wired up yet"))
            .build();
        let preferences = gio::ActionEntry::builder("preferences")
            .activate(|win: &Self, _, _| win.toast("Preferences are not wired up yet"))
            .build();
        self.add_action_entries([new_sandbox, preferences]);
    }

    fn setup_sidebar(&self) {
        let imp = self.imp();
        imp.sandboxes.replace(sample_sandboxes());
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

    fn update_status(&self, status: &LxdStatus) {
        let label = &self.imp().status_label;
        let lxd = match status {
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
        label.set_tooltip_text(Some(&format!(
            "{lxd}\nOpenShell gateway: not connected yet\nSandbox list shows sample data"
        )));
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
        {
            let mut sandboxes = imp.sandboxes.borrow_mut();
            let sandbox = &mut sandboxes[selected];
            match action {
                SandboxAction::ToggleRunning => {
                    sandbox.state = match sandbox.state {
                        SandboxState::Running => SandboxState::Stopped,
                        SandboxState::Stopped => SandboxState::Running,
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
                SandboxAction::OpenTerminal => {
                    message = Some("Terminal is not wired up yet".into())
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
