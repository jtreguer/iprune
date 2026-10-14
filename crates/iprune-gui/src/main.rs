//! The iprune window: a sidebar of sources and a grid of tracks.

mod data;
mod view_model;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use data::{Data, DeviceInfo};
use view_model::{Column, Row};

const APP_ID: &str = "dev.iprune.Iprune";

/// What the grid is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Library,
    Playlist(String),
}

struct Ui {
    data: Data,
    store: gio::ListStore,
    filter: gtk::CustomFilter,
    query: Rc<RefCell<view_model::Query>>,
    view: gtk::ColumnView,
    columns: Vec<(Column, gtk::ColumnViewColumn)>,
    status: gtk::Label,
    filtered: gtk::FilterListModel,
    title: gtk::Label,
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(activate);
    // GTK does not see iprune's own arguments.
    app.run_with_args::<&str>(&[])
}

fn activate(app: &gtk::Application) {
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

fn cell_label(numeric: bool) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(if numeric { 1.0 } else { 0.0 });
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_single_line_mode(true);
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

fn make_column(col: Column) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        item.set_child(Some(&cell_label(col.numeric())));
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
            label.add_css_class("dim-label");
        } else {
            label.remove_css_class("dim-label");
        }
        if col == Column::Name {
            label.set_tooltip_text(Some(&row.path.to_string_lossy()));
        }
    });
    let column = gtk::ColumnViewColumn::new(Some(col.title()), Some(factory));
    column.set_resizable(true);
    column.set_fixed_width(col.width());
    column.set_visible(col.visible_by_default());
    column.set_expand(col == Column::Name);
    let sorter = gtk::CustomSorter::new(move |a, b| col.compare(&row_of(a), &row_of(b)).into());
    column.set_sorter(Some(&sorter));
    column
}

fn sidebar_header(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label.add_css_class("dim-label");
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
        lines.push("run `iprune adopt` to manage it".into());
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
    if d.adopted {
        lines.push(match d.last_sync {
            Some(t) => format!("last sync {}", date(t)),
            None => "never synced".into(),
        });
    }
    lines.join("\n")
}

impl Ui {
    fn show(&self, source: &Source) {
        let rows = match source {
            Source::Library => self.data.library(),
            Source::Playlist(name) => self.data.playlist(name),
        };
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                // Never leave the previous source's rows under the new title.
                self.store.remove_all();
                self.title.set_text(match source {
                    Source::Library => "Music",
                    Source::Playlist(name) => name,
                });
                self.status.set_text(&format!("{e:#}"));
                return;
            }
        };
        let objects: Vec<glib::Object> = rows
            .into_iter()
            .map(|r| glib::BoxedAnyObject::new(r).upcast())
            .collect();
        let in_playlist = matches!(source, Source::Playlist(_));
        let (sort_by, title) = match source {
            Source::Library => (Column::Artist, "Music".to_owned()),
            Source::Playlist(name) => (Column::Position, name.clone()),
        };
        // Sort order first, then the rows, so they are sorted once.
        self.store.remove_all();
        for (col, c) in &self.columns {
            if *col == Column::Position {
                c.set_visible(in_playlist);
            }
            if *col == sort_by {
                self.view.sort_by_column(Some(c), gtk::SortType::Ascending);
            }
        }
        self.store.extend_from_slice(&objects);
        self.title.set_text(&title);
        self.update_status();
    }

    fn update_status(&self) {
        let n = self.filtered.n_items();
        let objects: Vec<glib::Object> = (0..n).filter_map(|i| self.filtered.item(i)).collect();
        let borrowed: Vec<std::cell::Ref<'_, Row>> = objects.iter().map(row_of).collect();
        let summary = view_model::summary(borrowed.iter().map(|r| &**r));
        // A playlists folder that could not be read stays visible: the
        // playlists shown may be out of date.
        match &self.data.warning {
            Some(w) => self.status.set_text(&format!("{summary}  |  {w}")),
            None => self.status.set_text(&summary),
        }
    }
}

