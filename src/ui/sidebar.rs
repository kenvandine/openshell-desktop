use adw::prelude::*;

use crate::model::{Sandbox, SandboxState};

/// Rebuilds the sandbox list and restores the selection.
pub fn populate(list: &gtk::ListBox, sandboxes: &[Sandbox], selected: usize) {
    list.remove_all();
    for sandbox in sandboxes {
        list.append(&row(sandbox));
    }
    if let Some(row) = list.row_at_index(selected as i32) {
        list.select_row(Some(&row));
    }
}

fn row(sandbox: &Sandbox) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&sandbox.name)
        .subtitle(sandbox.summary())
        .build();

    let (glyph, class) = match sandbox.state {
        SandboxState::Running => ("●", "running"),
        SandboxState::Stopped => ("○", "stopped"),
    };
    let dot = gtk::Label::new(Some(glyph));
    dot.add_css_class("state-dot");
    dot.add_css_class(class);
    row.add_prefix(&dot);
    row
}
