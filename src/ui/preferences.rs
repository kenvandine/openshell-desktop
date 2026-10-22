//! The Preferences dialog: notification settings, and where the app finds
//! the OpenShell gateway and LXD.

use adw::prelude::*;
use gtk::glib;
use openshell_gateway_client::Config;
use openshell_lxd::{self as lxd, LxdStatus};

use crate::settings;

/// `gateway` is the status line the window shows in its tooltip.
pub fn dialog(gateway: &str) -> adw::PreferencesDialog {
    let page = adw::PreferencesPage::new();

    let notifications = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();
    let blocked = adw::SwitchRow::builder()
        .title("Blocked requests")
        .subtitle("Notify when a sandbox tries to reach a destination its policy blocks")
        .active(settings::notify_blocked_requests())
        .build();
    notifications.add(&blocked);
    page.add(&notifications);

    let config = Config::discover();
    let gateway_group = adw::PreferencesGroup::builder()
        .title("OpenShell Gateway")
        .description(
            "The gateway bundled in the snap. Change its port with \
             sudo snap set openshell-desktop gateway-port=PORT",
        )
        .build();
    gateway_group.add(&info_row("Status", gateway));
    gateway_group.add(&info_row("Endpoint", &config.endpoint));
    gateway_group.add(&info_row(
        "Client certificates",
        &config.tls_dir.to_string_lossy(),
    ));
    page.add(&gateway_group);

    let lxd_group = adw::PreferencesGroup::builder().title("LXD").build();
    let status = match lxd::Client::default().status() {
        LxdStatus::Active { server_version } => format!(
            "Active, version {}",
            server_version.as_deref().unwrap_or("unknown")
        ),
        LxdStatus::Unavailable { reason } => format!("Unavailable: {reason}"),
    };
    lxd_group.add(&info_row("Status", &status));
    lxd_group.add(&info_row("Socket", lxd::SNAP_SOCKET));
    page.add(&lxd_group);

    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .search_enabled(false)
        .build();
    dialog.add(&page);

    blocked.connect_active_notify(glib::clone!(
        #[weak]
        dialog,
        move |row| {
            if let Err(error) = settings::set_notify_blocked_requests(row.is_active()) {
                dialog.add_toast(adw::Toast::new(&format!(
                    "Could not save the setting: {error}"
                )));
            }
        }
    ));
    dialog
}

fn info_row(title: &str, value: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_selectable(true)
        .use_markup(false)
        .css_classes(["property"])
        .build()
}
