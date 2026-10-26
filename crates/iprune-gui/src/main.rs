//! The iprune window: a sidebar of sources and a grid of tracks, with
//! playlist editing. Every edit goes through `iprune-core`, as the CLI does,
//! and is written to the `.m3u8` file at once.

mod adopt_dialog;
mod adopt_job;
#[cfg(test)]
mod close_tests;
mod data;
mod info_dialog;
mod scan_job;
mod selection_dialog;
mod style;
mod sync_job;
mod tag_job;
mod view_model;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use iprune_core::notify;

use data::{Data, DeviceInfo};
use view_model::{Column, Layout, Layouts, Row};

const APP_ID: &str = "dev.iprune.Iprune";

/// Set by SIGTERM, SIGHUP or SIGINT, see `build`.
static SIGNALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// What a drag from the grid carries: a value of a private type, so no other
/// widget (the search field) accepts it. The rows themselves stay in [`Ui`].
struct DragMarker;

fn drag_value() -> glib::Value {
    glib::BoxedAnyObject::new(DragMarker).to_value()
}

fn is_drag(value: &glib::Value) -> bool {
    value
        .get::<glib::BoxedAnyObject>()
        .is_ok_and(|b| b.try_borrow::<DragMarker>().is_ok())
}

fn drop_target() -> gtk::DropTarget {
    gtk::DropTarget::new(
        glib::BoxedAnyObject::static_type(),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    )
}

/// What the grid is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Library,
    Playlist(String),
}

/// What a playlist picked in the picker is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// Add to Playlist: the selected tracks go at its end.
    Add,
    /// Show in Playlist: it is shown with this library track selected.
    Show(i64),
}

/// What an open Get Info form edits.
struct InfoState {
    /// The tracks it writes to.
    ids: RefCell<Vec<i64>>,
    /// The row shown, for one song, in the grid's order.
    at: Cell<Option<u32>>,
    /// The running write's Stop flag.
    stop: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
}

/// Rows being dragged.
#[derive(Debug, Clone)]
struct Drag {
    from: Source,
    /// 1-based playlist positions, when dragged from a playlist.
    positions: Vec<usize>,
    /// The files of the rows that have one.
    paths: Vec<PathBuf>,
}

struct Ui {
    window: gtk::ApplicationWindow,
    data: Data,
    store: gio::ListStore,
    filter: gtk::CustomFilter,
    query: RefCell<view_model::Query>,
    view: gtk::ColumnView,
    selection: gtk::MultiSelection,
    columns: Vec<(Column, gtk::ColumnViewColumn)>,
    status: gtk::Label,
    filtered: gtk::FilterListModel,
    title: gtk::Label,
    /// The grid's search field.
    search: gtk::SearchEntry,
    sidebar: gtk::ListBox,
    sources: RefCell<Vec<Option<Source>>>,
    current: RefCell<Source>,
    drag: RefCell<Option<Drag>>,
    /// The playlist a sidebar context menu was opened on.
    context_playlist: RefCell<Option<String>>,
    layouts: RefCell<Layouts>,
    /// Whether the columns now follow the playlist layout.
    playlist_layout: Cell<bool>,
    reload_pending: Cell<bool>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    /// The modification time of the playlist file on screen when it was
    /// read, to notice a change made by any other program, `iprune` included.
    shown_mtime: Cell<Option<i64>>,
    /// The grid's right-click menu and the actions only a playlist enables.
    row_menu: gtk::PopoverMenu,
    playlist_actions: RefCell<Vec<gio::SimpleAction>>,
    /// Show in Playlist, enabled when another playlist holds the track the
    /// menu was opened on, `menu_track`.
    show_in_action: RefCell<Option<gio::SimpleAction>>,
    menu_track: Cell<Option<i64>>,
    /// Get Info, enabled when a row the menu acts on has a file.
    info_action: RefCell<Option<gio::SimpleAction>>,
    /// The sidebar's iPod panel and the actions on the iPod.
    device_box: gtk::Box,
    scan_action: RefCell<Option<gio::SimpleAction>>,
    sync_action: RefCell<Option<gio::SimpleAction>>,
    adopt_action: RefCell<Option<gio::SimpleAction>>,
    selection_action: RefCell<Option<gio::SimpleAction>>,
    /// The iPod the sidebar shows, as last read.
    device: RefCell<Option<DeviceInfo>>,
    eject_action: RefCell<Option<gio::SimpleAction>>,
    /// The sidebar's playlist filter.
    side_query: RefCell<view_model::Query>,
    /// The Add to Playlist picker: a filter over a list of the playlists,
    /// filled when it opens, at the point the grid menu was opened.
    picker: gtk::Popover,
    picker_search: gtk::SearchEntry,
    picker_list: gtk::ListBox,
    picker_names: RefCell<Vec<String>>,
    picker_query: RefCell<view_model::Query>,
    pick_mode: Cell<Pick>,
    menu_point: Cell<(i32, i32)>,
    /// Sidebar and grid; its position is the sidebar's width.
    paned: gtk::Paned,
    /// True while a sync writes to the iPod: the window cannot close.
    writing: Cell<bool>,
    /// The running sync's Stop flag, which a SIGTERM, SIGHUP or SIGINT sets.
    cancel: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    /// True while a sync or adoption dialog is open, its worker still
    /// runs, or an eject is under way: one thing at a time with the iPod.
    busy: Cell<bool>,
    /// True while the plan's worker runs (it scans and writes the index).
    planning: Cell<bool>,
    /// Kept so its mount signals keep coming.
    volumes: RefCell<Option<gio::VolumeMonitor>>,
    /// Set when the playlists folder cannot be watched; kept in the status.
    unwatched: Cell<bool>,
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| style::install());
    app.connect_activate(activate);
    // GTK does not see iprune's own arguments.
    app.run_with_args::<&str>(&[])
}

fn activate(app: &gtk::Application) {
    // Started again, from the launcher or a key binding: the running
    // instance gets the activation and shows its window.
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("iprune")
        .default_width(1280)
        .default_height(800)
        .build();
    match Data::open() {
        Ok(data) => build(&window, data),
        Err(e) => {
            let label = gtk::Label::new(Some(&format!("{e:#}")));
            label.set_wrap(true);
            label.set_margin_top(24);
            label.set_margin_bottom(24);
            label.set_margin_start(24);
            label.set_margin_end(24);
            window.set_child(Some(&label));
        }
    }
    window.present();
}

fn layouts_file() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "iprune")?;
    Some(dirs.state_dir()?.join("ui.toml"))
}

fn load_layouts() -> Layouts {
    layouts_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| Layouts::parse(&t))
        .unwrap_or_default()
}

fn cell_label(numeric: bool) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(if numeric { 1.0 } else { 0.0 });
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_single_line_mode(true);
    l.add_css_class("cell-label");
    l
}

fn row_of(obj: &glib::Object) -> std::cell::Ref<'_, Row> {
    obj.downcast_ref::<glib::BoxedAnyObject>()
        .expect("the grid holds rows")
        .borrow::<Row>()
}

fn date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix)
        .and_then(|d| d.format("%Y-%m-%d"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

type UiSlot = Rc<OnceCell<Weak<Ui>>>;

fn ui_of(slot: &UiSlot) -> Option<Rc<Ui>> {
    slot.get().and_then(Weak::upgrade)
}

fn make_column(col: Column, slot: UiSlot) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = cell_label(col.numeric());
        let weak_item = item.downgrade();

        // Rows are dragged from any cell: the selection if the row is in it,
        // else the row alone.
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
        {
            let slot = slot.clone();
            let weak_item = weak_item.clone();
            source.connect_prepare(move |_, _, _| {
                let ui = ui_of(&slot)?;
                let item = weak_item.upgrade()?;
                ui.begin_drag(item.position());
                Some(gdk::ContentProvider::for_value(&drag_value()))
            });
        }
        label.add_controller(source);

        // Dropping on a row of the same playlist reorders it.
        let target = drop_target();
        {
            let slot = slot.clone();
            let weak_item = weak_item.clone();
            target.connect_drop(move |target, value, _, y| {
                let (Some(ui), Some(item)) = (ui_of(&slot), weak_item.upgrade()) else {
                    return false;
                };
                if !is_drag(value) {
                    return false;
                }
                let Some(position) = item.item().and_then(|o| row_of(&o).position) else {
                    return false;
                };
                let height = f64::from(target.widget().map_or(1, |w| w.height()));
                ui.drop_on_row(view_model::drop_before(position, y, height))
            });
        }
        label.add_controller(target);

        // Right-click: the menu acts on the row clicked, selected first if
        // it is not part of the selection.
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        {
            let slot = slot.clone();
            let label = label.downgrade();
            click.connect_pressed(move |_, _, x, y| {
                if let (Some(ui), Some(item), Some(label)) =
                    (ui_of(&slot), weak_item.upgrade(), label.upgrade())
                {
                    ui.open_row_menu(item.position(), &label, x, y);
                }
            });
        }
        label.add_controller(click);
        item.set_child(Some(&label));
        // Rows above added, removed or sorted move this one without a rebind.
        {
            let label = label.downgrade();
            item.connect_position_notify(move |item| {
                if let Some(label) = label.upgrade() {
                    stripe(item, &label);
                }
            });
        }
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = item.child().and_downcast::<gtk::Label>().expect("a label");
        let obj = item.item().expect("a row");
        let row = row_of(&obj);
        let text = match col.time(&row) {
            Some(t) => date(t),
            None => col.text(&row),
        };
        label.set_text(&text);
        // An entry with no file behind it is shown dimmed.
        if row.unresolved {
            label.add_css_class("unresolved");
        } else {
            label.remove_css_class("unresolved");
        }
        if col == Column::Name {
            label.set_tooltip_text(Some(&row.path.to_string_lossy()));
        }
        stripe(item, &label);
    });
    let column = gtk::ColumnViewColumn::new(Some(col.title()), Some(factory));
    column.set_id(Some(col.key()));
    column.set_resizable(true);
    column.set_fixed_width(col.width());
    column.set_visible(col.visible_by_default());
    column.set_expand(col == Column::Name);
    let sorter = gtk::CustomSorter::new(move |a, b| col.compare(&row_of(a), &row_of(b)).into());
    column.set_sorter(Some(&sorter));
    column
}

/// Shades every other grid row, by its position in the list. CSS
/// `:nth-child` would follow the row widgets, which GTK recycles and
/// reorders while scrolling, so the stripes would jump. The class goes on
/// each cell's label, which fills its cell (see `style.css`): restyling
/// GTK's own row widget while it binds leaves the grid empty.
fn stripe(item: &gtk::ListItem, label: &gtk::Label) {
    if style::is_odd_row(item.position()) {
        label.add_css_class("odd");
    } else {
        label.remove_css_class("odd");
    }
}

fn sidebar_header(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("sidebar-heading");
    label.set_margin_top(12);
    label.set_margin_start(8);
    label.set_margin_bottom(4);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_selectable(false);
    row.set_activatable(false);
    row
}

fn sidebar_item(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_margin_start(16);
    label.set_margin_end(8);
    label.set_margin_top(3);
    label.set_margin_bottom(3);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row
}

