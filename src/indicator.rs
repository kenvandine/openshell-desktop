//! The status indicator in the top bar (a StatusNotifierItem, which Ubuntu's
//! AppIndicator support shows): what is running and whether anything waits
//! for approval, with quick actions in its menu.
//!
//! ksni serves the item on its own thread. Menu clicks come back to the GTK
//! main loop as `Action`s; the window pushes a new `Summary` whenever its
//! sandbox list changes.

use gtk::{gdk_pixbuf, gio, glib};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{StandardItem, SubMenu};
use ksni::{Icon, MenuItem, Status, ToolTip};

use crate::APP_ID;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Show,
    NewSandbox,
    OpenTerminal(String),
    Stop(String),
    /// Show the sandbox with pending requests.
    Review(String),
    Quit,
}

/// What the indicator shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub connected: bool,
    pub running: Vec<String>,
    /// Sandboxes with blocked requests waiting, and how many each.
    pub pending: Vec<(String, usize)>,
}

impl Summary {
    fn pending_count(&self) -> usize {
        self.pending.iter().map(|(_, n)| n).sum()
    }

    /// One line for the tooltip and the menu's header.
    pub fn headline(&self) -> String {
        if !self.connected {
            return "OpenShell gateway not running".to_owned();
        }
        let running = match self.running.len() {
            0 => "No sandboxes running".to_owned(),
            1 => "1 sandbox running".to_owned(),
            n => format!("{n} sandboxes running"),
        };
        match self.pending_count() {
            0 => running,
            1 => format!("{running} · 1 request waiting"),
            n => format!("{running} · {n} requests waiting"),
        }
    }
}

struct Tray {
    summary: Summary,
    actions: async_channel::Sender<Action>,
    /// The app icon at the sizes top bars use. The shell draws the item, and
    /// it cannot look a snap's icon up by name in the host's icon theme, so
    /// the item sends the pixels.
    icon: Vec<Icon>,
}

impl Tray {
    fn item(&self, label: &str, action: Action) -> MenuItem<Self> {
        StandardItem {
            label: label.replace('_', "__"),
            activate: Box::new(move |tray: &mut Self| {
                let _ = tray.actions.send_blocking(action.clone());
            }),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        APP_ID.into()
    }

    fn title(&self) -> String {
        "OpenShell Desktop".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn attention_icon_name(&self) -> String {
        "dialog-warning-symbolic".into()
    }

    fn status(&self) -> Status {
        if self.summary.pending_count() > 0 {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "OpenShell Desktop".into(),
            description: self.summary.headline(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send_blocking(Action::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let summary = &self.summary;
        let mut menu = vec![
            StandardItem {
                label: summary.headline(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];
        for (sandbox, count) in &summary.pending {
            let label = match count {
                1 => format!("Review 1 request from {sandbox}…"),
                n => format!("Review {n} requests from {sandbox}…"),
            };
            menu.push(self.item(&label, Action::Review(sandbox.clone())));
        }
        if !summary.running.is_empty() {
            menu.push(MenuItem::Separator);
            for sandbox in &summary.running {
                menu.push(
                    SubMenu {
                        label: sandbox.replace('_', "__"),
                        submenu: vec![
                            self.item("Open Terminal", Action::OpenTerminal(sandbox.clone())),
                            self.item("Stop", Action::Stop(sandbox.clone())),
                        ],
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        menu.push(MenuItem::Separator);
        if summary.connected {
            menu.push(self.item("New Agent…", Action::NewSandbox));
        }
        menu.push(self.item("Show OpenShell Desktop", Action::Show));
        menu.push(self.item("Quit", Action::Quit));
        menu
    }
}

pub struct Indicator(Handle<Tray>);

impl Indicator {
    /// Registers the indicator from a helper thread, since registering waits
    /// on D-Bus. The handle arrives on the first channel once the shell has
    /// accepted the item; it never does when no StatusNotifierItem host runs,
    /// and the app then simply has no indicator.
    pub fn spawn() -> (
        async_channel::Receiver<Indicator>,
        async_channel::Receiver<Action>,
    ) {
        let (actions, actions_rx) = async_channel::unbounded();
        let (handle_tx, handle_rx) = async_channel::bounded(1);
        let icon = icon_pixmaps();
        std::thread::Builder::new()
            .name("indicator".into())
            .spawn(move || {
                let tray = Tray {
                    summary: Summary::default(),
                    actions,
                    icon,
                };
                match tray.spawn() {
                    Ok(handle) => {
                        let _ = handle_tx.send_blocking(Indicator(handle));
                    }
                    Err(error) => eprintln!("openshell-desktop: no status indicator: {error}"),
                }
            })
            .expect("spawn indicator thread");
        (handle_rx, actions_rx)
    }

    /// Another handle to the same item, so updating it needs no borrow held.
    pub fn clone_handle(&self) -> Self {
        Self(self.0.clone())
    }

    pub fn update(&self, summary: Summary) {
        self.0.update(move |tray| {
            if tray.summary != summary {
                tray.summary = summary;
            }
        });
    }
}

/// The app icon rendered at 16 to 64 pixels, from `data/icons/indicator`.
const ICON_PNGS: &[&[u8]] = &[
    include_bytes!("../data/icons/indicator/16.png"),
    include_bytes!("../data/icons/indicator/22.png"),
    include_bytes!("../data/icons/indicator/24.png"),
    include_bytes!("../data/icons/indicator/32.png"),
    include_bytes!("../data/icons/indicator/48.png"),
    include_bytes!("../data/icons/indicator/64.png"),
];

/// The icon as StatusNotifierItem pixmaps: ARGB32 in network byte order.
fn icon_pixmaps() -> Vec<Icon> {
    ICON_PNGS
        .iter()
        .filter_map(|png| {
            let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(png));
            let pixbuf = gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
            Some(to_argb(&pixbuf))
        })
        .collect()
}

fn to_argb(pixbuf: &gdk_pixbuf::Pixbuf) -> Icon {
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let channels = pixbuf.n_channels() as usize;
    let stride = pixbuf.rowstride() as usize;
    let pixels = pixbuf.read_pixel_bytes();
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let p = &pixels[y * stride + x * channels..][..channels];
            let alpha = if channels == 4 { p[3] } else { 255 };
            data.extend_from_slice(&[alpha, p[0], p[1], p[2]]);
        }
    }
    Icon {
        width,
        height,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_sent_as_argb_pixmaps() {
        let icons = icon_pixmaps();
        assert_eq!(
            icons.iter().map(|i| i.width).collect::<Vec<_>>(),
            [16, 22, 24, 32, 48, 64]
        );
        let big = icons.last().unwrap();
        assert_eq!(big.data.len(), 64 * 64 * 4);
        // The corner is outside the rounded window; its middle is opaque.
        assert_eq!(big.data[0], 0);
        let middle = (32 * 64 + 32) * 4;
        assert_eq!(big.data[middle], 255);
    }

    #[test]
    fn headline_counts_running_and_waiting() {
        let mut summary = Summary::default();
        assert_eq!(summary.headline(), "OpenShell gateway not running");
        summary.connected = true;
        assert_eq!(summary.headline(), "No sandboxes running");
        summary.running = vec!["a".into(), "b".into()];
        summary.pending = vec![("a".into(), 2), ("b".into(), 1)];
        assert_eq!(
            summary.headline(),
            "2 sandboxes running · 3 requests waiting"
        );
    }
}
