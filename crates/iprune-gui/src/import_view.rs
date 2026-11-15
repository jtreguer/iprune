//! The Import view (design sections 7.6 and 8, technical document sections
//! 7.2 and 14): the inbox's files in a grid, the pane for the one selected,
//! and the bar with the playlist and the Import button. What it shows is
//! worked out in `view_model::import` and the work runs in `import_job`;
//! this is the widgets, and the window's side of the work.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use iprune_core::import::{Killer, Query, Status};
use iprune_core::store::TrackRow;

use crate::view_model::import::{self as model, ImportColumn, ImportRow};
use crate::{Source, Ui, UiSlot, cell_label, import_job, info_dialog, joined, report_page, ui_of};

/// The playlist songs are filed into, if any.
type Playlist = Option<String>;

/// What the view's worker is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Idle,
    LookingUp,
    Filing,
}

pub struct ImportView {
    slot: UiSlot,
    pub page: gtk::Box,
    store: gio::ListStore,
    pub filter: gtk::CustomFilter,
    selection: gtk::MultiSelection,
    // The pane.
    heading: gtk::Label,
    explanation: gtk::Label,
    candidates: gtk::ListBox,
    radios: RefCell<Vec<gtk::CheckButton>>,
    use_match: gtk::Button,
    search: gtk::MenuButton,
    search_popover: gtk::Popover,
    search_artist: gtk::Entry,
    search_title: gtk::Entry,
    type_tags: gtk::Button,
    look_again: gtk::Button,
    anyway: gtk::CheckButton,
    duplicates: gtk::Box,
    destination: gtk::Label,
    // The bar.
    progress: gtk::Label,
    stop: gtk::Button,
    playlists: gtk::StringList,
    playlist: gtk::DropDown,
    import: gtk::Button,
    // State.
    /// The rows as last read, in the grid's default order.
    rows: RefCell<Vec<ImportRow>>,
    job: Cell<Job>,
    reading: Cell<bool>,
    read_again: Cell<bool>,
    /// Lookups asked for while the worker was busy.
    look_after: Cell<bool>,
    /// Import pressed during lookups: once they stop, the songs are filed,
    /// into this playlist if any.
    file_after: RefCell<Option<Playlist>>,
    stop_flag: RefCell<Option<Arc<AtomicBool>>>,
    killer: Arc<Mutex<Option<Killer>>>,
    /// Why lookups do not start by themselves: Stop was pressed, or autotag
    /// failed. Cleared by what the user does in the view, or by opening it.
    hold: RefCell<Option<String>>,
    /// What the line under the grid says when nothing runs and nothing is
    /// held: a path in the inbox that could not be read, or an import
    /// refused while something else ran.
    notice: RefCell<Option<String>>,
    /// Why the last Import press did nothing (a scan or sync was running):
    /// said until the next press.
    refused: RefCell<Option<String>>,
    /// The file the pane shows, kept across reloads.
    shown: RefCell<Option<PathBuf>>,
    filling: Cell<bool>,
    /// The rows are being replaced: the selection's changes meanwhile are
    /// not the user's.
    rebuilding: Cell<bool>,
    /// The sidebar's Import row, when there is one.
    pub sidebar_row: RefCell<Option<gtk::ListBoxRow>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    reload_pending: Cell<bool>,
}

fn import_row_of(obj: &glib::Object) -> std::cell::Ref<'_, ImportRow> {
    obj.downcast_ref::<glib::BoxedAnyObject>()
        .expect("the import grid holds rows")
        .borrow::<ImportRow>()
}

fn make_column(col: ImportColumn) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        item.set_child(Some(&cell_label(col.numeric())));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = item.child().and_downcast::<gtk::Label>().expect("a label");
        let obj = item.item().expect("a row");
        let row = import_row_of(&obj);
        let text = col.text(&row);
        // A value the file would get changed is bold, the one it has in
        // the tooltip.
        if col.was(&row).is_some() {
            label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&text)));
        } else {
            label.set_use_markup(false);
            label.set_text(&text);
        }
        label.set_tooltip_text(col.tooltip(&row).as_deref());
    });
    let column = gtk::ColumnViewColumn::new(Some(col.title()), Some(factory));
    column.set_resizable(true);
    column.set_fixed_width(col.width());
    column.set_expand(col == ImportColumn::Name);
    let sorter = gtk::CustomSorter::new(move |a, b| {
        col.compare(&import_row_of(a), &import_row_of(b)).into()
    });
    column.set_sorter(Some(&sorter));
    column
}

fn pane_label(css: Option<&str>) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_selectable(true);
    if let Some(c) = css {
        l.add_css_class(c);
    }
    l
}

impl ImportView {
    /// The window, from the view's side.
    pub fn ui(&self) -> Option<Rc<Ui>> {
        ui_of(&self.slot)
    }

    /// Lets lookups start by themselves again: the view was opened.
    pub fn clear_hold(&self) {
        self.hold.replace(None);
    }