fn device_text(d: &DeviceInfo) -> String {
    let mut lines = vec![d.name.clone()];
    if d.adopted {
        lines.push(format!("{} tracks managed", d.mapped_tracks));
    } else {
        lines.push("Adopt matches its tracks to the library".into());
    }
    match (d.free, d.capacity) {
        (Some(free), Some(cap)) => lines.push(format!(
            "{} free of {}",
            iprune_core::sync::bytes(free),
            iprune_core::sync::bytes(cap)
        )),
        _ if !d.connected => lines.push("not connected".into()),
        _ => {}
    }
    if let Some(selection) = &d.selection {
        lines.push(selection.clone());
    }
    if d.adopted {
        lines.push(match d.last_sync {
            Some(t) => format!("last sync {}", date(t)),
            None => "never synced".into(),
        });
    }
    lines.join("\n")
}

/// Asks for a playlist name in a small modal window and calls `done` with
/// it, trimmed and not empty.
fn ask_name(
    parent: &gtk::ApplicationWindow,
    title: &str,
    prompt: &str,
    initial: &str,
    done: impl Fn(String) + 'static,
) {
    let dialog = gtk::Window::builder()
        .title(title)
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(360)
        .build();
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    let ok = gtk::Button::with_label("OK");
    ok.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&ok);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(16);
    body.set_margin_end(16);
    let label = gtk::Label::new(Some(prompt));
    label.set_xalign(0.0);
    body.append(&label);
    body.append(&entry);
    body.append(&buttons);
    dialog.set_child(Some(&body));
    dialog.set_default_widget(Some(&ok));
    let done = Rc::new(done);
    {
        let (dialog, entry, done) = (dialog.downgrade(), entry.clone(), done.clone());
        ok.connect_clicked(move |_| {
            let name = entry.text().trim().to_owned();
            if !name.is_empty() {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
                done(name);
            }
        });
    }
    closes(&cancel, &dialog);
    dialog.present();
    entry.grab_focus();
}

impl Ui {
    fn show_error(&self, what: &str, e: &anyhow::Error) {
        gtk::AlertDialog::builder()
            .message(what)
            .detail(format!("{e:#}"))
            .modal(true)
            .build()
            .show(Some(&self.window));
    }

    fn playlist_name(&self) -> Option<String> {
        match &*self.current.borrow() {
            Source::Playlist(name) => Some(name.clone()),
            Source::Library => None,
        }
    }

    /// The selected rows, in view order.
    fn selected_rows(&self) -> Vec<Row> {
        let bits = self.selection.selection();
        let mut out = Vec::new();
        for i in 0..bits.size() {
            let pos = bits.nth(i as u32);
            if let Some(obj) = self.selection.item(pos) {
                out.push(row_of(&obj).clone());
            }
        }
        out
    }

    fn selected_positions(&self) -> Vec<usize> {
        self.selected_rows()
            .iter()
            .filter_map(|r| r.position)
            .collect()
    }

    /// Selects the rows at these playlist positions and shows the first.
    fn select_positions(&self, positions: &[usize]) {
        self.selection.unselect_all();
        let wanted: HashSet<usize> = positions.iter().copied().collect();
        let mut first = None;
        for i in 0..self.selection.n_items() {
            let hit = self
                .selection
                .item(i)
                .is_some_and(|o| row_of(&o).position.is_some_and(|p| wanted.contains(&p)));
            if hit {
                self.selection.select_item(i, false);
                first.get_or_insert(i);
            }
        }
        if let Some(i) = first {
            self.view
                .scroll_to(i, None, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn begin_drag(&self, view_position: u32) {
        if !self.selection.is_selected(view_position) {
            self.selection.select_item(view_position, true);
        }
        let rows = self.selected_rows();
        *self.drag.borrow_mut() = Some(Drag {
            from: self.current.borrow().clone(),
            positions: rows.iter().filter_map(|r| r.position).collect(),
            paths: rows
                .iter()
                .filter(|r| !r.unresolved)
                .map(|r| r.path.clone())
                .collect(),
        });
    }

    fn sorted_by_position(&self) -> bool {
        let Some(sorter) = self.view.sorter().and_downcast::<gtk::ColumnViewSorter>() else {
            return false;
        };
        let position = self.columns.iter().find(|(c, _)| *c == Column::Position);
        sorter.primary_sort_column().as_ref() == position.map(|(_, c)| c)
            && sorter.primary_sort_order() == gtk::SortType::Ascending
    }

    /// True when the playlist file is still the one on screen. Otherwise
    /// shows it again and says so: positions taken from a stale view would
    /// land on other entries.
    fn ensure_fresh(&self, name: &str) -> bool {
        if self.data.playlist_mtime(name) == self.shown_mtime.get() {
            return true;
        }
        self.refresh();
        self.status
            .set_text("The playlist changed on disk and is shown again; nothing was changed");
        false
    }

    /// After a failed edit the view may be stale: show the file again.
    fn edit_failed(&self, what: &str, e: &anyhow::Error) {
        self.refresh();
        self.show_error(what, e);
    }

    /// A drop on a row of the playlist shown: reorders it when the rows
    /// come from it and it is sorted by `#`.
    fn drop_on_row(&self, before: usize) -> bool {
        let Some(name) = self.playlist_name() else {
            return false;
        };
        let Some(drag) = self.drag.borrow_mut().take() else {
            return false;
        };
        if drag.from != Source::Playlist(name.clone()) {
            self.status
                .set_text("Drop tracks on a playlist in the sidebar to add them");
            return false;
        }
        if !self.sorted_by_position() {
            self.status
                .set_text("Sort by # (ascending) to reorder the playlist");
            return false;
        }
        self.move_entries(&name, &drag.positions, before);
        true
    }

    fn move_entries(&self, name: &str, positions: &[usize], before: usize) {
        if !self.ensure_fresh(name) {
            return;
        }
        let len = self.store.n_items().try_into().unwrap_or(usize::MAX);
        let order = match iprune_core::playlists::reorder(len, positions, before) {
            Ok(o) => o,
            Err(_) => return,
        };
        if order.iter().copied().eq(1..=len) {
            return;
        }
        match self.data.move_entries(name, positions, before) {
            Ok(()) => {
                self.refresh();
                self.select_positions(&view_model::new_positions(&order, positions));
            }
            Err(e) => self.edit_failed("The playlist was not changed", &e),
        }
    }

    /// Moves the selected entries one place up (`-1`) or down (`+1`). Only
    /// adjacent entries: a scattered selection would be gathered instead.
    fn nudge(&self, step: i32) {
        let Some(name) = self.playlist_name() else {
            return;
        };
        let positions = self.selected_positions();
        let len = self.store.n_items() as usize;
        match view_model::nudge_before(&positions, len, step < 0) {
            view_model::Nudge::Before(before) => self.move_entries(&name, &positions, before),
            view_model::Nudge::NotAdjacent => self
                .status
                .set_text("Select adjacent entries to move them with Ctrl+Up and Ctrl+Down"),
            view_model::Nudge::Nothing => {}
        }
    }

    /// Removes the selected entries; more than one asks first, since the
    /// file is rewritten at once and there is no undo.
    fn remove_selected(self: &Rc<Self>) {
        let Some(name) = self.playlist_name() else {
            return;
        };
        let positions = self.selected_positions();
        match positions.len() {
            0 => {}
            1 => self.remove_entries(&name, &positions),
            n => {
                let dialog = gtk::AlertDialog::builder()
                    .message(format!("Remove {n} entries from {name:?}?"))
                    .detail("The tracks stay in the library.")
                    .buttons(["Cancel", "Remove"])
                    .cancel_button(0)
                    .default_button(0)
                    .modal(true)
                    .build();
                let ui = Rc::downgrade(self);
                dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |answer| {
                    if let (Some(ui), Ok(1)) = (ui.upgrade(), answer) {
                        ui.remove_entries(&name, &positions);
                    }
                });
            }
        }
    }

    fn remove_entries(&self, name: &str, positions: &[usize]) {
        if !self.ensure_fresh(name) {
            return;
        }
        let first = *positions.iter().min().expect("not empty");
        match self.data.remove(name, positions) {
            Ok(()) => {
                self.refresh();
                // Keep the place: select what now sits where the first was.
                let len = self.store.n_items() as usize;
                if len > 0 {
                    self.select_positions(&[first.min(len)]);
                }
                self.status.set_text(&match positions.len() {
                    1 => format!("Removed 1 entry from {name}"),
                    n => format!("Removed {n} entries from {name}"),
                });
            }
            Err(e) => self.edit_failed("The playlist was not changed", &e),
        }
    }

    /// Adds the dragged files at the end of `target`. The playlist on screen
    /// is the drag's source, never the target, so nothing to refresh.
    fn drop_on_playlist(&self, target: &str) -> bool {
        let Some(drag) = self.drag.borrow_mut().take() else {
            return false;
        };
        if drag.from == Source::Playlist(target.to_owned()) || drag.paths.is_empty() {
            return false;
        }
        match self.data.add(target, &drag.paths, None) {
            Ok(()) => {
                self.status.set_text(&match drag.paths.len() {
                    1 => format!("Added 1 track to {target}"),
                    n => format!("Added {n} tracks to {target}"),
                });
                true
            }
            Err(e) => {
                self.show_error(&format!("Nothing was added to {target}"), &e);
                false
            }
        }
    }

    /// Opens the grid's menu on the row at view position `pos`.
    fn open_row_menu(&self, pos: u32, cell: &gtk::Label, x: f64, y: f64) {
        if !self.selection.is_selected(pos) {
            self.selection.select_item(pos, true);
        }
        let current = self.playlist_name();
        for a in self.playlist_actions.borrow().iter() {
            a.set_enabled(current.is_some());
        }
        // Show in Playlist acts on the row clicked, even among several.
        let track = self.selection.item(pos).and_then(|o| row_of(&o).track_id);
        self.menu_track.set(track);
        if let Some(a) = &*self.show_in_action.borrow() {
            a.set_enabled(track.is_some_and(|id| !self.playlists_holding(id).is_empty()));
        }
        if let Some(a) = &*self.info_action.borrow() {
            a.set_enabled(!self.info_ids().is_empty());
        }
        let at = cell
            .compute_point(&self.view, &gtk::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
        self.menu_point.set((at.x() as i32, at.y() as i32));
        self.row_menu.set_pointing_to(Some(&gdk::Rectangle::new(
            at.x() as i32,
            at.y() as i32,
            1,
            1,
        )));
        self.row_menu.popup();
    }

    /// The playlists holding the library track `id`, but the one shown.
    fn playlists_holding(&self, id: i64) -> Vec<String> {
        let holding = self.data.playlists_holding(id).unwrap_or_default();
        view_model::other_playlists(holding, self.playlist_name().as_deref())
    }

    /// Opens the picker where the grid menu was, the filter empty and
    /// focused: for Add to Playlist every playlist but the one shown, for
    /// Show in Playlist those holding the track but the one shown.
    fn open_picker(&self, pick: Pick) {
        let current = self.playlist_name();
        while let Some(row) = self.picker_list.row_at_index(0) {
            self.picker_list.remove(&row);
        }
        let names: Vec<String> = match pick {
            Pick::Add => self
                .data
                .playlist_names()
                .unwrap_or_default()
                .into_iter()
                .filter(|n| Some(n) != current.as_ref())
                .collect(),
            Pick::Show(id) => self.playlists_holding(id),
        };
        self.pick_mode.set(pick);
        for name in &names {
            self.picker_list.append(&sidebar_item(name));
        }
        *self.picker_names.borrow_mut() = names;
        self.picker_search.set_text("");
        *self.picker_query.borrow_mut() = view_model::Query::default();
        self.picker_list.invalidate_filter();
        self.select_first_pick();
        let (x, y) = self.menu_point.get();
        self.picker
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        self.picker.popup();
        self.picker_search.grab_focus();
    }

    /// The first playlist the picker's filter keeps, selected so Enter
    /// adds to it.
    fn select_first_pick(&self) -> Option<gtk::ListBoxRow> {
        let query = self.picker_query.borrow();
        let names = self.picker_names.borrow();
        let first = (0..names.len())
            .find(|&i| query.matches_name(&names[i]))
            .and_then(|i| self.picker_list.row_at_index(i as i32));
        self.picker_list.select_row(first.as_ref());
        first
    }

    /// Adds the selected tracks to the picker's playlist at `row`, or
    /// shows that playlist with the track selected.
    fn pick(&self, row: &gtk::ListBoxRow) {
        let name = self
            .picker_names
            .borrow()
            .get(row.index() as usize)
            .cloned();
        if let Some(name) = name {
            self.picker.popdown();
            match self.pick_mode.get() {
                Pick::Add => self.add_selected_to(&name),
                Pick::Show(id) => self.show_track_in(&name, id),
            }
        }
    }

    /// Shows the playlist `name` with every entry of the library track `id`
    /// selected, the first one scrolled to.
    fn show_track_in(&self, name: &str, id: i64) {
        let index = self
            .sources
            .borrow()
            .iter()
            .position(|s| matches!(s, Some(Source::Playlist(n)) if n == name));
        let Some(row) = index.and_then(|i| self.sidebar.row_at_index(i as i32)) else {
            return;
        };
        // A search would hide the track there. The query is reset here
        // rather than by the field's signal, which may come later.
        self.search.set_text("");
        *self.query.borrow_mut() = view_model::Query::default();
        self.filter.changed(gtk::FilterChange::LessStrict);
        // Selecting it shows it, at once; the sidebar's filter keeps the
        // playlist on screen, so it is shown there too. The focus scrolls
        // the sidebar to it, then goes to the grid.
        self.sidebar.select_row(Some(&row));
        self.sidebar.invalidate_filter();
        row.grab_focus();
        let objects: Vec<glib::Object> = (0..self.selection.n_items())
            .filter_map(|i| self.selection.item(i))
            .collect();
        let rows: Vec<std::cell::Ref<'_, Row>> = objects.iter().map(row_of).collect();
        let positions = view_model::positions_of_track(rows.iter().map(|r| &**r), id);
        drop(rows);
        self.select_positions(&positions);
        self.view.grab_focus();
    }

    /// The background colour picked from the menu, if any. A value the file
    /// holds but that is not `#rrggbb` is ignored.
    fn background(&self) -> Option<style::Rgb> {
        let layouts = self.layouts.borrow();
        layouts.background.as_deref().and_then(style::Rgb::parse)
    }

    fn choose_background(self: &Rc<Self>) {
        let dialog = gtk::ColorDialog::new();
        dialog.set_title("Background Colour");
        dialog.set_with_alpha(false);
        let current = self.background().map(style::Rgb::to_rgba);
        let weak = Rc::downgrade(self);
        dialog.choose_rgba(
            Some(&self.window),
            current.as_ref(),
            None::<&gio::Cancellable>,
            move |result| {
                // Cancelled or closed: the background stays as it was.
                if let (Some(ui), Ok(color)) = (weak.upgrade(), result) {
                    ui.set_background(Some(style::Rgb::from_rgba(&color)));
                }
            },
        );
    }

    /// Repaints the windows on `background` (None: the theme's colours)
    /// and keeps it in `ui.toml` for the next start.
    fn set_background(&self, background: Option<style::Rgb>) {
        style::set_background(background);
        self.layouts.borrow_mut().background = background.map(style::Rgb::hex);
        self.save_layouts();
    }

    fn show_in_folder(&self) {
        let Some(row) = self.selected_rows().into_iter().find(|r| !r.unresolved) else {
            return;
        };
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&row.path)));
        launcher.open_containing_folder(Some(&self.window), gio::Cancellable::NONE, |_| {});
    }

