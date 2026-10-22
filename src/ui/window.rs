use relm4::adw::prelude::*;
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt, SimpleComponent, adw, gtk};

use super::detail::{self, SandboxAction};
use super::sidebar;
use crate::backend::lxd::{self, LxdStatus};
use crate::model::{NetworkRule, Sandbox, SandboxState, sample_sandboxes};

pub struct App {
    sandboxes: Vec<Sandbox>,
    selected: usize,
    lxd: LxdStatus,
    sidebar_list: gtk::ListBox,
    detail: adw::Bin,
    toasts: adw::ToastOverlay,
}

#[derive(Debug)]
pub enum AppMsg {
    Select(usize),
    Action(SandboxAction),
    NewSandbox,
    Settings,
    About,
}

impl App {
    fn status_label(&self) -> String {
        match &self.lxd {
            LxdStatus::Active { .. } => "Local LXD: Active ●".into(),
            LxdStatus::Unavailable { .. } => "Local LXD: Unavailable".into(),
        }
    }

    fn status_tooltip(&self) -> String {
        let lxd = match &self.lxd {
            LxdStatus::Active { server_version } => {
                format!(
                    "LXD {}",
                    server_version.as_deref().unwrap_or("(unknown version)")
                )
            }
            LxdStatus::Unavailable { reason } => format!("LXD unreachable: {reason}"),
        };
        format!("{lxd}\nOpenShell gateway: not connected yet\nSandbox list shows sample data")
    }

    fn status_classes(&self) -> [&'static str; 2] {
        [
            "status-pill",
            if self.lxd.is_active() {
                "active"
            } else {
                "inactive"
            },
        ]
    }

    fn current(&mut self) -> &mut Sandbox {
        &mut self.sandboxes[self.selected]
    }

    fn refresh_detail(&self, sender: &ComponentSender<Self>) {
        self.detail
            .set_child(Some(&detail::build(&self.sandboxes[self.selected], sender)));
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    fn handle_action(&mut self, action: SandboxAction, sender: &ComponentSender<Self>) {
        match action {
            SandboxAction::ToggleRunning => {
                let sandbox = self.current();
                sandbox.state = match sandbox.state {
                    SandboxState::Running => SandboxState::Stopped,
                    SandboxState::Stopped => SandboxState::Running,
                };
                sidebar::populate(&self.sidebar_list, &self.sandboxes, self.selected);
            }
            SandboxAction::AllowDomain(i) => {
                let sandbox = self.current();
                let prompt = sandbox.prompts.remove(i);
                sandbox.rules.push(NetworkRule {
                    host: prompt.host.clone(),
                    port: prompt.port,
                });
                self.toast(&format!("Allowed {}:{}", prompt.host, prompt.port));
            }
            SandboxAction::IgnorePrompt(i) => {
                self.current().prompts.remove(i);
            }
            SandboxAction::OpenTerminal => self.toast("Terminal is not wired up yet"),
            SandboxAction::PolicyRules => self.toast("Policy editor is not wired up yet"),
            SandboxAction::AddMount => self.toast("Mount picker is not wired up yet"),
            SandboxAction::AddRule => self.toast("Rule editor is not wired up yet"),
            SandboxAction::TerminateAndPurge => self.toast("Terminate & Purge is not wired up yet"),
        }
        self.refresh_detail(sender);
    }
}

#[relm4::component(pub)]
impl SimpleComponent for App {
    type Init = ();
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("OpenShell Manager"),
            set_default_width: 1100,
            set_default_height: 760,
            set_width_request: 360,
            set_height_request: 400,