fn build(window: &gtk::ApplicationWindow, data: Data) {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let query = Rc::new(RefCell::new(view_model::Query::default()));
    let filter = {
        let query = query.clone();
        gtk::CustomFilter::new(move |obj| row_of(obj).matches(&query.borrow()))
    };
    let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let view = gtk::ColumnView::new(None::<gtk::MultiSelection>);
    view.set_show_column_separators(true);
    // Compact rows, as a music grid needs (GTK 4.12).
    view.add_css_class("data-table");
    view.set_reorderable(true);
    let mut columns = Vec::new();
    for col in Column::ALL {
        let c = make_column(col);
        view.append_column(&c);
        columns.push((col, c));
    }
    let sorted = gtk::SortListModel::new(Some(filtered.clone()), view.sorter());
    let selection = gtk::MultiSelection::new(Some(sorted));
    view.set_model(Some(&selection));

    // Header: title and search.
    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Music"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search"));
    search.set_width_chars(28);
    header.pack_end(&search);
    window.set_titlebar(Some(&header));

    // Columns menu: show or hide each column.
    let menu = gio::Menu::new();
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
        menu.append(Some(col.title()), Some(&format!("columns.{name}")));
    }
    window.insert_action_group("columns", Some(&actions));
    let columns_button = gtk::MenuButton::new();
    columns_button.set_icon_name("view-more-symbolic");
    columns_button.set_tooltip_text(Some("Columns"));
    columns_button.set_menu_model(Some(&menu));
    header.pack_start(&columns_button);

    let status = gtk::Label::new(None);
    status.set_xalign(0.5);
    status.add_css_class("dim-label");
    status.set_margin_top(4);
    status.set_margin_bottom(4);

    // Sidebar.
    let sidebar = gtk::ListBox::new();
    sidebar.add_css_class("navigation-sidebar");
    let mut sources: Vec<Option<Source>> = Vec::new();
    sidebar.append(&sidebar_header("Library"));
    sources.push(None);
    sidebar.append(&sidebar_item("Music"));
    sources.push(Some(Source::Library));
    sidebar.append(&sidebar_header("Playlists"));
    sources.push(None);
    for name in data.playlist_names().unwrap_or_default() {
        sidebar.append(&sidebar_item(&name));
        sources.push(Some(Source::Playlist(name)));
    }
    let device_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if let Ok(Some(d)) = data.device() {
        let label = gtk::Label::new(Some(&device_text(&d)));
        label.set_xalign(0.0);
        label.set_margin_start(16);
        label.set_margin_top(4);
        label.set_margin_bottom(12);
        if !d.connected {
            label.add_css_class("dim-label");
        }
        let heading = gtk::Label::new(Some("Device"));
        heading.set_xalign(0.0);
        heading.add_css_class("heading");
        heading.add_css_class("dim-label");
        heading.set_margin_top(12);
        heading.set_margin_start(8);
        device_box.append(&heading);
        device_box.append(&label);
    }

    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let side_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar)
        .build();
    side.append(&side_scroll);
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
    paned.set_position(260);
    paned.set_shrink_start_child(false);
    window.set_child(Some(&paned));

    let ui = Rc::new(Ui {
        data,
        store,
        filter,
        query,
        view,
        columns,
        status,
        filtered,
        title,
    });

    let sources = Rc::new(sources);
    {
        let ui = ui.clone();
        let sources = sources.clone();
        sidebar.connect_row_selected(move |_, row| {
            if let Some(Some(source)) = row.and_then(|r| sources.get(r.index() as usize)) {
                ui.show(source);
            }
        });
    }
    {
        let ui = ui.clone();
        search.connect_search_changed(move |s| {
            *ui.query.borrow_mut() = view_model::Query::new(&s.text());
            ui.filter.changed(gtk::FilterChange::Different);
            ui.update_status();
        });
    }
    // Ctrl+F focuses the search, whatever Caps Lock says.
    let shortcuts = gtk::ShortcutController::new();
    shortcuts.set_scope(gtk::ShortcutScope::Global);
    {
        let search = search.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::CallbackAction::new(move |_, _| {
                search.grab_focus();
                glib::Propagation::Stop
            })),
        ));
    }
    window.add_controller(shortcuts);

    if let Some(music) = sidebar.row_at_index(1) {
        sidebar.select_row(Some(&music));
    }
}