    /// The columns' current order, widths and visibility.
    fn capture_layout(&self) -> Layout {
        let model = self.view.columns();
        let columns = (0..model.n_items())
            .filter_map(|i| model.item(i).and_downcast::<gtk::ColumnViewColumn>())
            .filter_map(|c| {
                Some(view_model::ColumnState {
                    id: c.id()?.to_string(),
                    width: c.fixed_width(),
                    visible: c.is_visible(),
                })
            })
            .collect();
        Layout { columns }
    }

    fn apply_layout(&self, layout: &Layout) {
        for (i, state) in layout.columns.iter().enumerate() {
            let Some(col) = Column::from_key(&state.id) else {
                continue;
            };
            let Some((_, c)) = self.columns.iter().find(|(k, _)| *k == col) else {
                continue;
            };
            self.view.insert_column(i as u32, c);
            c.set_fixed_width(state.width);
            c.set_visible(state.visible);
        }
    }

    /// Keeps the layout of the kind of view on screen, and switches to the
    /// other kind's when `playlist` differs.
    fn switch_layout(&self, playlist: bool) {
        if self.playlist_layout.get() == playlist {
            return;
        }
        let now = self.capture_layout();
        {
            let mut layouts = self.layouts.borrow_mut();
            match self.playlist_layout.get() {
                true => layouts.playlist = now,
                false => layouts.library = now,
            }
        }
        let next = {
            let layouts = self.layouts.borrow();
            match playlist {
                true => layouts.playlist.complete(true),
                false => layouts.library.complete(false),
            }
        };
        self.apply_layout(&next);
        self.playlist_layout.set(playlist);
        // Written now too, not only on close: a crash keeps the layout.
        self.save_layouts();
    }

    fn save_layouts(&self) {
        let now = self.capture_layout();
        let mut layouts = self.layouts.borrow().clone();
        layouts.note_sidebar(self.paned.position());
        match self.playlist_layout.get() {
            true => layouts.playlist = now,
            false => layouts.library = now,
        }
        let (Some(path), Ok(text)) = (layouts_file(), toml::to_string(&layouts)) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }

    /// Switches the grid to `source`: its layout, its default sort (`#` for
    /// a playlist, artist for the library), then its rows.
    fn show(&self, source: &Source) {
        *self.current.borrow_mut() = source.clone();
        self.title.set_text(match source {
            Source::Library => "Music",
            Source::Playlist(name) => name,
        });
        let in_playlist = matches!(source, Source::Playlist(_));
        let sort_by = match source {
            Source::Library => Column::Artist,
            Source::Playlist(_) => Column::Position,
        };
        // Sort order first, then the rows, so they are sorted once.
        self.store.remove_all();
        self.switch_layout(in_playlist);
        if let Some((_, c)) = self.columns.iter().find(|(c, _)| *c == sort_by) {
            // Cleared first, or the previous column stays as a second key
            // with its own arrow.
            self.view.sort_by_column(None, gtk::SortType::Ascending);
            self.view.sort_by_column(Some(c), gtk::SortType::Ascending);
        }
        self.load_rows();
    }

    /// Reads the current source's rows again, keeping the sort and layout
    /// the user chose: after an edit or an outside change.
    fn refresh(&self) {
        self.load_rows();
    }

    fn load_rows(&self) {
        let source = self.current.borrow().clone();
        self.shown_mtime.set(match &source {
            Source::Playlist(name) => self.data.playlist_mtime(name),
            Source::Library => None,
        });
        let rows = match &source {
            Source::Library => self.data.library(),
            Source::Playlist(name) => self.data.playlist(name),
        };
        self.store.remove_all();
        match rows {
            Ok(rows) => {
                let objects: Vec<glib::Object> = rows
                    .into_iter()
                    .map(|r| glib::BoxedAnyObject::new(r).upcast())
                    .collect();
                self.store.extend_from_slice(&objects);
                self.update_status();
            }
            // Never leave the previous rows under the new title.
            Err(e) => self.status.set_text(&format!("{e:#}")),
        }
    }

    fn update_status(&self) {
        let n = self.filtered.n_items();
        let objects: Vec<glib::Object> = (0..n).filter_map(|i| self.filtered.item(i)).collect();
        let borrowed: Vec<std::cell::Ref<'_, Row>> = objects.iter().map(row_of).collect();
        let summary = view_model::summary(borrowed.iter().map(|r| &**r));
        // Problems with the playlists folder stay visible: the playlists
        // shown may be out of date.
        let mut text = summary;
        if let Some(w) = &self.data.warning {
            text = format!("{text}  |  {w}");
        }
        if self.unwatched.get() {
            text = format!("{text}  |  changes made elsewhere show after a restart");
        }
        self.status.set_text(&text);
    }

    /// Shows the iPod's state in the sidebar and enables Sync and Eject for
    /// an adopted iPod that is mounted, Adopt and Eject for one not adopted.
    /// Scan needs no iPod; like them, it waits for whatever is running.
    fn refresh_device(&self) {
        while let Some(child) = self.device_box.first_child() {
            self.device_box.remove(&child);
        }
        let device = self.data.device().ok().flatten();
        *self.device.borrow_mut() = device.clone();
        let free = !self.busy.get();
        let connected = device.as_ref().is_some_and(|d| d.connected);
        let adopted = device.as_ref().is_some_and(|d| d.adopted);
        for (action, on) in [
            (&self.scan_action, true),
            (&self.sync_action, connected && adopted),
            (&self.adopt_action, connected && !adopted),
            (&self.eject_action, connected),
            (
                &self.selection_action,
                device.as_ref().is_some_and(|d| d.id.is_some()),
            ),
        ] {
            if let Some(a) = &*action.borrow() {
                a.set_enabled(on && free);
            }
        }
        let Some(d) = device else {
            return;
        };
        let heading = gtk::Label::new(Some("Device"));
        heading.set_xalign(0.0);
        heading.add_css_class("sidebar-heading");
        heading.set_margin_top(12);
        heading.set_margin_start(8);
        let label = gtk::Label::new(Some(&device_text(&d)));
        label.set_xalign(0.0);
        label.set_margin_start(16);
        label.set_margin_top(4);
        label.set_margin_bottom(12);
        if !d.connected {
            label.add_css_class("dim-label");
        }
        self.device_box.append(&heading);
        self.device_box.append(&label);
        if d.connected || d.id.is_some() {
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            buttons.set_margin_start(12);
            buttons.set_margin_bottom(12);
            if d.id.is_some() {
                let selection = gtk::Button::with_label("Selection…");
                selection.set_tooltip_text(Some("Choose what this iPod syncs"));
                selection.set_action_name(Some("win.selection"));
                buttons.append(&selection);
            }
            if !d.connected {
                self.device_box.append(&buttons);
                return;
            }
            if !d.adopted {
                let adopt = gtk::Button::with_label("Adopt…");
                adopt.set_tooltip_text(Some("Match the tracks on the iPod to library files"));
                adopt.set_action_name(Some("win.adopt"));
                buttons.append(&adopt);
            }
            let eject = gtk::Button::with_label("Eject");
            eject.set_tooltip_text(Some("Unmount the iPod and power it off"));
            eject.set_action_name(Some("win.eject"));
            buttons.append(&eject);
            self.device_box.append(&buttons);
        }
    }