    pub fn new(slot: UiSlot) -> ImportView {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let filter = {
            let slot = slot.clone();
            gtk::CustomFilter::new(move |obj| {
                ui_of(&slot).is_none_or(|ui| import_row_of(obj).matches(&ui.query.borrow()))
            })
        };
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        let view = gtk::ColumnView::new(None::<gtk::MultiSelection>);
        view.set_show_column_separators(true);
        view.add_css_class("data-table");
        view.add_css_class("track-grid");
        let mut first = None;
        for col in ImportColumn::ALL {
            let c = make_column(col);
            view.append_column(&c);
            first.get_or_insert(c);
        }
        let sorted = gtk::SortListModel::new(Some(filtered), view.sorter());
        let selection = gtk::MultiSelection::new(Some(sorted));
        view.set_model(Some(&selection));
        // Waiting for the user first, as the rows come.
        view.sort_by_column(first.as_ref(), gtk::SortType::Ascending);
        let grid_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&view)
            .build();

        // The pane.
        let heading = pane_label(Some("heading"));
        let explanation = pane_label(None);
        let candidates = gtk::ListBox::new();
        candidates.set_selection_mode(gtk::SelectionMode::None);
        candidates.add_css_class("boxed-list");
        let use_match = gtk::Button::with_label("Use This Match");
        use_match.set_tooltip_text(Some("Import the file with the match ticked above"));
        let search_artist = gtk::Entry::new();
        search_artist.set_placeholder_text(Some("Artist"));
        search_artist.set_activates_default(true);
        let search_title = gtk::Entry::new();
        search_title.set_placeholder_text(Some("Title"));
        let search_go = gtk::Button::with_label("Search");
        search_go.add_css_class("suggested-action");
        let search_box = gtk::Grid::new();
        search_box.set_row_spacing(6);
        search_box.set_column_spacing(6);
        search_box.attach(&gtk::Label::new(Some("Artist")), 0, 0, 1, 1);
        search_box.attach(&search_artist, 1, 0, 1, 1);
        search_box.attach(&gtk::Label::new(Some("Title")), 0, 1, 1, 1);
        search_box.attach(&search_title, 1, 1, 1, 1);
        search_box.attach(&search_go, 1, 2, 1, 1);
        search_go.set_halign(gtk::Align::End);
        let search_popover = gtk::Popover::new();
        search_popover.set_child(Some(&search_box));
        let search = gtk::MenuButton::new();
        search.set_label("Search Again…");
        search.set_tooltip_text(Some("Look the file up with another artist and title"));
        search.set_popover(Some(&search_popover));
        let type_tags = gtk::Button::with_label("Type Tags…");
        type_tags.set_tooltip_text(Some("Type the tags the file gets (Ctrl+I)"));
        let look_again = gtk::Button::with_label("Look Up Again");
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for w in [
            use_match.upcast_ref::<gtk::Widget>(),
            search.upcast_ref(),
            type_tags.upcast_ref(),
            look_again.upcast_ref(),
        ] {
            actions.append(w);
        }
        let anyway = gtk::CheckButton::with_label("Import anyway");
        anyway.set_tooltip_text(Some("Import it although it may already be in the library"));
        let duplicates = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let destination = pane_label(Some("dim-label"));
        let pane = gtk::Box::new(gtk::Orientation::Vertical, 8);
        pane.set_margin_top(12);
        pane.set_margin_bottom(12);
        pane.set_margin_start(12);
        pane.set_margin_end(12);
        // The actions above the candidates, which can be many: they stay in
        // sight in a short pane.
        for w in [
            heading.upcast_ref::<gtk::Widget>(),
            explanation.upcast_ref(),
            destination.upcast_ref(),
            actions.upcast_ref(),
            anyway.upcast_ref(),
            duplicates.upcast_ref(),
            candidates.upcast_ref(),
        ] {
            pane.append(w);
        }
        let pane_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&pane)
            .build();
        let paned = gtk::Paned::new(gtk::Orientation::Vertical);
        paned.set_start_child(Some(&grid_scroll));
        paned.set_end_child(Some(&pane_scroll));
        paned.set_resize_end_child(false);
        paned.set_shrink_end_child(false);
        paned.set_vexpand(true);
        paned.set_position(420);

        // The bar.
        let add_files = gtk::Button::with_label("Add Files…");
        add_files.set_tooltip_text(Some(
            "Copy songs into the inbox (or drop them on the window)",
        ));
        let add_folders = gtk::Button::with_label("Add Folders…");
        add_folders.set_tooltip_text(Some("Copy folders of songs into the inbox"));
        let progress = gtk::Label::new(None);
        progress.set_xalign(0.0);
        progress.set_hexpand(true);
        progress.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        progress.add_css_class("dim-label");
        let stop = gtk::Button::with_label("Stop");
        stop.set_visible(false);
        let playlists = gtk::StringList::new(&["No playlist"]);
        let playlist = gtk::DropDown::new(
            Some(playlists.clone()),
            Some(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            )),
        );
        playlist.set_enable_search(true);
        playlist.set_tooltip_text(Some("Add the songs imported to this playlist"));
        let import = gtk::Button::with_label(&model::import_label(0));
        import.add_css_class("suggested-action");
        import.set_sensitive(false);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_margin_top(6);
        bar.set_margin_bottom(6);
        bar.set_margin_start(6);
        bar.set_margin_end(6);
        for w in [
            add_files.upcast_ref::<gtk::Widget>(),
            add_folders.upcast_ref(),
            progress.upcast_ref(),
            stop.upcast_ref(),
            gtk::Label::new(Some("Add to")).upcast_ref(),
            playlist.upcast_ref(),
            import.upcast_ref(),
        ] {
            bar.append(w);
        }
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&paned);
        page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        page.append(&bar);

        let iv = ImportView {
            slot: slot.clone(),
            page,
            store,
            filter,
            selection: selection.clone(),
            heading,
            explanation,
            candidates,
            radios: RefCell::new(Vec::new()),
            use_match: use_match.clone(),
            search,
            search_popover: search_popover.clone(),
            search_artist: search_artist.clone(),
            search_title: search_title.clone(),
            type_tags: type_tags.clone(),
            look_again: look_again.clone(),
            anyway: anyway.clone(),
            duplicates,
            destination,
            progress,
            stop: stop.clone(),
            playlists,
            playlist,
            import: import.clone(),
            rows: RefCell::new(Vec::new()),
            job: Cell::new(Job::Idle),
            reading: Cell::new(false),
            read_again: Cell::new(false),
            look_after: Cell::new(false),
            file_after: RefCell::new(None),
            stop_flag: RefCell::new(None),
            killer: Arc::new(Mutex::new(None)),
            hold: RefCell::new(None),
            notice: RefCell::new(None),
            refused: RefCell::new(None),
            shown: RefCell::new(None),
            filling: Cell::new(false),
            rebuilding: Cell::new(false),
            sidebar_row: RefCell::new(None),
            monitor: RefCell::new(None),
            reload_pending: Cell::new(false),
        };

        // Each control asks the window, reached through the slot.
        let on = |button: &gtk::Button, f: fn(&Rc<Ui>)| {
            let slot = slot.clone();
            button.connect_clicked(move |_| {
                if let Some(ui) = ui_of(&slot) {
                    f(&ui);
                }
            });
        };
        on(&use_match, |ui| ui.import_choose());
        on(&type_tags, |ui| ui.import_type_tags());
        on(&look_again, |ui| ui.import_search(None));
        on(&stop, |ui| ui.import_stop());
        on(&import, |ui| ui.import_file());
        on(&add_files, |ui| ui.import_pick(false));
        on(&add_folders, |ui| ui.import_pick(true));
        {
            let slot = slot.clone();
            let search_now = move || {
                if let Some(ui) = ui_of(&slot) {
                    let q = Query {
                        artist: ui.import.search_artist.text().trim().to_owned(),
                        title: ui.import.search_title.text().trim().to_owned(),
                    };
                    ui.import.search_popover.popdown();
                    ui.import_search(Some(q));
                }
            };
            let again = search_now.clone();
            search_go.connect_clicked(move |_| again());
            let again = search_now.clone();
            search_artist.connect_activate(move |_| again());
            search_title.connect_activate(move |_| search_now());
        }
        {
            let slot = slot.clone();
            anyway.connect_toggled(move |b| {
                if let Some(ui) = ui_of(&slot)
                    && !ui.import.filling.get()
                {
                    ui.import_confirm(b.is_active());
                }
            });
        }
        {
            let slot = slot.clone();
            selection.connect_selection_changed(move |_, _, _| {
                if let Some(ui) = ui_of(&slot) {
                    ui.import_fill_pane();
                }
            });
        }
        {
            // Delete moves the selected files to the trash.
            let keys = gtk::ShortcutController::new();
            let slot = slot.clone();
            keys.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("Delete"),
                Some(gtk::CallbackAction::new(move |_, _| {
                    if let Some(ui) = ui_of(&slot) {
                        ui.import_trash_selected();
                    }
                    glib::Propagation::Stop
                })),
            ));
            view.add_controller(keys);
        }
        iv
    }
}

