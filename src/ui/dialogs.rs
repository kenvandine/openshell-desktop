//! Small forms: new network rule and new host folder mount.
//! Each asks its question in an `AdwAlertDialog` and hands the answer to a
//! callback, which only runs when the form is confirmed and valid.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk::{gio, glib};

/// Where mounts go by default: the sandbox workspace, which its policy
/// already lets the agent read and write.
const WORKSPACE: &str = "/sandbox";

fn form() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn alert(heading: &str, body: Option<&str>, confirm: &str) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), body);
    dialog.add_responses(&[("cancel", "Cancel"), ("confirm", confirm)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");
    dialog
}

/// Enables the confirm button only while `valid` holds, rechecking whenever
/// one of the `entries` changes.
fn validate(
    dialog: &adw::AlertDialog,
    entries: &[&adw::EntryRow],
    valid: impl Fn() -> bool + 'static,
) {
    let valid = std::rc::Rc::new(valid);
    dialog.set_response_enabled("confirm", valid());
    for entry in entries {
        let dialog = dialog.downgrade();
        let valid = valid.clone();
        entry.connect_changed(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("confirm", valid());
            }
        });
    }
}

/// The rule a user asked for: `binary` is `None` for any executable.
pub struct NewRule {
    pub host: String,
    pub port: u16,
    pub binary: Option<String>,
}

pub fn add_rule(parent: &impl IsA<gtk::Widget>, sandbox: &str, done: impl Fn(NewRule) + 'static) {
    let host = adw::EntryRow::builder()
        .title("Host (for example api.github.com or *.example.com)")
        .build();
    let port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    port.set_title("Port");
    port.set_value(443.0);
    let binary = adw::EntryRow::builder()
        .title("Only for executable (blank for any)")
        .build();
    let fields = form();
    fields.append(&host);
    fields.append(&port);
    fields.append(&binary);

    let dialog = alert(
        "Add Network Rule",
        Some(&format!("Let {sandbox} connect to another destination.")),
        "Add",
    );
    dialog.set_extra_child(Some(&fields));
    validate(&dialog, &[&host, &binary], {
        let (host, binary) = (host.clone(), binary.clone());
        move || {
            let host = host.text();
            let binary = binary.text();
            let host = host.trim();
            !host.is_empty()
                && !host.contains(char::is_whitespace)
                && !host.contains(['/', ':'])
                && (binary.trim().is_empty() || binary.trim().starts_with('/'))
        }
    });
    dialog.choose(Some(parent), gio::Cancellable::NONE, move |response| {
        if response != "confirm" {
            return;
        }
        let binary = binary.text().trim().to_owned();
        done(NewRule {
            host: host.text().trim().to_lowercase(),
            port: port.value() as u16,
            binary: (!binary.is_empty()).then_some(binary),
        });
    });
}

pub fn add_mount(
    parent: &impl IsA<gtk::Widget>,
    sandbox: &str,
    done: impl Fn(openshell_lxd::Disk) + 'static,
) {
    let source = adw::EntryRow::builder().title("Host folder").build();
    let choose = gtk::Button::from_icon_name("folder-open-symbolic");
    choose.set_tooltip_text(Some("Choose Folder"));
    choose.add_css_class("flat");
    choose.set_valign(gtk::Align::Center);
    source.add_suffix(&choose);
    let target = adw::EntryRow::builder().title("Path in sandbox").build();
    let readonly = adw::SwitchRow::builder()
        .title("Read-only")
        .active(true)
        .build();
    let fields = form();
    fields.append(&source);
    fields.append(&target);
    fields.append(&readonly);

    // Suggest a path under the workspace until the user types their own.
    let suggested = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    source.connect_changed(glib::clone!(
        #[weak]
        target,
        #[strong]
        suggested,
        move |source| {
            let current = target.text();
            if !current.is_empty() && *current != *suggested.borrow() {
                return;
            }
            let name = Path::new(source.text().trim())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let path = if name.is_empty() {
                String::new()
            } else {
                format!("{WORKSPACE}/{name}")
            };
            suggested.replace(path.clone());
            target.set_text(&path);
        }
    ));

    let dialog = alert(
        "Mount Host Folder",
        Some(&format!(
            "Share a folder with {sandbox}. Its default policy lets the agent \
             use folders in its workspace, {WORKSPACE}."
        )),
        "Mount",
    );
    dialog.set_extra_child(Some(&fields));
    choose.connect_clicked(glib::clone!(
        #[weak]
        source,
        #[weak]
        dialog,
        move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose a Folder to Share")
                .modal(true)
                .build();
            let window = dialog.root().and_downcast::<gtk::Window>();
            chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |result| {
                if let Some(path) = result.ok().and_then(|file| host_path(&file)) {
                    source.set_text(&path.to_string_lossy());
                }
            });
        }
    ));
    validate(&dialog, &[&source, &target], {
        let (source, target) = (source.clone(), target.clone());
        move || {
            let target = target.text();
            source.text().trim().starts_with('/')
                && target.trim().starts_with('/')
                && target.trim() != "/"
        }
    });
    dialog.choose(Some(parent), gio::Cancellable::NONE, move |response| {
        if response != "confirm" {
            return;
        }
        done(openshell_lxd::Disk {
            source: source.text().trim().to_owned(),
            path: target.text().trim().trim_end_matches('/').to_owned(),
            readonly: readonly.is_active(),
        });
    });
}

/// The folder's real path on the host. Inside the snap the file chooser
/// portal hands out document portal paths (`/run/user/1000/doc/<id>/<name>`),
/// which LXD cannot mount, so those are mapped back through the portal.
pub fn host_path(file: &gio::File) -> Option<PathBuf> {
    let path = file.path()?;
    let Some((id, rest)) = document_id(&path) else {
        return Some(path);
    };
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let reply = bus
        .call_sync(
            Some("org.freedesktop.portal.Documents"),
            "/org/freedesktop/portal/documents",
            "org.freedesktop.portal.Documents",
            "GetHostPaths",
            Some(&(vec![id.clone()],).to_variant()),
            Some(glib::VariantTy::new("(a{say})").expect("valid type")),
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let (paths,) = reply.get::<(HashMap<String, Vec<u8>>,)>()?;
    let bytes = paths.get(&id)?;
    let host = PathBuf::from(OsStr::from_bytes(bytes.strip_suffix(&[0]).unwrap_or(bytes)));
    Some(if rest.as_os_str().is_empty() {
        host
    } else {
        host.join(rest)
    })
}

/// Splits `/run/user/<uid>/doc/<id>/<name>/<rest…>` into the document ID and
/// `<rest…>`; the document itself is `<name>`.
fn document_id(path: &Path) -> Option<(String, PathBuf)> {
    let mut parts = path.strip_prefix("/run/user").ok()?.components();
    parts.next()?;
    if parts.next()?.as_os_str() != "doc" {
        return None;
    }
    let id = parts.next()?.as_os_str().to_str()?.to_owned();
    parts.next()?;
    Some((id, parts.as_path().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_document_portal_paths() {
        assert_eq!(
            document_id(Path::new("/run/user/1000/doc/a1b2/Projects/src")),
            Some(("a1b2".into(), PathBuf::from("src")))
        );
        assert_eq!(
            document_id(Path::new("/run/user/1000/doc/a1b2/Projects")),
            Some(("a1b2".into(), PathBuf::new()))
        );
        assert_eq!(document_id(Path::new("/home/me/Projects")), None);
        assert_eq!(document_id(Path::new("/run/user/1000/gvfs/x")), None);
    }
}
