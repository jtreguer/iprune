//! The dialog that sets what an iPod syncs: the whole library, only the
//! playlists checked, or all but them, with the size of the selection
//! against the iPod's.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use gtk::prelude::*;
use iprune_core::adopt::LibPlaylist;
use iprune_core::config::Scope;
use iprune_core::store::TrackRow;
use iprune_core::store::devices::DeviceScope;
use iprune_core::sync;

/// What the dialog needs to show and size a selection.
pub struct Input {
    pub ipod_name: String,
    /// The iPod's size, when it is connected.
    pub capacity: Option<u64>,
    pub current: DeviceScope,
    pub library: Vec<TrackRow>,
    pub playlists: Vec<LibPlaylist>,
}

/// The size line: tracks and bytes, and how they compare with `capacity`.
pub fn size_text(tracks: usize, bytes: u64, capacity: Option<u64>) -> String {
    let noun = if tracks == 1 { "track" } else { "tracks" };
    let base = format!("{tracks} {noun}, {}", sync::bytes(bytes));
    match capacity {
        Some(cap) if bytes > cap => format!(
            "{base}: {} more than the iPod holds",
            sync::bytes(bytes - cap)
        ),
        Some(cap) => format!("{base}, of the {} the iPod holds", sync::bytes(cap)),
        None => base,
    }
}

/// The line under the list, and whether the selection can be saved: one
/// that a sync would refuse cannot, and the line says why.
pub fn status(
    s: &DeviceScope,
    library: &[TrackRow],
    playlists: &[LibPlaylist],
    capacity: Option<u64>,
) -> (String, bool) {
    if let Some(why) = sync::scope_problem(s, true, library, playlists) {
        return (format!("Cannot be saved: {why}"), false);
    }
    let (tracks, bytes) = s.size(library, playlists);
    (size_text(tracks, bytes, capacity), true)
}

/// The playlists `current` names that no longer exist, which the dialog
/// leaves out.
pub fn gone(current: &DeviceScope, playlists: &[LibPlaylist]) -> Vec<String> {
    current
        .playlists
        .iter()
        .filter(|n| !playlists.iter().any(|p| &p.name == *n))
        .cloned()
        .collect()
}

/// Shows the dialog; `save` gets the selection when the user saves it.
pub fn show(parent: &gtk::ApplicationWindow, input: Input, save: impl Fn(DeviceScope) + 'static) {
    let dialog = gtk::Window::builder()
        .title(format!("What {} Syncs", input.ipod_name))
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .default_height(560)
        .build();

    let all = gtk::CheckButton::with_label("The whole library");
    let only = gtk::CheckButton::with_label("Only the playlists checked");
    let except = gtk::CheckButton::with_label("The whole library except the playlists checked");
    only.set_group(Some(&all));
    except.set_group(Some(&all));
    match input.current.kind {
        Scope::All => all.set_active(true),
        Scope::Playlists => only.set_active(true),
        Scope::Except => except.set_active(true),
    }

    // A playlist the selection names but that is gone is left out, and
    // said so: a sync refuses the selection until it is.
    let gone = gone(&input.current, &input.playlists);
    let checked: Rc<RefCell<BTreeSet<String>>> = Rc::new(RefCell::new(
        input
            .current
            .playlists
            .iter()
            .filter(|n| !gone.contains(n))
            .cloned()
            .collect(),
    ));
    let filter = gtk::SearchEntry::new();
    filter.set_placeholder_text(Some("Filter playlists"));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    for p in &input.playlists {
        let check = gtk::CheckButton::with_label(&p.name);
        check.set_active(checked.borrow().contains(&p.name));
        check.set_margin_start(6);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&check));
        row.set_activatable(false);
        row.set_widget_name(&p.name);
        list.append(&row);
        let (checked, name) = (checked.clone(), p.name.clone());
        check.connect_toggled(move |c| {
            if c.is_active() {
                checked.borrow_mut().insert(name.clone());
            } else {
                checked.borrow_mut().remove(&name);
            }
        });
    }
    {
        let filter = filter.clone();
        list.set_filter_func(move |row| {
            let text = filter.text().to_lowercase();
            text.is_empty() || row.widget_name().to_lowercase().contains(&text)
        });
    }
    {
        let list = list.downgrade();
        filter.connect_search_changed(move |_| {
            if let Some(list) = list.upgrade() {
                list.invalidate_filter();
            }
        });
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    scroll.add_css_class("frame");

    let size = gtk::Label::new(None);
    size.set_xalign(0.0);
    size.set_wrap(true);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Save");
    ok.add_css_class("suggested-action");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&ok);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.append(&all);
    body.append(&only);
    body.append(&except);
    body.append(&filter);
    body.append(&scroll);
    if !gone.is_empty() {
        let note = gtk::Label::new(Some(&format!(
            "No longer in the library, so left out: {}",
            gone.join(", ")
        )));
        note.set_xalign(0.0);
        note.set_wrap(true);
        body.append(&note);
    }
    body.append(&size);
    body.append(&buttons);
    dialog.set_child(Some(&body));

    let input = Rc::new(input);
    // The selection as the dialog stands.
    let current = {
        let (all, only, checked) = (all.clone(), only.clone(), checked.clone());
        move || DeviceScope {
            kind: if all.is_active() {
                Scope::All
            } else if only.is_active() {
                Scope::Playlists
            } else {
                Scope::Except
            },
            playlists: if all.is_active() {
                Vec::new()
            } else {
                checked.borrow().iter().cloned().collect()
            },
        }
    };
    let update = {
        let (input, current) = (input.clone(), current.clone());
        let (size, ok, filter, scroll) = (size.clone(), ok.clone(), filter.clone(), scroll.clone());
        move || {
            let s = current();
            let (text, can_save) = status(&s, &input.library, &input.playlists, input.capacity);
            size.set_text(&text);
            let pick = s.kind != Scope::All;
            filter.set_sensitive(pick);
            scroll.set_sensitive(pick);
            ok.set_sensitive(can_save);
        }
    };
    let update = Rc::new(update);
    for radio in [&all, &only, &except] {
        let update = update.clone();
        radio.connect_toggled(move |_| update());
    }
    let mut child = list.first_child();
    while let Some(row) = child {
        if let Some(check) = row
            .first_child()
            .and_then(|c| c.downcast::<gtk::CheckButton>().ok())
        {
            let update = update.clone();
            // Runs after the handler that records the playlist.
            check.connect_toggled(move |_| update());
        }
        child = row.next_sibling();
    }
    update();

    {
        let dialog = dialog.downgrade();
        ok.connect_clicked(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
            save(current());
        });
    }
    crate::closes(&cancel, &dialog);
    dialog.present();
}

#[cfg(test)]
mod tests;