impl Ui {
    /// The selected rows, in the grid's order.
    fn import_selected(&self) -> Vec<ImportRow> {
        let iv = &self.import;
        let bits = iv.selection.selection();
        (0..bits.size())
            .filter_map(|i| iv.selection.item(bits.nth(i as u32)))
            .map(|o| import_row_of(&o).clone())
            .collect()
    }

    /// The row the pane shows: the first selected.
    fn import_current(&self) -> Option<ImportRow> {
        self.import_selected().into_iter().next()
    }

    /// The status line while the Import view is shown.
    pub(crate) fn import_summary(&self) -> String {
        model::summary(&self.import.rows.borrow())
    }

    /// Fills the pane for the selected file.
    pub(crate) fn import_fill_pane(&self) {
        let iv = &self.import;
        if iv.rebuilding.get() {
            return;
        }
        iv.filling.set(true);
        let row = self.import_current();
        let same_file = iv.shown.replace(row.as_ref().map(|r| r.path.clone()))
            == row.as_ref().map(|r| r.path.clone());
        // The same file with the same candidates keeps its radios, and the
        // tick the user may have moved without pressing Use This Match yet.
        let labels: Vec<String> = iv
            .radios
            .borrow()
            .iter()
            .filter_map(|b| b.label())
            .map(|l| l.to_string())
            .collect();
        let keep = row
            .as_ref()
            .is_some_and(|r| model::keep_radios(same_file, &labels, &r.candidates));
        if !keep {
            while let Some(child) = iv.candidates.first_child() {
                iv.candidates.remove(&child);
            }
            iv.radios.borrow_mut().clear();
        }
        while let Some(child) = iv.duplicates.first_child() {
            iv.duplicates.remove(&child);
        }
        let Some(r) = row else {
            iv.heading.set_text(if iv.rows.borrow().is_empty() {
                "The inbox is empty: drop songs on the window, or use Add Files…"
            } else {
                "Select a file to see its matches."
            });
            for w in [
                iv.explanation.upcast_ref::<gtk::Widget>(),
                iv.candidates.upcast_ref(),
                iv.use_match.upcast_ref(),
                iv.search.upcast_ref(),
                iv.type_tags.upcast_ref(),
                iv.look_again.upcast_ref(),
                iv.anyway.upcast_ref(),
                iv.destination.upcast_ref(),
            ] {
                w.set_visible(false);
            }
            iv.filling.set(false);
            return;
        };
        let editable = r.item.is_some()
            && r.before.is_some()
            && !matches!(
                r.status,
                Status::AlreadyInLibrary(_) | Status::NotSupported(_)
            );
        iv.heading.set_text(&r.heading());
        iv.explanation.set_text(&r.explanation());
        iv.explanation.set_visible(true);
        let mut group: Option<gtk::CheckButton> = None;
        for (i, c) in r.candidates.iter().enumerate().filter(|_| !keep) {
            let button = gtk::CheckButton::with_label(&c.label);
            button.set_group(group.as_ref());
            button.set_active(r.candidate == Some(i) && !r.typed);
            if c.certain {
                button.set_tooltip_text(Some(
                    "Certain: found by fingerprint, or within 10 seconds of the file's length",
                ));
            }
            group.get_or_insert_with(|| button.clone());
            iv.candidates.append(&button);
            iv.radios.borrow_mut().push(button);
        }
        let has_candidates = !r.candidates.is_empty();
        iv.candidates.set_visible(has_candidates && editable);
        iv.use_match.set_visible(has_candidates && editable);
        iv.search.set_visible(editable);
        iv.type_tags.set_visible(editable);
        iv.look_again
            .set_visible(r.item.is_some() && matches!(r.status, Status::Error(_)));
        // Filled for another file only: a reload must not undo what the
        // user is typing.
        if !same_file {
            let (artist, title) = r.query.clone().unwrap_or_default();
            iv.search_artist.set_text(&artist);
            iv.search_title.set_text(&title);
        }
        iv.anyway
            .set_visible(r.status == Status::PossibleDuplicate || r.confirmed);
        iv.anyway.set_active(r.confirmed);
        for (track, path) in &r.duplicates {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let label = gtk::Label::new(Some(path));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_tooltip_text(Some(path));
            line.append(&label);
            match track {
                Some(id) => {
                    let show = gtk::Button::with_label("Show in Library");
                    let (slot, id) = (iv.slot.clone(), *id);
                    show.connect_clicked(move |_| {
                        if let Some(ui) = ui_of(&slot) {
                            ui.import_show_in_library(id);
                        }
                    });
                    line.append(&show);
                }
                None => label.set_text(&format!("{path} (same title)")),
            }
            iv.duplicates.append(&line);
        }
        iv.destination.set_text(
            &r.destination
                .as_deref()
                .map(|d| format!("→ {d}"))
                .unwrap_or_default(),
        );
        iv.destination.set_visible(r.destination.is_some());
        iv.filling.set(false);
    }