            add_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
                adw::BreakpointConditionLengthType::MaxWidth,
                640.0,
                adw::LengthUnit::Sp,
            )) {
                add_setter: (&split_view, "collapsed", Some(&true.to_value())),
            },

            #[local_ref]
            toasts -> adw::ToastOverlay {
                #[name = "split_view"]
                adw::NavigationSplitView {
                    #[wrap(Some)]
                    set_sidebar = &adw::NavigationPage {
                        set_title: "Sandboxes",

                        #[wrap(Some)]
                        set_child = &adw::ToolbarView {
                            add_top_bar = &adw::HeaderBar {
                                pack_start = &gtk::MenuButton {
                                    set_icon_name: "open-menu-symbolic",
                                    set_tooltip_text: Some("Main Menu"),
                                    #[wrap(Some)]
                                    set_popover = &gtk::Popover {
                                        gtk::Button {
                                            set_label: "About OpenShell Manager",
                                            add_css_class: "flat",
                                            connect_clicked => AppMsg::About,
                                        },
                                    },
                                },
                            },

                            #[wrap(Some)]
                            set_content = &gtk::ScrolledWindow {
                                set_vexpand: true,
                                set_hscrollbar_policy: gtk::PolicyType::Never,

                                #[local_ref]
                                sidebar_list -> gtk::ListBox {
                                    add_css_class: "navigation-sidebar",
                                    connect_row_selected[sender] => move |_, row| {
                                        if let Some(row) = row {
                                            sender.input(AppMsg::Select(row.index() as usize));
                                        }
                                    },
                                },
                            },

                            add_bottom_bar = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 6,
                                set_margin_all: 6,

                                gtk::Button {
                                    add_css_class: "flat",
                                    #[wrap(Some)]
                                    set_child = &adw::ButtonContent {
                                        set_icon_name: "list-add-symbolic",
                                        set_label: "New Sandbox",
                                    },
                                    connect_clicked => AppMsg::NewSandbox,
                                },
                                gtk::Button {
                                    add_css_class: "flat",
                                    #[wrap(Some)]
                                    set_child = &adw::ButtonContent {
                                        set_icon_name: "emblem-system-symbolic",
                                        set_label: "Settings",
                                    },
                                    connect_clicked => AppMsg::Settings,
                                },
                            },
                        },
                    },

                    #[wrap(Some)]
                    set_content = &adw::NavigationPage {
                        #[watch]
                        set_title: &model.sandboxes[model.selected].name,

                        #[wrap(Some)]
                        set_child = &adw::ToolbarView {
                            add_top_bar = &adw::HeaderBar {
                                #[wrap(Some)]
                                set_title_widget = &gtk::Label {
                                    set_label: &model.status_label(),
                                    set_tooltip_text: Some(&model.status_tooltip()),
                                    set_css_classes: &model.status_classes(),
                                },
                            },

                            #[wrap(Some)]
                            #[local_ref]
                            set_content = detail -> adw::Bin {},
                        },
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = App {
            sandboxes: sample_sandboxes(),
            selected: 0,
            lxd: lxd::probe(),
            sidebar_list: gtk::ListBox::new(),
            detail: adw::Bin::new(),
            toasts: adw::ToastOverlay::new(),
        };

        let sidebar_list = &model.sidebar_list;
        let detail = &model.detail;
        let toasts = &model.toasts;
        let widgets = view_output!();

        sidebar::populate(&model.sidebar_list, &model.sandboxes, model.selected);
        model.refresh_detail(&sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::Select(index) if index < self.sandboxes.len() => {
                self.selected = index;
                self.refresh_detail(&sender);
            }
            AppMsg::Select(_) => {}
            AppMsg::Action(action) => self.handle_action(action, &sender),
            AppMsg::NewSandbox => self.toast("New Sandbox wizard is not wired up yet"),
            AppMsg::Settings => self.toast("Settings are not wired up yet"),
            AppMsg::About => {
                let about = adw::AboutDialog::builder()
                    .application_name("OpenShell Manager")
                    .application_icon("utilities-terminal")
                    .version(env!("CARGO_PKG_VERSION"))
                    .website(env!("CARGO_PKG_REPOSITORY"))
                    .license_type(gtk::License::Apache20)
                    .comments("Configure, launch and monitor NVIDIA OpenShell sandboxes on LXD")
                    .build();
                about.present(self.toasts.root().as_ref());
            }
        }
    }
}