    /// Opens the dialog that sets what the iPod in the sidebar syncs.
    fn choose_selection(self: &Rc<Self>) {
        let Some(d) = self.device.borrow().clone() else {
            return;
        };
        let Some(id) = d.id else { return };
        let input = self.data.selection(id).and_then(|(current, _)| {
            let (library, playlists) = self.data.selection_inputs()?;
            Ok(selection_dialog::Input {
                ipod_name: d.name.clone(),
                capacity: d.capacity,
                current,
                library,
                playlists,
            })
        });
        let input = match input {
            Ok(i) => i,
            Err(e) => return self.show_error("The selection could not be read", &e),
        };
        let ui = Rc::downgrade(self);
        selection_dialog::show(&self.window, input, move |s| {
            let Some(ui) = ui.upgrade() else { return };
            if let Err(e) = ui.data.set_selection(id, &s) {
                ui.show_error("The selection was not saved", &e);
            }
            ui.refresh_device();
        });
    }

    /// Unmounts the iPod and powers it off, on a worker.
    fn eject(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.refresh_device();
        self.status.set_text("Ejecting the iPod…");
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || -> anyhow::Result<Option<String>> {
                // Not while the CLI writes the iPod.
                let _lock = iprune_core::lock::ipod(&paths)?;
                let mountpoint = iprune_core::device::mountpoint(&config)?;
                Ok(iprune_core::device::eject(&mountpoint)?)
            })
            .await;
            let Some(ui) = ui.upgrade() else { return };
            ui.busy.set(false);
            match result.unwrap_or_else(|_| Err(anyhow::anyhow!("the eject worker crashed"))) {
                Ok(None) => ui.status.set_text("The iPod can be unplugged"),
                Ok(Some(warning)) => ui.status.set_text(&warning),
                Err(e) => {
                    ui.update_status();
                    ui.show_error("The iPod was not ejected", &e);
                }
            }
            ui.refresh_device();
        });
    }

    /// Adds the selected tracks that have a file at the end of `target`.
    fn add_selected_to(&self, target: &str) {
        let paths: Vec<PathBuf> = self
            .selected_rows()
            .into_iter()
            .filter(|r| !r.unresolved)
            .map(|r| r.path)
            .collect();
        if paths.is_empty() {
            return;
        }
        match self.data.add(target, &paths, None) {
            Ok(()) => self.status.set_text(&match paths.len() {
                1 => format!("Added 1 track to {target}"),
                n => format!("Added {n} tracks to {target}"),
            }),
            Err(e) => self.show_error(&format!("Nothing was added to {target}"), &e),
        }
    }

    /// The library tracks of the selected rows, each once, in view order.
    fn info_ids(&self) -> Vec<i64> {
        let mut ids = Vec::new();
        for id in self.selected_rows().iter().filter_map(|r| r.track_id) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// Where the row of track `id`, at playlist `position`, is in the
    /// grid as sorted and filtered.
    fn view_position(&self, id: i64, position: Option<usize>) -> Option<u32> {
        (0..self.selection.n_items()).find(|&i| {
            self.selection.item(i).is_some_and(|o| {
                let r = row_of(&o);
                r.track_id == Some(id) && r.position == position
            })
        })
    }

    /// The nearest row from view position `from`, one way, that has a file:
    /// its track and playlist position.
    fn neighbour(&self, from: u32, up: bool) -> Option<(i64, Option<usize>)> {
        let mut i = from;
        loop {
            i = if up { i.checked_sub(1)? } else { i + 1 };
            let obj = self.selection.item(i)?;
            let r = row_of(&obj);
            if let Some(id) = r.track_id {
                return Some((id, r.position));
            }
        }
    }

    /// Shows the rows of the tracks `ids` as the index has them now: the
    /// rows are replaced in one go, so the grid sorts once however many
    /// were edited, and the selection and the scroll are kept.
    fn update_rows(&self, ids: &HashSet<i64>) {
        if ids.is_empty() {
            return;
        }
        let tracks = match self.data.tracks(ids) {
            Ok(t) => t,
            Err(e) => return self.status.set_text(&format!("{e:#}")),
        };
        let selected: HashSet<(Option<i64>, Option<usize>)> = self
            .selected_rows()
            .iter()
            .map(|r| (r.track_id, r.position))
            .collect();
        let scrolled = self.view.vadjustment().map(|a| a.value());
        let n = self.store.n_items();
        let objects: Vec<glib::Object> = (0..n)
            .filter_map(|i| self.store.item(i))
            .map(|obj| {
                let fresh = {
                    let r = row_of(&obj);
                    r.track_id
                        .and_then(|id| tracks.get(&id))
                        .map(|t| self.data.row(t, r.position))
                };
                match fresh {
                    Some(row) => glib::BoxedAnyObject::new(row).upcast(),
                    None => obj,
                }
            })
            .collect();
        self.store.splice(0, n, &objects);
        // Replaced rows come back unselected.
        let keep = gtk::Bitset::new_empty();
        for i in 0..self.selection.n_items() {
            let hit = self.selection.item(i).is_some_and(|o| {
                let r = row_of(&o);
                selected.contains(&(r.track_id, r.position))
            });
            if hit {
                keep.add(i);
            }
        }
        self.selection
            .set_selection(&keep, &gtk::Bitset::new_range(0, self.selection.n_items()));
        if let (Some(v), Some(a)) = (scrolled, self.view.vadjustment()) {
            a.set_value(v);
        }
        self.update_status();
    }

    /// Gives the grid the focus, on the first selected row: replacing the
    /// rows forgets the one that had it, and the grid alone would then
    /// pass the focus on to the playlist filter.
    fn focus_grid(&self) {
        let bits = self.selection.selection();
        if bits.is_empty() {
            self.view.grab_focus();
        } else {
            self.view
                .scroll_to(bits.nth(0), None, gtk::ListScrollFlags::FOCUS, None);
            // That sets the row the grid focuses; this moves the focus there.
            self.view.grab_focus();
        }
    }

    /// Opens Get Info on the selected rows that have a file: their tags, to
    /// edit as in iTunes.
    fn get_info(self: &Rc<Self>) {
        let ids = self.info_ids();
        if ids.is_empty() {
            return;
        }
        let tracks: anyhow::Result<Vec<_>> = ids.iter().map(|&id| self.data.track(id)).collect();
        let tracks: Vec<_> = match tracks {
            Ok(t) => t.into_iter().flatten().collect(),
            Err(e) => return self.show_error("The songs could not be read", &e),
        };
        if tracks.is_empty() {
            return;
        }
        let several = tracks.len() > 1;
        let completions = self.data.completions().unwrap_or_default();
        let dialog = info_dialog::InfoDialog::new(&self.window, several, completions);
        dialog.fill(&tracks);
        let state = Rc::new(InfoState {
            ids: RefCell::new(tracks.iter().map(|t| t.id).collect()),
            // The row shown, for Previous and Next.
            at: Cell::new(
                self.selected_rows()
                    .iter()
                    .find(|r| r.track_id == Some(tracks[0].id))
                    .and_then(|r| self.view_position(tracks[0].id, r.position)),
            ),
            stop: RefCell::new(None),
        });
        self.update_neighbours(&dialog, &state);
        {
            let (ui, d, st) = (Rc::downgrade(self), Rc::downgrade(&dialog), state.clone());
            dialog.ok.connect_clicked(move |_| {
                let (Some(ui), Some(d)) = (ui.upgrade(), d.upgrade()) else {
                    return;
                };
                ui.info_done(&d, &st, None);
            });
        }
        for (button, up) in [(&dialog.previous, true), (&dialog.next, false)] {
            let (ui, d, st) = (Rc::downgrade(self), Rc::downgrade(&dialog), state.clone());
            button.connect_clicked(move |_| {
                let (Some(ui), Some(d)) = (ui.upgrade(), d.upgrade()) else {
                    return;
                };
                let target = st.at.get().and_then(|at| ui.neighbour(at, up));
                if target.is_some() {
                    ui.info_done(&d, &st, target);
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
            let st = state.clone();
            dialog.stop.connect_clicked(move |b| {
                if let Some(stop) = &*st.stop.borrow() {
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    b.set_sensitive(false);
                }
            });
        }
        {
            // The dialog lives until its window closes: every way of
            // closing it asks first (the dialog's own handler, connected
            // earlier, refuses while it writes). The grid then has the
            // focus back, once the window is active again, which resets
            // its focus when the widget that had it is gone.
            let keep = RefCell::new(Some(dialog.clone()));
            let ui = Rc::downgrade(self);
            dialog.window.connect_close_request(move |_| {
                keep.borrow_mut().take();
                let ui = ui.clone();
                glib::timeout_add_local_once(Duration::from_millis(100), move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.focus_grid();
                    }
                });
                glib::Propagation::Proceed
            });
        }
        dialog.window.present();
    }

    fn update_neighbours(&self, dialog: &info_dialog::InfoDialog, state: &InfoState) {
        let at = state.at.get();
        dialog
            .previous
            .set_sensitive(at.and_then(|a| self.neighbour(a, true)).is_some());
        dialog
            .next
            .set_sensitive(at.and_then(|a| self.neighbour(a, false)).is_some());
    }

    /// OK, or Previous and Next with the row to go to: writes what the
    /// form changed, then closes it or shows that row.
    fn info_done(
        self: &Rc<Self>,
        dialog: &Rc<info_dialog::InfoDialog>,
        state: &Rc<InfoState>,
        go_to: Option<(i64, Option<usize>)>,
    ) {
        let Ok(changes) = dialog.changes() else {
            return;
        };
        if changes.is_empty() {
            self.info_next(dialog, state, go_to);
            return;
        }
        // One thing at a time: a scan or a sync reads the files and the index.
        if self.busy.replace(true) {
            dialog.say("A scan, sync or adoption is under way; try again once it is done.");
            return;
        }
        self.refresh_device();
        dialog.set_writing(true);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        state.stop.replace(Some(stop.clone()));
        // As during a sync: the window cannot close, and a signal stops
        // the write between files, the window quitting once it is done.
        self.writing.set(true);
        self.cancel.replace(Some(stop.clone()));
        let (tx, rx) = async_channel::unbounded::<(usize, usize)>();
        {
            let d = Rc::downgrade(dialog);
            glib::spawn_future_local(async move {
                while let Ok((done, total)) = rx.recv().await {
                    if let Some(d) = d.upgrade() {
                        d.show_progress(done, total);
                    }
                }
            });
        }
        let ids = state.ids.borrow().clone();
        let paths = self.data.paths().clone();
        let (ui, d, st) = (Rc::downgrade(self), Rc::downgrade(dialog), state.clone());
        glib::spawn_future_local(async move {
            let asked = ids.len();
            let result = gio::spawn_blocking(move || {
                let progress = |done, total| {
                    let _ = tx.send_blocking((done, total));
                };
                tag_job::write(&paths, &ids, &changes, &progress, &stop)
            })
            .await;
            let Some(ui) = ui.upgrade() else { return };
            ui.busy.set(false);
            ui.writing.set(false);
            ui.cancel.replace(None);
            ui.refresh_device();
            st.stop.replace(None);
            let result = result.unwrap_or_else(|_| Err(anyhow::anyhow!("the tag worker crashed")));
            let Some(d) = d.upgrade() else { return };
            d.set_writing(false);
            let report = match result {
                Ok(r) => r,
                Err(e) => {
                    // Files before the error may have been written.
                    ui.refresh();
                    d.say(&format!("The edit stopped: {e:#}"));
                    return;
                }
            };
            let written: HashSet<i64> = report
                .results
                .iter()
                .filter(|r| r.outcome == iprune_core::tag_edit::Outcome::Written)
                .map(|r| r.track_id)
                .collect();
            ui.update_rows(&written);
            let complete = report.refused().next().is_none() && !report.stopped;
            match (complete, asked) {
                (true, _) => {
                    ui.status.set_text(&match written.len() {
                        0 => "The tags were already as entered".to_string(),
                        1 => "Tags written to 1 song".to_string(),
                        n => format!("Tags written to {n} songs"),
                    });
                    ui.info_next(&d, &st, go_to);
                }
                // One song stays in the form, with why.
                (false, 1) => match (&report.index_error, report.refused().next()) {
                    (Some(e), _) => {
                        d.say(&format!("Written, but the index could not be updated: {e}"))
                    }
                    (None, Some((_, why))) => d.say(&format!("Not written: {why}.")),
                    (None, None) => d.say("Stopped before the file was written."),
                },
                (false, _) => d.show_report(&tag_job::describe(&report, asked)),
            }
        });
    }

    /// After a write that went through: closes the form, or shows the row
    /// `go_to` in it and selects it in the grid.
    fn info_next(
        &self,
        dialog: &info_dialog::InfoDialog,
        state: &InfoState,
        go_to: Option<(i64, Option<usize>)>,
    ) {
        let Some((id, position)) = go_to else {
            dialog.window.close();
            return;
        };
        let track = match self.data.track(id) {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(e) => return dialog.say(&format!("{e:#}")),
        };
        // The rows may have moved if the grid is sorted by a field edited.
        let at = self.view_position(id, position);
        if let Some(i) = at {
            self.selection.select_item(i, true);
            self.view
                .scroll_to(i, None, gtk::ListScrollFlags::NONE, None);
        }
        state.at.set(at);
        *state.ids.borrow_mut() = vec![id];
        dialog.fill(std::slice::from_ref(&track));
        self.update_neighbours(dialog, state);
    }

    /// Scans the library on a worker thread, as `iprune scan` does, and shows
    /// its report in a dialog.
    fn scan_library(self: &Rc<Self>) {
        // One thing at a time: Sync and Adopt scan too, and a scan during a
        // sync would change the index under it.
        if self.busy.replace(true) {
            return;
        }
        self.refresh_device();
        self.status.set_text("Scanning the library…");
        let dialog = gtk::Window::builder()
            .title("Scan")
            .transient_for(&self.window)
            .modal(true)
            .default_width(640)
            .default_height(480)
            .build();
        let gone = Rc::new(Cell::new(false));
        {
            let gone = gone.clone();
            // Closed during the scan, the scan goes on: the report then
            // goes to the status line.
            dialog.connect_destroy(move |_| gone.set(true));
        }
        let stack = gtk::Stack::new();
        dialog.set_child(Some(&stack));
        working_page(&stack, "Scanning the library…");
        dialog.present();

        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let dialog_weak = dialog.downgrade();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || scan_job::scan(&config, &paths)).await;
            let Some(ui) = ui.upgrade() else { return };
            ui.busy.set(false);
            ui.refresh_device();
            // Show the library as the scan left it. It reloads the playlists
            // too: the sidebar follows one added or removed, and falls back
            // to the library if the one shown is gone.
            if ui.sidebar_stale() {
                ui.fill_sidebar();
            } else {
                ui.refresh();
            }
            let result = result.unwrap_or_else(|_| Err(anyhow::anyhow!("the scan worker crashed")));
            let dialog = dialog_weak.upgrade().filter(|_| !gone.get());
            match (result, dialog) {
                (Ok(report), Some(dialog)) => report_page(&dialog, &stack, &report),
                (Ok(report), None) => ui
                    .status
                    .set_text(report.lines().next().unwrap_or_default()),
                (Err(e), Some(dialog)) => result_page(
                    &dialog,
                    &stack,
                    "The library could not be scanned",
                    &format!("{e:#}"),
                ),
                (Err(e), None) => ui.show_error("The library could not be scanned", &e),
            }
        });
    }

    /// Plans a sync on a worker thread and shows the plan in a dialog, from
    /// which it can be applied.
    fn open_sync(self: &Rc<Self>) {
        // One sync at a time, planning included: a second one would read
        // the iPod while the first writes it.
        if self.busy.replace(true) {
            return;
        }
        let dialog = gtk::Window::builder()
            .title("Sync")
            .transient_for(&self.window)
            .modal(true)
            .default_width(760)
            .default_height(600)
            .build();
        let gone = Rc::new(Cell::new(false));
        {
            let ui = Rc::downgrade(self);
            // A close request (the window manager's close key too) is
            // refused while the iPod is written.
            dialog.connect_close_request(move |_| match ui.upgrade() {
                Some(ui) if ui.writing.get() => glib::Propagation::Stop,
                _ => glib::Propagation::Proceed,
            });
            let ui = Rc::downgrade(self);
            let gone = gone.clone();
            // Closed during planning, the sync is over when the worker is.
            dialog.connect_destroy(move |_| {
                gone.set(true);
                if let Some(ui) = ui.upgrade()
                    && !ui.planning.get()
                {
                    ui.busy.set(false);
                    ui.refresh_device();
                }
            });
        }
        // Sync, Adopt and Eject are off until it is over.
        self.refresh_device();
        self.planning.set(true);
        let stack = gtk::Stack::new();
        dialog.set_child(Some(&stack));
        working_page(&stack, "Scanning the library and reading the iPod…");
        dialog.present();

        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let dialog_weak = dialog.downgrade();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || sync_job::plan(&config, &paths)).await;
            let Some(ui) = ui.upgrade() else { return };
            ui.planning.set(false);
            // Destroyed while planning (a reference may outlive the window,
            // so the flag is checked too): the sync ends now.
            let dialog = match dialog_weak.upgrade() {
                Some(d) if !gone.get() => d,
                _ => {
                    ui.busy.set(false);
                    ui.refresh_device();
                    return;
                }
            };
            // The scan may have changed the index: show the library as it is.
            ui.refresh();
            match joined(result) {
                Ok(planned) => ui.plan_page(&dialog, &stack, planned),
                Err(e) => result_page(
                    &dialog,
                    &stack,
                    "The sync could not be planned",
                    &format!("{e:#}\n\nNothing was written to the iPod."),
                ),
            }
        });
    }

    fn plan_page(
        self: &Rc<Self>,
        dialog: &gtk::Window,
        stack: &gtk::Stack,
        planned: sync_job::Planned,
    ) {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
        page.set_margin_top(16);
        page.set_margin_bottom(16);
        page.set_margin_start(16);
        page.set_margin_end(16);

        let heading = gtk::Label::new(Some(&format!("Sync with {}", planned.ipod_name)));
        heading.add_css_class("title-2");
        heading.set_xalign(0.0);
        page.append(&heading);
        let scan = gtk::Label::new(Some(&planned.scan));
        scan.add_css_class("dim-label");
        scan.set_xalign(0.0);
        page.append(&scan);
        let summary = gtk::Label::new(Some(planned.summary.trim_end()));
        summary.add_css_class("monospace");
        summary.set_xalign(0.0);
        summary.set_selectable(true);
        page.append(&summary);

        let mut note = planned.to_apply.clone();
        if !planned.fits {
            note.push_str(" The tracks to copy do not fit on the iPod: Apply is not possible.");
        }
        let note = gtk::Label::new(Some(&note));
        note.set_wrap(true);
        note.set_xalign(0.0);
        page.append(&note);

        let allow = gtk::CheckButton::with_label("Apply it anyway");
        if let Some(why) = &planned.mass_removal {
            let warn = gtk::Label::new(Some(why));
            warn.set_wrap(true);
            warn.set_xalign(0.0);
            warn.add_css_class("error");
            page.append(&warn);
            page.append(&allow);
        }

        let details = gtk::TextView::new();
        details.set_editable(false);
        details.set_cursor_visible(false);
        details.set_monospace(true);
        details.buffer().set_text(planned.details.trim_end());
        let scroll = gtk::ScrolledWindow::builder()
            .child(&details)
            .vexpand(true)
            .min_content_height(240)
            .build();
        let expander = gtk::Expander::new(Some("All changes"));
        expander.set_child(Some(&scroll));
        expander.set_vexpand(true);
        page.append(&expander);

        // Alt+A and Alt+C reach the buttons from the keyboard.
        let cancel = gtk::Button::with_mnemonic("_Cancel");
        let apply = gtk::Button::with_mnemonic("_Apply");
        apply.add_css_class("suggested-action");
        let (some, fits, mass) = (
            !planned.work.is_empty(),
            planned.fits,
            planned.mass_removal.is_some(),
        );
        let ready = move |allowed: bool| some && fits && (!mass || allowed);
        apply.set_sensitive(ready(allow.is_active()));
        {
            let apply = apply.clone();
            allow.connect_toggled(move |a| apply.set_sensitive(ready(a.is_active())));
        }
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&apply);
        page.append(&buttons);
        closes(&cancel, dialog);
        {
            let ui = Rc::downgrade(self);
            let (dialog, stack, allow) = (dialog.downgrade(), stack.clone(), allow.clone());
            let work = planned.work.clone();
            apply.connect_clicked(move |_| {
                if let (Some(ui), Some(dialog)) = (ui.upgrade(), dialog.upgrade()) {
                    ui.apply_sync(&dialog, &stack, work.clone(), allow.is_active());
                }
            });
        }
        stack.add_named(&page, Some("plan"));
        stack.set_visible_child(&page);
    }

    fn apply_sync(
        self: &Rc<Self>,
        dialog: &gtk::Window,
        stack: &gtk::Stack,
        work: iprune_core::exec::FullWork,
        allow: bool,
    ) {
        // Set before anything else: a second Apply does nothing.
        if self.writing.replace(true) {
            return;
        }
        // Neither this dialog nor the window may close while the iPod is
        // written (see their close-request handlers); the application stays
        // up until the worker is done.
        dialog.set_deletable(false);
        let hold = self.window.application().map(|a| a.hold());
        // No suspend, shutdown or lid action until the worker is done.
        let inhibit = iprune_core::inhibit::hold("Writing to the iPod");
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancel.replace(Some(cancel.clone()));
        let progress_page =
            ProgressPage::new(stack, cancel.clone(), !work.tracks.copies.is_empty());
        let (tx, rx) = async_channel::unbounded::<iprune_core::exec::Progress>();
        {
            let page = progress_page.clone();
            glib::spawn_future_local(async move {
                while let Ok(p) = rx.recv().await {
                    page.show(&p);
                }
            });
        }
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let (dialog, stack) = (dialog.clone(), stack.clone());
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let progress = |p: iprune_core::exec::Progress| {
                    let _ = tx.send_blocking(p);
                };
                sync_job::apply(&config, &paths, &work, allow, &progress, &cancel)
            })
            .await;
            drop(inhibit);
            drop(hold);
            dialog.set_deletable(true);
            let Some(ui) = ui.upgrade() else { return };
            ui.writing.set(false);
            ui.cancel.replace(None);
            let result = joined(result);
            if let Some(note) = match &result {
                Ok(done) => done.note.clone(),
                Err(e) => Some(notify::failed(&format!("{e:#}"))),
            } {
                notify::send(&note);
            }
            match result {
                Ok(done) => {
                    let (title, mut text) = match &done.stopped {
                        None => ("Sync done", String::new()),
                        Some(why) => (
                            "The sync stopped early",
                            format!(
                                "It stopped ({why}). What was done before is written and verified; the next sync continues.\n"
                            ),
                        ),
                    };
                    if done.stamp.is_none() {
                        text.push_str("Nothing needed applying.");
                    } else {
                        text.push_str(&format!(
                            "Copied {} tracks ({}), copied {} again, removed {}, updated the tags of {} and made {} playlist changes, then verified the iPod database by reading it back.",
                            done.copied,
                            iprune_core::sync::bytes(done.bytes),
                            done.recopied,
                            done.removed,
                            done.retagged,
                            done.applied
                        ));
                        text.push_str(&format!(
                            "\nStatistics updated for {} tracks ({} new plays).",
                            done.stats_tracks, done.plays
                        ));
                    }
                    if done.recovered > 0 {
                        text.push_str(&format!(
                            "\nAn earlier sync stopped before writing; its statistics for {} tracks were undone first.",
                            done.recovered
                        ));
                    }
                    if let Some(stamp) = &done.stamp {
                        text.push_str(&format!(
                            "\nBackup {stamp}: `iprune restore {stamp}` puts the previous database back."
                        ));
                    }
                    for (path, why) in &done.skipped {
                        text.push_str(&format!("\nNot copied, {why}: {}", path.display()));
                    }
                    for w in &done.warnings {
                        text.push_str(&format!("\nWarning: {w}"));
                    }
                    result_page(&dialog, &stack, title, &text);
                }
                Err(e) => {
                    let busy = e
                        .downcast_ref::<iprune_core::Error>()
                        .is_some_and(|e| matches!(e, iprune_core::Error::IpodBusy));
                    match busy {
                        true => result_page(
                            &dialog,
                            &stack,
                            "The iPod is busy",
                            &format!("{e:#}\n\nNothing was written to the iPod."),
                        ),
                        false => {
                            result_page(&dialog, &stack, "The sync stopped", &format!("{e:#}"))
                        }
                    }
                }
            }
            // Statistics, mappings and the last sync may have changed,
            // whatever the outcome.
            if let Err(e) = ui.data.reload_stats() {
                ui.status.set_text(&format!("{e:#}"));
            }
            ui.refresh_device();
            ui.refresh();
        });
    }

    /// Rebuilds the sidebar's playlists and selects the current source, or
    /// the library when it is gone.
    fn fill_sidebar(self: &Rc<Self>) {
        while let Some(row) = self.sidebar.row_at_index(0) {
            self.sidebar.remove(&row);
        }
        let mut sources = Vec::new();
        self.sidebar.append(&sidebar_header("Library"));
        sources.push(None);
        self.sidebar.append(&sidebar_item("Music"));
        sources.push(Some(Source::Library));
        self.sidebar.append(&sidebar_header("Playlists"));
        sources.push(None);
        for name in self.data.playlist_names().unwrap_or_default() {
            let row = sidebar_item(&name);
            // Tracks dropped on a playlist are added at its end.
            let target = drop_target();
            {
                let ui = Rc::downgrade(self);
                let name = name.clone();
                target.connect_drop(move |_, value, _, _| {
                    let Some(ui) = ui.upgrade() else {
                        return false;
                    };
                    is_drag(value) && ui.drop_on_playlist(&name)
                });
            }
            row.add_controller(target);
            self.sidebar.append(&row);
            sources.push(Some(Source::Playlist(name)));
        }
        let current = self.current.borrow().clone();
        let index = sources
            .iter()
            .position(|s| s.as_ref() == Some(&current))
            .unwrap_or(1);
        *self.sources.borrow_mut() = sources;
        // The filter ran on each row as it was added, against the old list.
        self.sidebar.invalidate_filter();
        // Selecting shows it; a reload of the same source is fine.
        if let Some(row) = self.sidebar.row_at_index(index as i32) {
            self.sidebar.select_row(Some(&row));
        }
    }

    fn new_playlist(self: &Rc<Self>) {
        let ui = Rc::downgrade(self);
        ask_name(
            &self.window,
            "New Playlist",
            "Name of the new playlist",
            "",
            move |name| {
                let Some(ui) = ui.upgrade() else { return };
                match ui.data.create(&name) {
                    Ok(()) => {
                        *ui.current.borrow_mut() = Source::Playlist(name);
                        ui.fill_sidebar();
                    }
                    Err(e) => ui.show_error("The playlist was not created", &e),
                }
            },
        );
    }

    fn rename_playlist(self: &Rc<Self>, old: String) {
        let ui = Rc::downgrade(self);
        let initial = old.clone();
        ask_name(
            &self.window,
            "Rename Playlist",
            "New name",
            &initial,
            move |new| {
                let Some(ui) = ui.upgrade() else { return };
                if new == old {
                    return;
                }
                match ui.data.rename(&old, &new) {
                    Ok(()) => {
                        if *ui.current.borrow() == Source::Playlist(old.clone()) {
                            *ui.current.borrow_mut() = Source::Playlist(new);
                        }
                        ui.fill_sidebar();
                    }
                    Err(e) => ui.show_error("The playlist was not renamed", &e),
                }
            },
        );
    }

    fn delete_playlist(self: &Rc<Self>, name: String) {
        let dialog = gtk::AlertDialog::builder()
            .message(format!("Delete the playlist {name:?}?"))
            .detail(
                "Its .m3u8 file is deleted. The tracks stay in the library. If the iPod has \
                 this playlist, the next sync removes it there.",
            )
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .modal(true)
            .build();
        let ui = Rc::downgrade(self);
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |answer| {
            let Some(ui) = ui.upgrade() else { return };
            if answer != Ok(1) {
                return;
            }
            match ui.data.delete(&name) {
                // The sidebar falls back to the library when the playlist
                // shown is gone.
                Ok(()) => ui.fill_sidebar(),
                Err(e) => ui.show_error("The playlist was not deleted", &e),
            }
        });
    }

    /// Called on every change in the playlists folder; reloads once things
    /// settle.
    fn schedule_reload(self: &Rc<Self>) {
        if self.reload_pending.replace(true) {
            return;
        }
        let ui = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(ui) = ui.upgrade() {
                ui.reload_pending.set(false);
                ui.reload_from_disk();
            }
        });
    }

    /// Picks up playlist files changed by another program. iprune's own
    /// writes already left the index up to date, so they reload nothing.
    fn reload_from_disk(self: &Rc<Self>) {
        if let Err(e) = self.data.reload_playlists() {
            self.status.set_text(&format!("{e:#}"));
            return;
        }
        // Another iprune (the CLI) may have updated the index while writing
        // the file, so the file on screen is checked directly.
        let shown_changed = match &*self.current.borrow() {
            Source::Playlist(name) => self.data.playlist_mtime(name) != self.shown_mtime.get(),
            Source::Library => false,
        };
        if self.sidebar_stale() {
            // The sidebar falls back to the library if the playlist shown
            // is gone.
            self.fill_sidebar();
        } else if shown_changed {
            self.refresh();
        }
    }

    /// Whether the sidebar's playlists differ from the index's.
    fn sidebar_stale(&self) -> bool {
        let listed: Vec<String> = self
            .sources
            .borrow()
            .iter()
            .filter_map(|s| match s {
                Some(Source::Playlist(n)) => Some(n.clone()),
                _ => None,
            })
            .collect();
        self.data.playlist_names().ok().as_ref() != Some(&listed)
    }
}