    /// The Import button, Stop and the line under the grid, after the
    /// rows, the worker or the busy flag changed.
    pub(crate) fn import_update_controls(&self) {
        let iv = &self.import;
        let ready = iv.rows.borrow().iter().filter(|r| r.ready()).count();
        iv.import.set_label(&model::import_label(ready));
        iv.import
            .set_sensitive(ready > 0 && iv.job.get() != Job::Filing && !self.busy.get());
        iv.stop.set_visible(iv.job.get() != Job::Idle);
        // While songs are filed, the rows the worker read must not change.
        let free = iv.job.get() != Job::Filing;
        for w in [
            iv.use_match.upcast_ref::<gtk::Widget>(),
            iv.search.upcast_ref(),
            iv.type_tags.upcast_ref(),
            iv.look_again.upcast_ref(),
            iv.anyway.upcast_ref(),
        ] {
            w.set_sensitive(free);
        }
        if iv.job.get() == Job::Idle {
            let hold = iv.hold.borrow();
            let notice = iv.notice.borrow();
            let refused = iv.refused.borrow();
            iv.progress.set_text(
                hold.as_deref()
                    .or(refused.as_deref())
                    .or(notice.as_deref())
                    .unwrap_or_default(),
            );
        }
    }

    /// The playlist chosen in the bar, if any.
    fn import_playlist(&self) -> Option<String> {
        let i = self.import.playlist.selected();
        (i > 0 && i != gtk::INVALID_LIST_POSITION)
            .then(|| self.import.playlists.string(i).map(|s| s.to_string()))
            .flatten()
    }

