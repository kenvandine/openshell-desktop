//! The app's own preferences, in a key file under the user's config
//! directory (inside the snap, `$SNAP_USER_DATA/.config`).

use std::path::PathBuf;

use gtk::glib;

const GROUP: &str = "notifications";
const BLOCKED_REQUESTS: &str = "blocked-requests";

fn path() -> PathBuf {
    glib::user_config_dir()
        .join("openshell-desktop")
        .join("settings.ini")
}

fn load() -> glib::KeyFile {
    let file = glib::KeyFile::new();
    let _ = file.load_from_file(path(), glib::KeyFileFlags::KEEP_COMMENTS);
    file
}

/// Whether a desktop notification announces each newly blocked request.
pub fn notify_blocked_requests() -> bool {
    load().boolean(GROUP, BLOCKED_REQUESTS).unwrap_or(true)
}

pub fn set_notify_blocked_requests(enabled: bool) -> Result<(), String> {
    let file = load();
    file.set_boolean(GROUP, BLOCKED_REQUESTS, enabled);
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    file.save_to_file(&path).map_err(|e| e.to_string())
}