/// A worker's result, with a panic turned into an error.
fn joined<T>(r: Result<anyhow::Result<T>, Box<dyn std::any::Any + Send>>) -> anyhow::Result<T> {
    r.unwrap_or_else(|_| Err(anyhow::anyhow!("the sync worker crashed")))
}

fn working_page(stack: &gtk::Stack, text: &str) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_valign(gtk::Align::Center);
    page.set_halign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.start();
    page.append(&spinner);
    page.append(&gtk::Label::new(Some(text)));
    stack.add_child(&page);
    stack.set_visible_child(&page);
}

/// A scan's report, which can run to thousands of lines (every missing
/// file), so it scrolls.
fn report_page(dialog: &gtk::Window, stack: &gtk::Stack, report: &str) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);
    let heading = gtk::Label::new(Some("Library scanned"));
    heading.add_css_class("title-2");
    heading.set_xalign(0.0);
    let text = gtk::TextView::new();
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.buffer().set_text(report.trim_end());
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&text)
        .build();
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    closes(&close, dialog);
    page.append(&heading);
    page.append(&scroll);
    page.append(&close);
    stack.add_child(&page);
    stack.set_visible_child(&page);
    close.grab_focus();
}

fn result_page(dialog: &gtk::Window, stack: &gtk::Stack, title: &str, text: &str) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("title-2");
    heading.set_xalign(0.0);
    let body = gtk::Label::new(Some(text));
    body.set_wrap(true);
    body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    body.set_xalign(0.0);
    body.set_yalign(0.0);
    body.set_selectable(true);
    body.set_vexpand(true);
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    closes(&close, dialog);
    page.append(&heading);
    page.append(&body);
    page.append(&close);
    stack.add_child(&page);
    stack.set_visible_child(&page);
}

