//! The dialog that sets what an iPod syncs: the whole library, only the
//! playlists checked, or all but them, with the size of each playlist and
//! of the selection against the iPod's.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use iprune_core::adopt::LibPlaylist;
use iprune_core::config::Scope;
use iprune_core::store::TrackRow;
use iprune_core::store::devices::DeviceScope;
use iprune_core::sync;
use unicode_normalization::UnicodeNormalization;

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

/// A playlist as the dialog lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRow {
    pub name: String,
    /// The name lowercased with its Latin accents removed. Punctuation and
    /// a leading "The" stay, so the "+" and "-" groups keep together, as
    /// in the sidebar.
    key: Vec<char>,
    /// What its present tracks weigh, each counted once, as a selection of
    /// this playlist alone would.
    pub bytes: u64,
}

impl PlaylistRow {
    /// By name, ignoring case and accents, with numbers in order of value:
    /// "Synthwave 2" before "Synthwave 10".
    pub fn by_name(&self, other: &PlaylistRow) -> Ordering {
        natural(&self.key, &other.key).then_with(|| self.name.cmp(&other.name))
    }

    /// Smallest first, then by name.
    pub fn by_size(&self, other: &PlaylistRow) -> Ordering {
        self.bytes
            .cmp(&other.bytes)
            .then_with(|| self.by_name(other))
    }
}

/// Lowercase, Latin accents (U+0300 to U+036F) removed.
fn fold(name: &str) -> Vec<char> {
    name.nfkd()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .collect::<String>()
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .chars()
        .collect()
}

/// Character by character, except that runs of digits compare by value.
/// Runs of equal value but different leading zeros compare equal.
fn natural(a: &[char], b: &[char]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let ((x, next_i), (y, next_j)) = (digit_run(a, i), digit_run(b, j));
            let o = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
            if o != Ordering::Equal {
                return o;
            }
            (i, j) = (next_i, next_j);
        } else {
            let o = a[i].cmp(&b[j]);
            if o != Ordering::Equal {
                return o;
            }
            (i, j) = (i + 1, j + 1);
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// The run of digits starting at `k` without its leading zeros, and where
/// the run ends.
fn digit_run(s: &[char], k: usize) -> (&[char], usize) {
    let end = s[k..]
        .iter()
        .position(|c| !c.is_ascii_digit())
        .map_or(s.len(), |n| k + n);
    let digits = &s[k..end];
    let zeros = digits.iter().take_while(|c| **c == '0').count();
    (&digits[zeros..], end)
}

/// One row per playlist, in the order given.
pub fn rows(library: &[TrackRow], playlists: &[LibPlaylist]) -> Vec<PlaylistRow> {
    let sizes: HashMap<i64, u64> = library
        .iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| (t.id, t.size))
        .collect();
    playlists
        .iter()
        .map(|p| {
            let ids: HashSet<i64> = p.members.iter().flatten().copied().collect();
            PlaylistRow {
                name: p.name.clone(),
                key: fold(&p.name),
                bytes: ids.iter().filter_map(|id| sizes.get(id)).sum(),
            }
        })
        .collect()
}

/// A playlist's size: in MB, in GB from what would round to 1000.0 MB.
pub fn size_label(bytes: u64) -> String {
    if bytes >= 999_950_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        crate::view_model::megabytes(bytes)
    }
}

/// The playlist a list item stands for.
fn row_of<'a>(rows: &'a [PlaylistRow], obj: &glib::Object) -> &'a PlaylistRow {
    let i = *obj
        .downcast_ref::<glib::BoxedAnyObject>()
        .expect("the list holds indexes")
        .borrow::<usize>();
    &rows[i]
}

