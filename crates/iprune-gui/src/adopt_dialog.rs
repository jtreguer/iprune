//! The adoption dialog: reads the iPod on a worker, shows the proposals as
//! a grid, one row per iPod track with a choice of library file, and saves
//! the mapping. The iPod is only read.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::adopt_job::{self, Review};
use crate::{Ui, joined, result_page};

/// The index of the row a grid item stands for.
fn index_of(obj: &glib::Object) -> usize {
    *obj.downcast_ref::<glib::BoxedAnyObject>()
        .expect("the grid holds indexes")
        .borrow::<usize>()
}

/// A plain text column over the rows.
fn text_column(
    title: &str,
    rows: &Rc<Vec<adopt_job::ReviewRow>>,
    width: i32,
    text: fn(&adopt_job::ReviewRow) -> String,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        item.set_child(Some(&label));
    });
    let rows = rows.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = item.child().and_downcast::<gtk::Label>().expect("a label");
        let row = &rows[index_of(&item.item().expect("an item"))];
        let t = text(row);
        label.set_tooltip_text(Some(&t));
        label.set_text(&t);
    });
    let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    column.set_resizable(true);
    column.set_fixed_width(width);
    column
}

/// Labels for a drop-down's strings: shortened on its button, in full in
/// its list.
fn label_factory(shorten: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        if shorten {
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let label = item.child().and_downcast::<gtk::Label>().expect("a label");
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string().to_string())
            .unwrap_or_default();
        label.set_tooltip_text(Some(&text));
        label.set_text(&text);
    });
    factory
}

/// The column where each proposal gets its decision. `changed` runs after
/// the user picks something.
fn choice_column(
    rows: &Rc<Vec<adopt_job::ReviewRow>>,
    picked: &Rc<RefCell<Vec<usize>>>,
    changed: Rc<dyn Fn()>,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    // Set while a widget is bound to its row: what binding selects is not
    // a pick, and the conflicts are checked once at the end. One flag for
    // the column will do, since binds run one at a time.
    let binding = Rc::new(Cell::new(false));
    {
        let (picked, binding, changed) = (picked.clone(), binding.clone(), changed.clone());
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let drop_down = gtk::DropDown::new(None::<gtk::StringList>, None::<gtk::Expression>);
            drop_down.set_factory(Some(&label_factory(true)));
            drop_down.set_list_factory(Some(&label_factory(false)));
            // Records the pick of the row this widget shows now: a
            // recycled widget moves between rows.
            let weak_item = item.downgrade();
            let (picked, binding, changed) = (picked.clone(), binding.clone(), changed.clone());
            drop_down.connect_selected_notify(move |d| {
                if binding.get() {
                    return;
                }
                let Some(i) = weak_item
                    .upgrade()
                    .and_then(|it| it.item())
                    .map(|o| index_of(&o))
                else {
                    return;
                };
                if let Some(slot) = picked.borrow_mut().get_mut(i) {
                    *slot = d.selected() as usize;
                }
                changed();
            });
            item.set_child(Some(&drop_down));
        });
    }
    let rows = rows.clone();
    let picked = picked.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let drop_down = item
            .child()
            .and_downcast::<gtk::DropDown>()
            .expect("a drop-down");
        let i = index_of(&item.item().expect("an item"));
        let want = picked.borrow().get(i).copied().unwrap_or(0);
        let labels: Vec<&str> = rows[i].choices.iter().map(|c| c.label.as_str()).collect();
        binding.set(true);
        drop_down.set_model(Some(&gtk::StringList::new(&labels)));
        drop_down.set_selected(want as u32);
        binding.set(false);
    });
    let column = gtk::ColumnViewColumn::new(Some("Library file"), Some(factory));
    column.set_resizable(true);
    column.set_expand(true);
    column
}