/// The dialog page shown while a sync writes: what it is doing, a bar for
/// the copies, and a button that stops after the current file.
#[derive(Clone)]
struct ProgressPage {
    label: gtk::Label,
    bar: gtk::ProgressBar,
    stop: gtk::Button,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ProgressPage {
    /// `copies`: whether there is anything Stop can stop.
    fn new(
        stack: &gtk::Stack,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        copies: bool,
    ) -> Self {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        page.set_valign(gtk::Align::Center);
        page.set_margin_start(32);
        page.set_margin_end(32);
        let heading = gtk::Label::new(Some("Syncing… Do not disconnect the iPod."));
        heading.add_css_class("title-3");
        let label = gtk::Label::new(Some("Starting…"));
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let bar = gtk::ProgressBar::new();
        let stop = gtk::Button::with_mnemonic("_Stop after this file");
        stop.set_halign(gtk::Align::Center);
        {
            let label = label.clone();
            let cancel = cancel.clone();
            stop.connect_clicked(move |b| {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                b.set_sensitive(false);
                label.set_text("Stopping after the current file…");
            });
        }
        page.append(&heading);
        page.append(&label);
        page.append(&bar);
        stop.set_visible(copies);
        bar.set_visible(copies);
        page.append(&stop);
        stack.add_child(&page);
        stack.set_visible_child(&page);
        ProgressPage {
            label,
            bar,
            stop,
            cancel,
        }
    }

    fn show(&self, p: &iprune_core::exec::Progress) {
        use iprune_core::exec::Progress;
        match p {
            Progress::Removing { done, total } => {
                self.label
                    .set_text(&format!("Removing tracks: {done} of {total}"));
            }
            Progress::Copying {
                done,
                total,
                bytes,
                bytes_total,
                name,
            } => {
                // Stop is back once a checkpoint's write is done, unless
                // it was pressed.
                let cancelled = self.cancel.load(std::sync::atomic::Ordering::Relaxed);
                self.stop
                    .set_sensitive(sync_job::stop_enabled(p, cancelled).unwrap_or(false));
                if cancelled {
                    self.label.set_text("Stopping after the current file…");
                    return;
                }
                let shown = (*done + 1).min(*total);
                self.label.set_text(&format!(
                    "Copying {shown} of {total} ({} of {}): {name}",
                    iprune_core::sync::bytes(*bytes),
                    iprune_core::sync::bytes(*bytes_total)
                ));
                if *bytes_total > 0 {
                    self.bar.set_fraction(*bytes as f64 / *bytes_total as f64);
                }
            }
            Progress::Writing => {
                // Not while a write is under way.
                self.stop
                    .set_sensitive(sync_job::stop_enabled(p, false).unwrap_or(false));
                self.label.set_text("Writing the iPod database…");
            }
            Progress::Verifying => self.label.set_text("Reading it back to verify…"),
        }
    }
}

fn action(
    window: &gtk::ApplicationWindow,
    name: &str,
    ui: &Rc<Ui>,
    f: impl Fn(&Rc<Ui>) + 'static,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(name, None);
    let weak = Rc::downgrade(ui);
    action.connect_activate(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    });
    window.add_action(&action);
    action
}