/// The playlists with their boxes. `changed` runs after a box is checked
/// or cleared.
fn name_column(
    rows: &Rc<Vec<PlaylistRow>>,
    checked: &Rc<RefCell<BTreeSet<String>>>,
    changed: Rc<dyn Fn()>,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    // Set while a box is bound to its row: what binding sets is not a
    // click. Binds run one at a time, so one flag will do.
    let binding = Rc::new(Cell::new(false));
    {
        let (rows, checked, binding) = (rows.clone(), checked.clone(), binding.clone());
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let check = gtk::CheckButton::new();
            check.set_child(Some(&label));
            // Records the playlist this box shows now: a recycled box
            // moves between rows.
            let weak_item = item.downgrade();
            let (rows, checked, binding, changed) = (
                rows.clone(),
                checked.clone(),
                binding.clone(),
                changed.clone(),
            );
            check.connect_toggled(move |c| {
                if binding.get() {
                    return;
                }
                let Some(obj) = weak_item.upgrade().and_then(|it| it.item()) else {
                    return;
                };
                let name = &row_of(&rows, &obj).name;
                if c.is_active() {
                    checked.borrow_mut().insert(name.clone());
                } else {
                    checked.borrow_mut().remove(name);
                }
                changed();
            });
            item.set_child(Some(&check));
        });
    }
    let (rows_bind, checked) = (rows.clone(), checked.clone());
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let check = item
            .child()
            .and_downcast::<gtk::CheckButton>()
            .expect("a check button");
        let label = check.child().and_downcast::<gtk::Label>().expect("a label");
        let row = row_of(&rows_bind, &item.item().expect("an item"));
        label.set_text(&row.name);
        label.set_tooltip_text(Some(&row.name));
        binding.set(true);
        check.set_active(checked.borrow().contains(&row.name));
        binding.set(false);
    });
    let column = gtk::ColumnViewColumn::new(Some("Name"), Some(factory));
    column.set_resizable(true);
    column.set_expand(true);
    let rows = rows.clone();
    let sorter =
        gtk::CustomSorter::new(move |a, b| row_of(&rows, a).by_name(row_of(&rows, b)).into());
    column.set_sorter(Some(&sorter));
    column
}

/// Each playlist's size.
fn size_column(rows: &Rc<Vec<PlaylistRow>>) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = gtk::Label::new(None);
        label.set_xalign(1.0);
        item.set_child(Some(&label));
    });
    {
        let rows = rows.clone();
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let label = item.child().and_downcast::<gtk::Label>().expect("a label");
            let row = row_of(&rows, &item.item().expect("an item"));
            label.set_text(&size_label(row.bytes));
        });
    }
    let column = gtk::ColumnViewColumn::new(Some("Size"), Some(factory));
    column.set_resizable(true);
    column.set_fixed_width(90);
    let rows = rows.clone();
    let sorter =
        gtk::CustomSorter::new(move |a, b| row_of(&rows, a).by_size(row_of(&rows, b)).into());
    column.set_sorter(Some(&sorter));
    column
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
    let rows = Rc::new(rows(&input.library, &input.playlists));
    let filter = gtk::SearchEntry::new();
    filter.set_placeholder_text(Some("Filter playlists"));
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let objects: Vec<glib::Object> = (0..rows.len())
        .map(|i| glib::BoxedAnyObject::new(i).upcast())
        .collect();
    store.extend_from_slice(&objects);
    let name_filter = {
        let (rows, filter) = (rows.clone(), filter.downgrade());
        gtk::CustomFilter::new(move |obj| {
            let text = filter
                .upgrade()
                .map(|f| f.text().to_lowercase())
                .unwrap_or_default();
            text.is_empty() || row_of(&rows, obj).name.to_lowercase().contains(&text)
        })
    };
    let filtered = gtk::FilterListModel::new(Some(store), Some(name_filter.clone()));
    filter.connect_search_changed(move |_| name_filter.changed(gtk::FilterChange::Different));
    let view = gtk::ColumnView::new(None::<gtk::NoSelection>);
    view.add_css_class("data-table");
    view.set_show_column_separators(true);
    let sorted = gtk::SortListModel::new(Some(filtered), view.sorter());
    view.set_model(Some(&gtk::NoSelection::new(Some(sorted))));
    let scroll = gtk::ScrolledWindow::builder()
        .child(&view)
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
    let update: Rc<dyn Fn()> = Rc::new(update);
    for radio in [&all, &only, &except] {
        let update = update.clone();
        radio.connect_toggled(move |_| update());
    }
    let name = name_column(&rows, &checked, update.clone());
    view.append_column(&name);
    view.append_column(&size_column(&rows));
    view.sort_by_column(Some(&name), gtk::SortType::Ascending);
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