impl Ui {
    /// Reads the iPod and the library on a worker and shows the proposals
    /// to decide; Save records the mapping, and Sync is then possible.
    pub(crate) fn open_adopt(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.refresh_device();
        let dialog = gtk::Window::builder()
            .title("Adopt iPod")
            .transient_for(&self.window)
            .modal(true)
            .default_width(1100)
            .default_height(700)
            .build();
        // Set while a worker runs: closing the dialog then ends the
        // adoption only once the worker is done.
        let working = Rc::new(Cell::new(true));
        let gone = Rc::new(Cell::new(false));
        {
            let (ui, working, gone) = (Rc::downgrade(self), working.clone(), gone.clone());
            dialog.connect_destroy(move |_| {
                gone.set(true);
                if let Some(ui) = ui.upgrade()
                    && !working.get()
                {
                    ui.busy.set(false);
                    ui.refresh_device();
                }
            });
        }
        let stack = gtk::Stack::new();
        dialog.set_child(Some(&stack));
        let progress = progress_page(&stack, "Scanning the library and reading the iPod…");
        dialog.present();

        let (tx, rx) = async_channel::unbounded::<(usize, usize)>();
        glib::spawn_future_local(async move {
            while let Ok((done, total)) = rx.recv().await {
                progress.set_text(&format!(
                    "Comparing iPod files with the library: {done} of {total}"
                ));
            }
        });
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let dialog_weak = dialog.downgrade();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let progress = |done: usize, total: usize| {
                    let _ = tx.send_blocking((done, total));
                };
                adopt_job::prepare(&config, &paths, &progress)
            })
            .await;
            let Some(ui) = ui.upgrade() else { return };
            working.set(false);
            let dialog = match dialog_weak.upgrade() {
                Some(d) if !gone.get() => d,
                _ => {
                    ui.busy.set(false);
                    ui.refresh_device();
                    return;
                }
            };
            // The scan may have changed the index.
            ui.refresh();
            match joined(result) {
                Ok(review) => ui.review_page(&dialog, &stack, review, working),
                Err(e) => result_page(
                    &dialog,
                    &stack,
                    "The iPod could not be adopted",
                    &format!("{e:#}\n\nNothing was saved."),
                ),
            }
        });
    }

    fn review_page(
        self: &Rc<Self>,
        dialog: &gtk::Window,
        stack: &gtk::Stack,
        review: Review,
        working: Rc<Cell<bool>>,
    ) {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
        page.set_margin_top(16);
        page.set_margin_bottom(16);
        page.set_margin_start(16);
        page.set_margin_end(16);
        let heading = gtk::Label::new(Some(&format!("Adopt {}", review.ipod_name)));
        heading.add_css_class("title-2");
        heading.set_xalign(0.0);
        page.append(&heading);
        let summary = gtk::Label::new(Some(review.summary.trim_end()));
        summary.add_css_class("monospace");
        summary.set_xalign(0.0);
        summary.set_selectable(true);
        // Long lines and paths wrap anywhere, so the dialog keeps its width.
        summary.set_wrap(true);
        summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        page.append(&summary);
        let intro = gtk::Label::new(Some(&if review.rows.is_empty() {
            format!(
                "No match needs a decision. Save records the matches; nothing on the iPod \
                 changes. Full report: {}",
                review.report.display()
            )
        } else {
            format!(
                "Each iPod track below may be one of several library files. Pick the one that \
                 holds the same song, or leave the track unmanaged: it stays on the iPod and \
                 iprune leaves it alone. Save records the matches; nothing on the iPod changes. \
                 Full report: {}",
                review.report.display()
            )
        }));
        intro.set_wrap(true);
        intro.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        intro.set_xalign(0.0);
        page.append(&intro);

        let rows = Rc::new(review.rows.clone());
        let picked = Rc::new(RefCell::new(
            rows.iter().map(|r| r.suggested).collect::<Vec<_>>(),
        ));
        let review = Rc::new(RefCell::new(Some(review)));
        let problems = gtk::Label::new(None);
        problems.set_wrap(true);
        problems.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        problems.set_xalign(0.0);
        problems.add_css_class("error");
        let save = gtk::Button::with_mnemonic("_Save");
        save.add_css_class("suggested-action");
        // Save is refused while a library file is picked twice.
        let check: Rc<dyn Fn()> = {
            let (review, picked, problems, save) = (
                review.clone(),
                picked.clone(),
                problems.clone(),
                save.clone(),
            );
            Rc::new(move || {
                let lines = review
                    .borrow()
                    .as_ref()
                    .map(|r| adopt_job::conflicts(r, &picked.borrow()))
                    .unwrap_or_default();
                problems.set_text(&lines.join("\n"));
                problems.set_visible(!lines.is_empty());
                save.set_sensitive(lines.is_empty());
            })
        };

        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let fill = {
            let store = store.clone();
            let n = rows.len();
            move || {
                let objects: Vec<glib::Object> = (0..n)
                    .map(|i| glib::BoxedAnyObject::new(i).upcast())
                    .collect();
                store.remove_all();
                store.extend_from_slice(&objects);
            }
        };
        fill();
        if !rows.is_empty() {
            let view = gtk::ColumnView::new(Some(gtk::NoSelection::new(Some(store.clone()))));
            view.add_css_class("data-table");
            view.set_show_column_separators(true);
            view.append_column(&text_column("iPod track", &rows, 280, |r| r.ipod.clone()));
            view.append_column(&text_column("Album", &rows, 160, |r| r.album.clone()));
            view.append_column(&text_column("Size, time", &rows, 120, |r| r.detail.clone()));
            view.append_column(&text_column("Why", &rows, 200, |r| r.why.to_owned()));
            view.append_column(&choice_column(&rows, &picked, check.clone()));
            let scroll = gtk::ScrolledWindow::builder()
                .child(&view)
                .vexpand(true)
                .min_content_height(240)
                .build();
            page.append(&scroll);
        }
        page.append(&problems);

        let cancel = gtk::Button::with_mnemonic("_Cancel");
        let suggested = gtk::Button::with_mnemonic("Use All _Suggestions");
        let none = gtk::Button::with_mnemonic("Leave All _Unmanaged");
        for (button, all) in [(&suggested, true), (&none, false)] {
            let (rows, picked, check, fill) =
                (rows.clone(), picked.clone(), check.clone(), fill.clone());
            button.connect_clicked(move |_| {
                *picked.borrow_mut() = rows
                    .iter()
                    .map(|r| if all { r.suggested } else { 0 })
                    .collect();
                // Shown again with the new picks.
                fill();
                check();
            });
        }
        suggested.set_visible(!rows.is_empty());
        none.set_visible(!rows.is_empty());
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&suggested);
        buttons.append(&none);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        buttons.append(&spacer);
        buttons.append(&cancel);
        buttons.append(&save);
        page.append(&buttons);
        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let ui = Rc::downgrade(self);
            let (dialog, stack) = (dialog.clone(), stack.clone());
            save.connect_clicked(move |_| {
                let (Some(ui), Some(review)) = (ui.upgrade(), review.borrow_mut().take()) else {
                    return;
                };
                ui.save_adoption(
                    &dialog,
                    &stack,
                    review,
                    picked.borrow().clone(),
                    working.clone(),
                );
            });
        }
        check();
        stack.add_named(&page, Some("review"));
        stack.set_visible_child(&page);
    }

    fn save_adoption(
        self: &Rc<Self>,
        dialog: &gtk::Window,
        stack: &gtk::Stack,
        review: Review,
        picked: Vec<usize>,
        working: Rc<Cell<bool>>,
    ) {
        working.set(true);
        progress_page(stack, "Saving…");
        let config = self.data.config().clone();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let (dialog, stack) = (dialog.clone(), stack.clone());
        glib::spawn_future_local(async move {
            let result =
                gio::spawn_blocking(move || adopt_job::save(&config, &paths, review, &picked))
                    .await;
            working.set(false);
            let Some(ui) = ui.upgrade() else { return };
            match joined(result) {
                Ok(saved) => result_page(
                    &dialog,
                    &stack,
                    "iPod adopted",
                    &format!(
                        "{} tracks and {} playlists are now managed by iprune. {} iPod tracks are \
                         left unmanaged: they stay on the iPod as they are. {} library tracks are \
                         not on the iPod.\n\nSync shows what it would copy, update and change in \
                         playlists before writing anything.",
                        saved.tracks, saved.playlists, saved.unmanaged, saved.to_copy
                    ),
                ),
                Err(e) => result_page(
                    &dialog,
                    &stack,
                    "The adoption was not saved",
                    &format!("{e:#}"),
                ),
            }
            // Closed while saving: the dialog's destroy handler saw the
            // worker running, so the adoption ends here.
            if !dialog.is_visible() {
                ui.busy.set(false);
            }
            ui.refresh_device();
        });
    }
}

/// A page with a spinner and a line of text the caller can update.
fn progress_page(stack: &gtk::Stack, text: &str) -> gtk::Label {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_valign(gtk::Align::Center);
    page.set_halign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.start();
    let label = gtk::Label::new(Some(text));
    page.append(&spinner);
    page.append(&label);
    stack.add_child(&page);
    stack.set_visible_child(&page);
    label
}