fn shortcut(controller: &gtk::ShortcutController, trigger: &str, action: &str) {
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string(trigger),
        Some(gtk::NamedAction::new(action)),
    ));
}

/// The text of the badge that follows the Sync button in the header.
const APP_MARK: &str = "IPRUNE";

/// Packs the Scan and Sync buttons at the start of `header`, then the badge
/// that tells this window from fiio-tunes', which it otherwise matches.
fn pack_buttons_and_mark(
    header: &gtk::HeaderBar,
    scan_button: &gtk::Button,
    sync_button: &gtk::Button,
) {
    header.pack_start(scan_button);
    header.pack_start(sync_button);
    let mark = gtk::Label::new(Some(APP_MARK));
    mark.add_css_class("app-mark");
    mark.set_valign(gtk::Align::Center);
    header.pack_start(&mark);
}

/// `button` closes `dialog`. Through a weak reference: a button inside the
/// dialog holding it would keep it alive, and its `destroy` handlers, which
/// end a sync or an adoption, would never run.
fn closes(button: &gtk::Button, dialog: &gtk::Window) {
    let dialog = dialog.downgrade();
    button.connect_clicked(move |_| {
        if let Some(dialog) = dialog.upgrade() {
            dialog.close();
        }
    });
}

/// A right-click menu over `widget`, built from `menu`.
fn context_menu(
    widget: &impl IsA<gtk::Widget>,
    menu: &gio::Menu,
    before: impl Fn(f64, f64) -> bool + 'static,
) {
    let popover = gtk::PopoverMenu::from_model(Some(menu));
    popover.set_parent(widget);
    popover.set_has_arrow(false);
    {
        let popover = popover.clone();
        widget.connect_destroy(move |_| popover.unparent());
    }
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(move |_, _, x, y| {
        if before(x, y) {
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    });
    widget.add_controller(click);
}

fn build(window: &gtk::ApplicationWindow, data: Data) {
    let slot: UiSlot = Rc::new(OnceCell::new());
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let filter = {
        let slot = slot.clone();
        gtk::CustomFilter::new(move |obj| {
            ui_of(&slot).is_none_or(|ui| row_of(obj).matches(&ui.query.borrow()))
        })
    };
    let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let view = gtk::ColumnView::new(None::<gtk::MultiSelection>);
    view.set_show_column_separators(true);
    // Compact rows, as a music grid needs (GTK 4.12).
    view.add_css_class("data-table");
    // The stripes and cell padding of `style.css`.
    view.add_css_class("track-grid");
    view.set_reorderable(true);
    let mut columns = Vec::new();
    for col in Column::ALL {
        let c = make_column(col, slot.clone());
        view.append_column(&c);
        columns.push((col, c));
    }
    let sorted = gtk::SortListModel::new(Some(filtered.clone()), view.sorter());
    let selection = gtk::MultiSelection::new(Some(sorted));
    view.set_model(Some(&selection));
    // Opened by each cell's right-click, see `make_column`.
    let grid_menu = gio::Menu::new();
    grid_menu.append(Some("Get Info"), Some("win.get-info"));
    grid_menu.append(Some("Add to Playlist…"), Some("win.add-to-playlist"));
    grid_menu.append(Some("Show in Playlist…"), Some("win.show-in-playlist"));
    grid_menu.append(Some("Remove from Playlist"), Some("win.remove"));
    grid_menu.append(Some("Show in Folder"), Some("win.show-in-folder"));
    let row_menu = gtk::PopoverMenu::from_model(Some(&grid_menu));
    row_menu.set_parent(&view);
    row_menu.set_has_arrow(false);
    {
        let row_menu = row_menu.clone();
        view.connect_destroy(move |_| row_menu.unparent());
    }
    // The Add to Playlist picker. A submenu would be as short as the menu
    // it slides over: a list as high as the screen allows, with a filter.
    let picker_search = gtk::SearchEntry::new();
    picker_search.set_placeholder_text(Some("Filter playlists"));
    let picker_list = gtk::ListBox::new();
    picker_list.add_css_class("navigation-sidebar");
    picker_list.set_activate_on_single_click(true);
    picker_list.set_placeholder(Some(&{
        let none = gtk::Label::new(Some("No playlist"));
        none.add_css_class("dim-label");
        none.set_margin_top(12);
        none.set_margin_bottom(12);
        none
    }));
    // Typing in the list goes on in the filter.
    picker_search.set_key_capture_widget(Some(&picker_list));
    let picker_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(480)
        .min_content_width(280)
        .child(&picker_list)
        .build();
    let picker_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    picker_box.append(&picker_search);
    picker_box.append(&picker_scroll);
    let picker = gtk::Popover::new();
    picker.set_child(Some(&picker_box));
    picker.set_parent(&view);
    picker.set_has_arrow(false);
    {
        let picker = picker.clone();
        view.connect_destroy(move |_| picker.unparent());
    }

    // Header: columns menu, title and search.
    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Music"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search"));
    search.set_width_chars(28);
    header.pack_end(&search);
    window.set_titlebar(Some(&header));

    let column_items = gio::Menu::new();
    let actions = gio::SimpleActionGroup::new();
    for (i, (col, c)) in columns.iter().enumerate() {
        let name = format!("column{i}");
        let action = gio::SimpleAction::new_stateful(&name, None, &c.is_visible().to_variant());
        {
            let c = c.clone();
            action.connect_change_state(move |_, v| {
                if let Some(v) = v.and_then(|v| v.get::<bool>()) {
                    c.set_visible(v);
                }
            });
        }
        // The menu follows the column, whoever changed it.
        {
            let action = action.clone();
            c.connect_visible_notify(move |c| action.set_state(&c.is_visible().to_variant()));
        }
        actions.add_action(&action);
        column_items.append(Some(col.title()), Some(&format!("columns.{name}")));
    }
    window.insert_action_group("columns", Some(&actions));
    let background_items = gio::Menu::new();
    background_items.append(Some("Background Colour…"), Some("win.background"));
    background_items.append(Some("Default Background"), Some("win.default-background"));
    let menu = gio::Menu::new();
    menu.append_section(Some("Columns"), &column_items);
    menu.append_section(None, &background_items);
    let columns_button = gtk::MenuButton::new();
    columns_button.set_icon_name("view-more-symbolic");
    columns_button.set_tooltip_text(Some("Columns and Background"));
    columns_button.set_menu_model(Some(&menu));
    header.pack_start(&columns_button);

    let status = gtk::Label::new(None);
    status.set_xalign(0.5);
    status.add_css_class("status-bar");

    // Sidebar, filled once the window state exists.
    let sidebar = gtk::ListBox::new();
    sidebar.add_css_class("navigation-sidebar");
    let new_button = gtk::Button::from_icon_name("list-add-symbolic");
    new_button.set_tooltip_text(Some("New Playlist (Ctrl+N)"));
    new_button.set_action_name(Some("win.new-playlist"));
    new_button.add_css_class("flat");
    new_button.set_halign(gtk::Align::Start);
    new_button.set_margin_start(8);
    new_button.set_margin_top(4);
    new_button.set_margin_bottom(4);

    let device_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let scan_button = gtk::Button::with_label("Scan");
    scan_button.set_tooltip_text(Some(
        "Scan the library for added, changed, moved and missing files",
    ));
    scan_button.set_action_name(Some("win.scan"));
    let sync_button = gtk::Button::with_label("Sync");
    sync_button.set_tooltip_text(Some("Show the sync plan for the connected iPod"));
    sync_button.set_action_name(Some("win.sync"));
    pack_buttons_and_mark(&header, &scan_button, &sync_button);

    let side_search = gtk::SearchEntry::new();
    side_search.set_placeholder_text(Some("Filter playlists"));
    side_search.set_margin_start(8);
    side_search.set_margin_end(8);
    side_search.set_margin_top(8);
    side_search.set_margin_bottom(4);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side.add_css_class("sidebar-pane");
    side.append(&side_search);
    let side_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar)
        .build();
    side.append(&side_scroll);
    side.append(&new_button);
    side.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    side.append(&device_box);

    let grid_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&view)
        .build();
    let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main.append(&grid_scroll);
    main.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    main.append(&status);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&side));
    paned.set_end_child(Some(&main));
    let layouts = load_layouts();
    paned.set_position(layouts.sidebar_width());
    paned.set_shrink_start_child(false);
    window.set_child(Some(&paned));

    let ui = Rc::new(Ui {
        window: window.clone(),
        data,
        store,
        filter,
        query: RefCell::new(view_model::Query::default()),
        view: view.clone(),
        selection,
        columns,
        status,
        filtered,
        title,
        search: search.clone(),
        sidebar: sidebar.clone(),
        sources: RefCell::new(Vec::new()),
        current: RefCell::new(Source::Library),
        drag: RefCell::new(None),
        context_playlist: RefCell::new(None),
        layouts: RefCell::new(layouts),
        playlist_layout: Cell::new(false),
        reload_pending: Cell::new(false),
        monitor: RefCell::new(None),
        shown_mtime: Cell::new(None),
        row_menu,
        playlist_actions: RefCell::new(Vec::new()),
        show_in_action: RefCell::new(None),
        menu_track: Cell::new(None),
        info_action: RefCell::new(None),
        device_box: device_box.clone(),
        scan_action: RefCell::new(None),
        sync_action: RefCell::new(None),
        adopt_action: RefCell::new(None),
        selection_action: RefCell::new(None),
        device: RefCell::new(None),
        eject_action: RefCell::new(None),
        side_query: RefCell::new(view_model::Query::default()),
        picker,
        picker_search: picker_search.clone(),
        picker_list: picker_list.clone(),
        picker_names: RefCell::new(Vec::new()),
        picker_query: RefCell::new(view_model::Query::default()),
        pick_mode: Cell::new(Pick::Add),
        menu_point: Cell::new((0, 0)),
        paned: paned.clone(),
        writing: Cell::new(false),
        cancel: RefCell::new(None),
        busy: Cell::new(false),
        planning: Cell::new(false),
        volumes: RefCell::new(None),
        unwatched: Cell::new(false),
    });
    slot.set(Rc::downgrade(&ui)).expect("set once");
    let library_layout = ui.layouts.borrow().library.complete(false);
    ui.apply_layout(&library_layout);
    style::set_background(ui.background());

    // Actions, also reachable from the keyboard and the context menus.
    let remove = action(window, "remove", &ui, |ui| ui.remove_selected());
    let up = action(window, "move-up", &ui, |ui| ui.nudge(-1));
    let down = action(window, "move-down", &ui, |ui| ui.nudge(1));
    action(window, "show-in-folder", &ui, |ui| ui.show_in_folder());
    action(window, "new-playlist", &ui, |ui| ui.new_playlist());
    action(window, "background", &ui, |ui| ui.choose_background());
    action(window, "default-background", &ui, |ui| {
        ui.set_background(None)
    });
    // F2 renames the playlist on screen; the sidebar menu acts on the
    // playlist it was opened on.
    action(window, "rename-playlist", &ui, |ui| {
        if let Some(name) = ui.playlist_name() {
            ui.rename_playlist(name);
        }
    });
    action(window, "rename-clicked", &ui, |ui| {
        if let Some(name) = ui.context_playlist.take() {
            ui.rename_playlist(name);
        }
    });
    action(window, "delete-clicked", &ui, |ui| {
        if let Some(name) = ui.context_playlist.take() {
            ui.delete_playlist(name);
        }
    });
    *ui.playlist_actions.borrow_mut() = vec![remove.clone(), up.clone(), down.clone()];
    let scan = action(window, "scan", &ui, |ui| ui.scan_library());
    *ui.scan_action.borrow_mut() = Some(scan);
    let sync = action(window, "sync", &ui, |ui| ui.open_sync());
    *ui.sync_action.borrow_mut() = Some(sync);
    let adopt = action(window, "adopt", &ui, |ui| ui.open_adopt());
    *ui.adopt_action.borrow_mut() = Some(adopt);
    let selection = action(window, "selection", &ui, |ui| ui.choose_selection());
    *ui.selection_action.borrow_mut() = Some(selection);
    let eject = action(window, "eject", &ui, |ui| ui.eject());
    *ui.eject_action.borrow_mut() = Some(eject);
    // Opened once the grid menu has closed, where it was.
    action(window, "add-to-playlist", &ui, |ui| {
        let weak = Rc::downgrade(ui);
        glib::idle_add_local_once(move || {
            if let Some(ui) = weak.upgrade() {
                ui.open_picker(Pick::Add);
            }
        });
    });
    let show_in = action(window, "show-in-playlist", &ui, |ui| {
        let Some(id) = ui.menu_track.get() else {
            return;
        };
        let weak = Rc::downgrade(ui);
        glib::idle_add_local_once(move || {
            if let Some(ui) = weak.upgrade() {
                ui.open_picker(Pick::Show(id));
            }
        });
    });
    *ui.show_in_action.borrow_mut() = Some(show_in);
    // Opened once the grid menu has closed.
    let info = action(window, "get-info", &ui, |ui| {
        let weak = Rc::downgrade(ui);
        glib::idle_add_local_once(move || {
            if let Some(ui) = weak.upgrade() {
                ui.get_info();
            }
        });
    });
    *ui.info_action.borrow_mut() = Some(info);
    {
        let weak = Rc::downgrade(&ui);
        picker_list.set_filter_func(move |row| {
            let Some(ui) = weak.upgrade() else {
                return true;
            };
            let names = ui.picker_names.borrow();
            names
                .get(row.index() as usize)
                .is_none_or(|n| ui.picker_query.borrow().matches_name(n))
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        picker_search.connect_search_changed(move |s| {
            let Some(ui) = weak.upgrade() else { return };
            *ui.picker_query.borrow_mut() = view_model::Query::new(&s.text());
            ui.picker_list.invalidate_filter();
            ui.select_first_pick();
        });
    }
    {
        // Enter adds to the first playlist shown.
        let weak = Rc::downgrade(&ui);
        picker_search.connect_activate(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(row) = ui.picker_list.selected_row() {
                ui.pick(&row);
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        picker_search.connect_stop_search(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.picker.popdown();
            }
        });
    }
    {
        // Down goes from the filter into the list.
        let weak = Rc::downgrade(&ui);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(ui) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Down
                && let Some(row) = ui.picker_list.selected_row()
            {
                row.grab_focus();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        picker_search.add_controller(keys);
    }
    {
        let weak = Rc::downgrade(&ui);
        picker_list.connect_row_activated(move |_, row| {
            if let Some(ui) = weak.upgrade() {
                ui.pick(row);
            }
        });
    }
    ui.refresh_device();
    // An iPod plugged in or out updates the panel and the Sync button.
    let volumes = gio::VolumeMonitor::get();
    for signal in ["mount-added", "mount-removed"] {
        let weak = Rc::downgrade(&ui);
        volumes.connect_local(signal, false, move |_| {
            let weak = weak.clone();
            // The mount may not be readable yet when the signal comes.
            glib::timeout_add_local_once(Duration::from_millis(800), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.refresh_device();
                }
            });
            None
        });
    }
    *ui.volumes.borrow_mut() = Some(volumes);

    // Grid keys: Delete removes from the playlist, Ctrl+Up and Ctrl+Down
    // move. Local to the grid, so the search field keeps its own keys.
    let grid_keys = gtk::ShortcutController::new();
    // Before the grid's own Ctrl+Up and Ctrl+Down, which move the focus.
    grid_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    shortcut(&grid_keys, "Delete", "win.remove");
    shortcut(&grid_keys, "<Control>Up", "win.move-up");
    shortcut(&grid_keys, "<Control>Down", "win.move-down");
    view.add_controller(grid_keys);
    let window_keys = gtk::ShortcutController::new();
    window_keys.set_scope(gtk::ShortcutScope::Global);
    shortcut(&window_keys, "<Control>n", "win.new-playlist");
    shortcut(&window_keys, "F2", "win.rename-playlist");
    {
        // Not through the action, which the menu disables for rows
        // without a file: Ctrl+I looks at the selection when pressed.
        let weak = Rc::downgrade(&ui);
        window_keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>i"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.get_info();
                }
                glib::Propagation::Stop
            })),
        ));
    }
    {
        let search = search.clone();
        // Ctrl+F focuses the search, whatever Caps Lock says.
        window_keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::CallbackAction::new(move |_, _| {
                search.grab_focus();
                glib::Propagation::Stop
            })),
        ));
    }
    window.add_controller(window_keys);

    // Sidebar context menu on a playlist: rename or delete it.
    let side_menu = gio::Menu::new();
    side_menu.append(Some("Rename…"), Some("win.rename-clicked"));
    side_menu.append(Some("Delete…"), Some("win.delete-clicked"));
    {
        let weak = Rc::downgrade(&ui);
        context_menu(&sidebar, &side_menu, move |_, y| {
            let Some(ui) = weak.upgrade() else {
                return false;
            };
            let Some(row) = ui.sidebar.row_at_y(y as i32) else {
                return false;
            };
            let source = ui
                .sources
                .borrow()
                .get(row.index() as usize)
                .cloned()
                .flatten();
            match source {
                Some(Source::Playlist(name)) => {
                    *ui.context_playlist.borrow_mut() = Some(name);
                    true
                }
                _ => false,
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        let set_enabled = move |in_playlist: bool| {
            for a in [&remove, &up, &down] {
                a.set_enabled(in_playlist);
            }
        };
        sidebar.connect_row_selected(move |_, row| {
            let Some(ui) = weak.upgrade() else { return };
            let source = row.and_then(|r| {
                ui.sources
                    .borrow()
                    .get(r.index() as usize)
                    .cloned()
                    .flatten()
            });
            if let Some(source) = source {
                set_enabled(matches!(source, Source::Playlist(_)));
                ui.show(&source);
            }
        });
    }
    // The sidebar's filter hides the playlists it does not keep; the
    // headers, Music and the playlist on screen (one just created or
    // renamed, say) stay.
    {
        let weak = Rc::downgrade(&ui);
        sidebar.set_filter_func(move |row| {
            let Some(ui) = weak.upgrade() else {
                return true;
            };
            match ui.sources.borrow().get(row.index() as usize) {
                Some(Some(Source::Playlist(name))) => {
                    let shown = ui
                        .current
                        .try_borrow()
                        .is_ok_and(|c| matches!(&*c, Source::Playlist(n) if n == name));
                    shown || ui.side_query.borrow().matches_name(name)
                }
                _ => true,
            }
        });
    }
    {
        let weak = Rc::downgrade(&ui);
        side_search.connect_search_changed(move |s| {
            let Some(ui) = weak.upgrade() else { return };
            *ui.side_query.borrow_mut() = view_model::Query::new(&s.text());
            ui.sidebar.invalidate_filter();
        });
    }
    {
        // Enter shows the first playlist kept.
        let weak = Rc::downgrade(&ui);
        side_search.connect_activate(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            let query = ui.side_query.borrow().clone();
            let first = ui
                .sources
                .borrow()
                .iter()
                .position(|s| matches!(s, Some(Source::Playlist(n)) if query.matches_name(n)));
            if let Some(row) = first.and_then(|i| ui.sidebar.row_at_index(i as i32)) {
                ui.sidebar.select_row(Some(&row));
            }
        });
    }
    // Escape empties it.
    side_search.connect_stop_search(|s| s.set_text(""));
    {
        let weak = Rc::downgrade(&ui);
        search.connect_search_changed(move |s| {
            let Some(ui) = weak.upgrade() else { return };
            *ui.query.borrow_mut() = view_model::Query::new(&s.text());
            ui.filter.changed(gtk::FilterChange::Different);
            ui.update_status();
        });
    }

    // Playlist files changed by other programs are picked up.
    let dir = gio::File::for_path(ui.data.playlists_dir());
    if let Ok(monitor) =
        dir.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
    {
        let weak = Rc::downgrade(&ui);
        monitor.connect_changed(move |_, _, _, _| {
            if let Some(ui) = weak.upgrade() {
                ui.schedule_reload();
            }
        });
        *ui.monitor.borrow_mut() = Some(monitor);
    } else {
        ui.unwatched.set(true);
    }
    {
        let weak = Rc::downgrade(&ui);
        window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                if ui.writing.get() {
                    // Closing now would end the process in the middle of
                    // writing the iPod database.
                    return glib::Propagation::Stop;
                }
                ui.save_layouts();
            }
            glib::Propagation::Proceed
        });
    }
    // SIGTERM (logout, shutdown), SIGHUP and SIGINT: during a sync, the
    // same as Stop, and the process ends once the worker is done; otherwise
    // the window quits. The handler only sets a flag (it runs on its own
    // thread), which the main loop looks at.
    let _ = ctrlc::set_handler(|| SIGNALLED.store(true, std::sync::atomic::Ordering::SeqCst));
    {
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // During a sync the flag stays set: the sync is stopped now,
            // and the window quits once it is done.
            if SIGNALLED.load(std::sync::atomic::Ordering::SeqCst) {
                if ui.writing.get() {
                    if let Some(c) = &*ui.cancel.borrow() {
                        c.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                } else {
                    SIGNALLED.store(false, std::sync::atomic::Ordering::SeqCst);
                    ui.save_layouts();
                    if let Some(app) = ui.window.application() {
                        app.quit();
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
    // The window owns the state from here.
    keep_alive(window, ui.clone());
    ui.fill_sidebar();
}

/// Ties `ui` to the window's lifetime: everything else holds weak links.
fn keep_alive(window: &gtk::ApplicationWindow, ui: Rc<Ui>) {
    let cell = RefCell::new(Some(ui));
    window.connect_destroy(move |_| {
        cell.borrow_mut().take();
    });
}