    /// The bar's playlists, the one chosen kept.
    pub(crate) fn import_fill_playlists(&self) {
        let iv = &self.import;
        let chosen = self.import_playlist();
        let names = self.data.playlist_names().unwrap_or_default();
        let mut all = vec!["No playlist".to_owned()];
        all.extend(names);
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        iv.playlists.splice(0, iv.playlists.n_items(), &refs);
        let at = chosen
            .and_then(|c| all.iter().position(|n| *n == c))
            .unwrap_or(0);
        iv.playlist.set_selected(at as u32);
    }

    /// Reads the inbox again on a worker, then shows it. Lookups start by
    /// themselves when files wait for one, unless the user stopped them or
    /// autotag failed.
    pub(crate) fn import_reload(self: &Rc<Self>) {
        let iv = &self.import;
        if iv.reading.replace(true) {
            iv.read_again.set(true);
            return;
        }
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || import_job::read(&config, &paths)).await;
            let Some(ui) = ui.upgrade() else { return };
            ui.import.reading.set(false);
            match joined(result) {
                Ok(listing) => ui.import_set_rows(listing),
                Err(e) => ui.import.progress.set_text(&format!("{e:#}")),
            }
            if ui.import.read_again.replace(false) {
                ui.import_reload();
            }
        });
    }

    fn import_set_rows(self: &Rc<Self>, listing: import_job::Listing) {
        let iv = &self.import;
        let selected: std::collections::HashSet<PathBuf> =
            self.import_selected().into_iter().map(|r| r.path).collect();
        let shown = iv.shown.borrow().clone();
        let objects: Vec<glib::Object> = listing
            .rows
            .iter()
            .cloned()
            .map(|r| glib::BoxedAnyObject::new(r).upcast())
            .collect();
        let waiting = listing.rows.iter().any(|r| r.status == Status::LookingUp);
        let count = listing.rows.len();
        *iv.rows.borrow_mut() = listing.rows;
        iv.rebuilding.set(true);
        iv.store.remove_all();
        iv.store.extend_from_slice(&objects);
        // The rows selected stay selected, the one the pane showed first;
        // with none left, the first row.
        let n = iv.selection.n_items();
        let path_at = |i: u32| iv.selection.item(i).map(|o| import_row_of(&o).path.clone());
        let first = (0..n)
            .find(|&i| path_at(i).as_ref() == shown.as_ref())
            .or_else(|| (0..n).find(|&i| path_at(i).is_some_and(|p| selected.contains(&p))))
            .or((n > 0).then_some(0));
        if let Some(i) = first {
            iv.selection.select_item(i, true);
            for j in (0..n).filter(|&j| j != i) {
                if path_at(j).is_some_and(|p| selected.contains(&p)) {
                    iv.selection.select_item(j, false);
                }
            }
        }
        iv.rebuilding.set(false);
        self.import_fill_pane();
        // A path that cannot be read is said, but holds nothing.
        *iv.notice.borrow_mut() = listing
            .unreadable
            .first()
            .map(|(p, why)| format!("{}: {why}", p.display()));
        self.import_watch();
        self.import_update_controls();
        self.import_count_changed(count);
        let shown_now = *self.current.borrow() == Source::Import;
        if shown_now {
            self.update_status();
        }
        let idle = iv.job.get() == Job::Idle;
        if model::auto_look_up(waiting, shown_now, idle, iv.hold.borrow().is_some()) {
            self.import_look_up();
        }
    }

    /// Looks up the files waiting for it, and the genres of the matches
    /// chosen, on a worker.
    pub(crate) fn import_look_up(self: &Rc<Self>) {
        let iv = &self.import;
        if iv.job.get() != Job::Idle {
            iv.look_after.set(true);
            return;
        }
        *iv.hold.borrow_mut() = None;
        let stop = Arc::new(AtomicBool::new(false));
        *iv.stop_flag.borrow_mut() = Some(stop.clone());
        iv.job.set(Job::LookingUp);
        iv.progress.set_text(&model::progress_line(false, 0, 0, ""));
        self.import_update_controls();
        let (tx, rx) = async_channel::unbounded::<(usize, usize, String)>();
        let ui = Rc::downgrade(self);
        {
            let ui = ui.clone();
            glib::spawn_future_local(async move {
                while let Ok((n, total, file)) = rx.recv().await {
                    if let Some(ui) = ui.upgrade()
                        && ui.import.job.get() == Job::LookingUp
                    {
                        ui.import
                            .progress
                            .set_text(&model::progress_line(false, n, total, &file));
                        // The file before it is answered: show it now.
                        if n > 0 {
                            ui.import_reload();
                        }
                    }
                }
            });
        }
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let killer = iv.killer.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let progress = move |n: usize, total: usize, file: &str| {
                    let _ = tx.send_blocking((n, total, file.to_owned()));
                };
                import_job::look_up(&config, &paths, &progress, &stop, &killer)
            })
            .await;
            let Some(ui) = ui.upgrade() else { return };
            let iv = &ui.import;
            iv.job.set(Job::Idle);
            iv.stop_flag.replace(None);
            let importing = iv.file_after.borrow().is_some();
            match joined(result) {
                // Stopped for an import: the lookups go on after it.
                Ok(looked) if looked.stopped && !importing => {
                    *iv.hold.borrow_mut() = Some("Lookups stopped.".into());
                }
                Ok(_) => {}
                Err(e) => *iv.hold.borrow_mut() = Some(format!("{e:#}")),
            }
            ui.import_update_controls();
            if let Some(playlist) = iv.file_after.take() {
                iv.look_after.set(false);
                ui.import_file_now(playlist);
                return;
            }
            let again = iv.look_after.replace(false);
            ui.import_reload();
            if again {
                ui.import_look_up();
            }
        });
    }

    /// Stop: lookups end after the one in progress, which is killed if it
    /// has not ended 2 seconds later (it writes nothing); filing ends after
    /// the file in progress.
    pub(crate) fn import_stop(self: &Rc<Self>) {
        let iv = &self.import;
        let Some(stop) = iv.stop_flag.borrow().clone() else {
            return;
        };
        iv.file_after.replace(None);
        iv.look_after.set(false);
        iv.stop.set_sensitive(false);
        if iv.job.get() == Job::LookingUp {
            self.import_halt_lookups(stop);
        } else {
            stop.store(true, Ordering::SeqCst);
        }
        let ui = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            if let Some(ui) = ui.upgrade() {
                ui.import.stop.set_sensitive(true);
            }
        });
    }

    /// Ends the lookups after the one in progress, which is killed if it
    /// has not ended 2 seconds later: it writes nothing.
    fn import_halt_lookups(self: &Rc<Self>, stop: Arc<AtomicBool>) {
        stop.store(true, Ordering::SeqCst);
        let ui = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            let Some(ui) = ui.upgrade() else { return };
            // The same pass still running: a later one has its own flag.
            let same = ui
                .import
                .stop_flag
                .borrow()
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &stop));
            if same && let Some(k) = &*ui.import.killer.lock().unwrap_or_else(|e| e.into_inner()) {
                k.kill();
            }
        });
    }

    /// Import: files the ready songs. During lookups, they stop as Stop
    /// stops them, then the songs are filed.
    pub(crate) fn import_file(self: &Rc<Self>) {
        let iv = &self.import;
        let playlist = self.import_playlist();
        match iv.job.get() {
            Job::Filing => {}
            Job::LookingUp => {
                let already = iv.file_after.replace(Some(playlist)).is_some();
                let stop = iv.stop_flag.borrow().clone();
                if let Some(stop) = stop
                    && !already
                {
                    self.import_halt_lookups(stop);
                }
                iv.progress
                    .set_text("Importing once the lookup in progress ends…");
            }
            Job::Idle => self.import_file_now(playlist),
        }
    }

    fn import_file_now(self: &Rc<Self>, playlist: Option<String>) {
        // One thing at a time: a scan or a sync reads the files and the
        // index, and a tag write changes files.
        if self.busy.replace(true) {
            *self.import.refused.borrow_mut() = Some(
                "A scan, sync, adoption or tag write is under way; import once it is done.".into(),
            );
            self.import_update_controls();
            self.import_reload();
            return;
        }
        self.import.refused.replace(None);
        let iv = &self.import;
        let stop = Arc::new(AtomicBool::new(false));
        *iv.stop_flag.borrow_mut() = Some(stop.clone());
        // As during a sync: the window cannot close, and a signal stops
        // filing between files, the window quitting once it is done.
        self.writing.set(true);
        self.cancel.replace(Some(stop.clone()));
        iv.job.set(Job::Filing);
        iv.progress.set_text(&model::progress_line(true, 0, 0, ""));
        self.refresh_device();
        let (tx, rx) = async_channel::unbounded::<(usize, usize, String)>();
        let ui = Rc::downgrade(self);
        {
            let ui = ui.clone();
            glib::spawn_future_local(async move {
                while let Ok((n, total, file)) = rx.recv().await {
                    if let Some(ui) = ui.upgrade() {
                        ui.import
                            .progress
                            .set_text(&model::progress_line(true, n, total, &file));
                    }
                }
            });
        }
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        glib::spawn_future_local(async move {
            let filed_config = config.clone();
            let result = gio::spawn_blocking(move || {
                let progress = move |n: usize, total: usize, file: &str| {
                    let _ = tx.send_blocking((n, total, file.to_owned()));
                };
                import_job::file(&config, &paths, playlist.as_deref(), &progress, &stop)
            })
            .await;
            let Some(ui) = ui.upgrade() else { return };
            let iv = &ui.import;
            ui.busy.set(false);
            ui.writing.set(false);
            ui.cancel.replace(None);
            iv.stop_flag.replace(None);
            iv.job.set(Job::Idle);
            iv.look_after.set(false);
            iv.progress.set_text("");
            ui.refresh_device();
            match joined(result) {
                Ok(report) => {
                    let inbox = filed_config.inbox().unwrap_or_default();
                    let root = filed_config
                        .import_root()
                        .map(PathBuf::from)
                        .unwrap_or_default();
                    let title = match report.filed.len() {
                        0 => "Nothing imported",
                        _ => "Songs imported",
                    };
                    ui.import_report(title, &import_job::describe(&report, &inbox, &root));
                }
                Err(e) => ui.show_error("The songs could not be imported", &e),
            }
            ui.import_reload();
        });
    }

    fn import_report(&self, title: &str, text: &str) {
        let dialog = gtk::Window::builder()
            .title("Import")
            .transient_for(&self.window)
            .modal(true)
            .default_width(640)
            .default_height(420)
            .build();
        let stack = gtk::Stack::new();
        dialog.set_child(Some(&stack));
        report_page(&dialog, &stack, title, text);
        dialog.present();
    }

    /// Add Files… or Add Folders…: copies what is picked into the inbox.
    pub(crate) fn import_pick(self: &Rc<Self>, folders: bool) {
        let dialog = gtk::FileDialog::builder()
            .title(if folders {
                "Add Folders to the Inbox"
            } else {
                "Add Files to the Inbox"
            })
            .modal(true)
            .build();
        let ui = Rc::downgrade(self);
        let done = move |r: Result<gio::ListModel, glib::Error>| {
            let (Some(ui), Ok(list)) = (ui.upgrade(), r) else {
                return;
            };
            let paths: Vec<PathBuf> = (0..list.n_items())
                .filter_map(|i| list.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
                .collect();
            ui.import_add(paths);
        };
        if folders {
            dialog.select_multiple_folders(Some(&self.window), gio::Cancellable::NONE, done);
        } else {
            dialog.open_multiple(Some(&self.window), gio::Cancellable::NONE, done);
        }
    }

    /// Copies files and folders into the inbox on a worker, then shows the
    /// Import view.
    pub(crate) fn import_add(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let config = self.data.config().clone();
        let ui = Rc::downgrade(self);
        self.import.progress.set_text("Copying into the inbox…");
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || import_job::add(&config, &paths)).await;
            let Some(ui) = ui.upgrade() else { return };
            ui.import.progress.set_text("");
            match joined(result) {
                Ok(_) => {
                    ui.import.hold.replace(None);
                    ui.import_watch();
                    ui.import_show();
                }
                Err(e) => ui.show_error("The files could not be copied into the inbox", &e),
            }
        });
    }

    /// Shows the Import view, adding its sidebar row if needed.
    pub(crate) fn import_show(self: &Rc<Self>) {
        let index = self
            .sources
            .borrow()
            .iter()
            .position(|s| matches!(s, Some(Source::Import)));
        match index.and_then(|i| self.sidebar.row_at_index(i as i32)) {
            Some(row) => {
                if self.sidebar.selected_row().as_ref() == Some(&row) {
                    self.show(&Source::Import);
                } else {
                    self.sidebar.select_row(Some(&row));
                }
            }
            None => {
                *self.current.borrow_mut() = Source::Import;
                self.fill_sidebar();
            }
        }
    }

    /// Delete: moves the selected files to the trash.
    pub(crate) fn import_trash_selected(self: &Rc<Self>) {
        if self.import.job.get() == Job::Filing {
            return;
        }
        let rows = self.import_selected();
        let failed: Vec<String> = rows
            .iter()
            .filter_map(|r| {
                gio::File::for_path(&r.path)
                    .trash(gio::Cancellable::NONE)
                    .err()
                    .map(|e| format!("{}: {e}", r.file))
            })
            .collect();
        if !failed.is_empty() {
            self.show_error(
                "Some files could not be moved to the trash",
                &anyhow::anyhow!(failed.join("\n")),
            );
        }
        self.import_reload();
    }

    /// Use This Match: the candidate ticked is the one the file gets; its
    /// genre is asked for when not known yet.
    fn import_choose(self: &Rc<Self>) {
        let Some(item) = self.import_current().and_then(|r| r.item) else {
            return;
        };
        let Some(choice) = self
            .import
            .radios
            .borrow()
            .iter()
            .position(|b| b.is_active())
        else {
            return;
        };
        if let Err(e) = self.data.import_choose(item, choice) {
            return self.show_error("The match could not be chosen", &e);
        }
        self.import_reload();
        self.import_look_up();
    }

    /// Search Again with an artist and title, or Look Up Again with none.
    fn import_search(self: &Rc<Self>, search: Option<Query>) {
        let Some(item) = self.import_current().and_then(|r| r.item) else {
            return;
        };
        let search = search.filter(|q| !q.artist.is_empty() || !q.title.is_empty());
        if let Err(e) = self.data.import_search(item, search.as_ref()) {
            return self.show_error("The file could not be looked up again", &e);
        }
        self.import_reload();
        self.import_look_up();
    }

    fn import_confirm(self: &Rc<Self>, on: bool) {
        let Some(item) = self.import_current().and_then(|r| r.item) else {
            return;
        };
        if let Err(e) = self.data.import_confirm(item, on) {
            return self.show_error("The file could not be changed", &e);
        }
        self.import_reload();
    }

    /// Type Tags…: the Get Info form on the tags the file would get. OK
    /// keeps what was typed for the import; nothing is written to the file.
    pub(crate) fn import_type_tags(self: &Rc<Self>) {
        let Some(r) = self.import_current() else {
            return;
        };
        if matches!(
            r.status,
            Status::AlreadyInLibrary(_) | Status::NotSupported(_)
        ) {
            return;
        }
        let (Some(item), Some(tags), Some(info)) = (r.item, model::proposal(&r), r.info.clone())
        else {
            return;
        };
        let completions = self.data.completions().unwrap_or_default();
        let dialog = info_dialog::InfoDialog::new(&self.window, false, completions);
        let track = TrackRow {
            id: 0,
            path: r.path.clone(),
            path_key: String::new(),
            size: std::fs::metadata(&r.path).map_or(0, |m| m.len()),
            mtime_ns: 0,
            hash: [0; 32],
            info,
            tags_hash: tags.hash(),
            tags: tags.clone(),
            added_at: 0,
            missing_since: None,
            tag_error: None,
            drm: false,
        };
        dialog.fill(&[track]);
        for b in [&dialog.previous, &dialog.next, &dialog.stop] {
            b.set_visible(false);
        }
        {
            let (ui, d) = (Rc::downgrade(self), Rc::downgrade(&dialog));
            dialog.ok.connect_clicked(move |_| {
                let (Some(ui), Some(d)) = (ui.upgrade(), d.upgrade()) else {
                    return;
                };
                let Ok(changes) = d.changes() else { return };
                let typed = model::typed_fields(&model::with_changes(&tags, &changes));
                match ui.data.import_typed(item, &typed) {
                    Ok(()) => {
                        d.window.close();
                        ui.import_reload();
                    }
                    Err(e) => d.say(&format!("{e:#}")),
                }
            });
        }
        {
            let d = Rc::downgrade(&dialog);
            dialog.cancel.connect_clicked(move |_| {
                if let Some(d) = d.upgrade() {
                    d.window.close();
                }
            });
        }
        {
            let keep = RefCell::new(Some(dialog.clone()));
            dialog.window.connect_close_request(move |_| {
                keep.borrow_mut().take();
                glib::Propagation::Proceed
            });
        }
        dialog.window.present();
    }

    /// Show in Library: the library, with the song selected.
    fn import_show_in_library(self: &Rc<Self>, id: i64) {
        let index = self
            .sources
            .borrow()
            .iter()
            .position(|s| matches!(s, Some(Source::Library)));
        let Some(row) = index.and_then(|i| self.sidebar.row_at_index(i as i32)) else {
            return;
        };
        // A search would hide it.
        self.search.set_text("");
        *self.query.borrow_mut() = crate::view_model::Query::default();
        self.filter.changed(gtk::FilterChange::LessStrict);
        self.sidebar.select_row(Some(&row));
        if let Some(at) = self.view_position(id, None) {
            self.selection.select_item(at, true);
            self.view.scroll_to(
                at,
                None,
                gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                None,
            );
            self.view.grab_focus();
        }
    }

    /// The sidebar's Import row follows the number of files in the inbox:
    /// added or removed as it fills or empties, but kept while shown.
    pub(crate) fn import_count_changed(self: &Rc<Self>, n: usize) {
        let has = self.import.sidebar_row.borrow().is_some();
        let shown = *self.current.borrow() == Source::Import;
        if model::import_row_wanted(n, shown) != has {
            self.fill_sidebar();
            return;
        }
        if let Some(label) = self
            .import
            .sidebar_row
            .borrow()
            .as_ref()
            .and_then(|row| row.child())
            .and_downcast::<gtk::Label>()
        {
            label.set_text(&model::sidebar_label(n));
        }
    }

    /// Watches the inbox, once it exists: files put there by other programs
    /// show at once.
    pub(crate) fn import_watch(self: &Rc<Self>) {
        if self.import.monitor.borrow().is_some() {
            return;
        }
        let Some(dir) = self.data.config().inbox().filter(|d| d.is_dir()) else {
            return;
        };
        let Ok(monitor) = gio::File::for_path(&dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            return;
        };
        let ui = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, _| {
            let Some(ui) = ui.upgrade() else { return };
            if ui.import.reload_pending.replace(true) {
                return;
            }
            let ui = Rc::downgrade(&ui);
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                let Some(ui) = ui.upgrade() else { return };
                ui.import.reload_pending.set(false);
                if matches!(*ui.current.borrow(), Source::Import) {
                    ui.import_reload();
                } else {
                    ui.import_count_changed(import_job::count(ui.data.config()));
                }
            });
        });
        *self.import.monitor.borrow_mut() = Some(monitor);
    }

    /// Files dropped on the window go to the inbox.
    pub(crate) fn import_drop_target(self: &Rc<Self>) -> gtk::DropTarget {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let ui = Rc::downgrade(self);
        target.connect_drop(move |_, value, _, _| {
            let (Some(ui), Ok(files)) = (ui.upgrade(), value.get::<gdk::FileList>()) else {
                return false;
            };
            let paths: Vec<PathBuf> = files.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            ui.import_add(paths);
            true
        });
        target
    }
}
