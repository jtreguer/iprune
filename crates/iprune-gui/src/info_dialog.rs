//! The Get Info form, modelled on the Info tab of iTunes 10: the tags of
//! one song, or of several at once ("Multiple Item Information"), where a
//! check box by each field says it is to be written. What it shows and
//! sends is worked out in `view_model`; this is the widgets.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use iprune_core::store::TrackRow;
use iprune_core::tag_edit::{self, Changes};

use crate::view_model::{self, Completions, Entered, InfoField, Opened};

/// Suggestions shown under a field at most.
const SUGGESTIONS: usize = 8;

struct Field {
    field: InfoField,
    entry: gtk::Entry,
    /// For several songs: whether the field is written.
    tick: gtk::CheckButton,
    /// The list of values under a completing field.
    suggest: Option<(gtk::Popover, gtk::ListBox, RefCell<Vec<String>>)>,
}

pub struct InfoDialog {
    pub window: gtk::Window,
    several: bool,
    fields: Vec<Field>,
    compilation: gtk::CheckButton,
    compilation_tick: gtk::CheckButton,
    path: gtk::Label,
    facts: gtk::Label,
    message: gtk::Label,
    progress: gtk::ProgressBar,
    pub ok: gtk::Button,
    pub cancel: gtk::Button,
    pub stop: gtk::Button,
    pub previous: gtk::Button,
    pub next: gtk::Button,
    completions: Completions,
    opened: RefCell<Opened>,
    /// Set while the form is filled, so the fields do not tick themselves.
    filling: Cell<bool>,
    /// Why the songs shown cannot be edited, if they cannot.
    read_only: RefCell<Option<String>>,
    writing: Cell<bool>,
}

impl InfoDialog {
    pub fn new(
        parent: &gtk::ApplicationWindow,
        several: bool,
        completions: Completions,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .modal(true)
            .default_width(620)
            .resizable(false)
            .build();
        let grid = gtk::Grid::new();
        grid.set_row_spacing(8);
        grid.set_column_spacing(8);

        let make = |field: InfoField| {
            let entry = gtk::Entry::new();
            entry.set_activates_default(true);
            entry.set_hexpand(!field.numeric());
            if field.numeric() {
                entry.set_width_chars(if field == InfoField::Year { 5 } else { 3 });
                entry.set_max_width_chars(5);
                entry.set_input_purpose(gtk::InputPurpose::Digits);
            }
            let tick = gtk::CheckButton::new();
            tick.set_visible(several);
            tick.set_tooltip_text(Some("Write this field to every song"));
            let suggest = field.completes().then(|| {
                let list = gtk::ListBox::new();
                list.add_css_class("navigation-sidebar");
                list.set_activate_on_single_click(true);
                let popover = gtk::Popover::new();
                popover.set_child(Some(&list));
                popover.set_autohide(false);
                popover.set_has_arrow(false);
                popover.set_position(gtk::PositionType::Bottom);
                popover.set_parent(&entry);
                (popover, list, RefCell::new(Vec::new()))
            });
            Field {
                field,
                entry,
                tick,
                suggest,
            }
        };
        let fields: Vec<Field> = InfoField::ALL.into_iter().map(make).collect();
        let cell = |f: InfoField| {
            let field = &fields[InfoField::ALL.iter().position(|&x| x == f).unwrap()];
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            b.append(&field.tick);
            b.append(&field.entry);
            b.set_visible(!several || f.for_several());
            b
        };
        let label = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.set_xalign(1.0);
            l
        };
        let pair = |n: InfoField, total: InfoField| {
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let num = cell(n);
            b.append(&num);
            let of = gtk::Label::new(Some("of"));
            of.set_visible(num.is_visible());
            b.append(&of);
            b.append(&cell(total));
            b
        };

        let name_label = label("Name");
        name_label.set_visible(!several);
        grid.attach(&name_label, 0, 0, 1, 1);
        grid.attach(&cell(InfoField::Title), 1, 0, 3, 1);
        grid.attach(&label("Artist"), 0, 1, 1, 1);
        grid.attach(&cell(InfoField::Artist), 1, 1, 1, 1);
        grid.attach(&label("Year"), 2, 1, 1, 1);
        grid.attach(&cell(InfoField::Year), 3, 1, 1, 1);
        grid.attach(&label("Album Artist"), 0, 2, 1, 1);
        grid.attach(&cell(InfoField::AlbumArtist), 1, 2, 1, 1);
        grid.attach(
            &label(if several { "Track total" } else { "Track" }),
            2,
            2,
            1,
            1,
        );
        grid.attach(&pair(InfoField::Track, InfoField::TrackTotal), 3, 2, 1, 1);
        grid.attach(&label("Album"), 0, 3, 1, 1);
        grid.attach(&cell(InfoField::Album), 1, 3, 1, 1);
        grid.attach(&label("Disc"), 2, 3, 1, 1);
        grid.attach(&pair(InfoField::Disc, InfoField::DiscTotal), 3, 3, 1, 1);
        grid.attach(&label("Composer"), 0, 4, 1, 1);
        grid.attach(&cell(InfoField::Composer), 1, 4, 3, 1);
        grid.attach(&label("Genre"), 0, 5, 1, 1);
        grid.attach(&cell(InfoField::Genre), 1, 5, 1, 1);
        let compilation = gtk::CheckButton::with_label("Part of a compilation");
        let compilation_tick = gtk::CheckButton::new();
        compilation_tick.set_visible(several);
        compilation_tick.set_tooltip_text(Some("Write this field to every song"));
        let comp_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        comp_box.append(&compilation_tick);
        comp_box.append(&compilation);
        grid.attach(&comp_box, 2, 5, 2, 1);

        let path = gtk::Label::new(None);
        path.set_xalign(0.0);
        path.set_selectable(true);
        path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path.add_css_class("dim-label");
        let facts = gtk::Label::new(None);
        facts.set_xalign(0.0);
        facts.add_css_class("dim-label");
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.set_wrap(true);
        message.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        message.set_selectable(true);
        let progress = gtk::ProgressBar::new();
        progress.set_show_text(true);
        progress.set_visible(false);

        let previous = gtk::Button::with_label("Previous");
        let next = gtk::Button::with_label("Next");
        previous.set_visible(!several);
        next.set_visible(!several);
        let cancel = gtk::Button::with_label("Cancel");
        let stop = gtk::Button::with_label("Stop");
        stop.set_visible(false);
        let ok = gtk::Button::with_label("OK");
        ok.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.append(&previous);
        buttons.append(&next);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        buttons.append(&spacer);
        buttons.append(&stop);
        buttons.append(&cancel);
        buttons.append(&ok);

        let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        page.set_margin_top(16);
        page.set_margin_bottom(16);
        page.set_margin_start(16);
        page.set_margin_end(16);
        page.append(&grid);
        page.append(&path);
        page.append(&facts);
        page.append(&message);
        page.append(&progress);
        page.append(&buttons);
        window.set_child(Some(&page));
        window.set_default_widget(Some(&ok));

        let dialog = Rc::new(InfoDialog {
            window,
            several,
            fields,
            compilation,
            compilation_tick,
            path,
            facts,
            message,
            progress,
            ok,
            cancel,
            stop,
            previous,
            next,
            completions,
            opened: RefCell::new(Opened::new(&[])),
            filling: Cell::new(false),
            read_only: RefCell::new(None),
            writing: Cell::new(false),
        });
        dialog.connect();
        dialog
    }

    fn connect(self: &Rc<Self>) {
        for (i, f) in self.fields.iter().enumerate() {
            let weak = Rc::downgrade(self);
            f.entry.connect_changed(move |_| {
                let Some(d) = weak.upgrade() else { return };
                if d.filling.get() {
                    return;
                }
                d.fields[i].tick.set_active(true);
                d.suggest(i);
                d.validate();
            });
            let weak = Rc::downgrade(self);
            f.tick.connect_toggled(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.validate();
                }
            });
            let Some((popover, list, _)) = &f.suggest else {
                continue;
            };
            {
                let weak = Rc::downgrade(self);
                list.connect_row_activated(move |_, row| {
                    if let Some(d) = weak.upgrade() {
                        d.take_suggestion(i, row.index());
                    }
                });
            }
            {
                // Up and Down walk the list, Enter takes the value picked,
                // Escape closes it; the focus stays in the field.
                let weak = Rc::downgrade(self);
                let (popover, list) = (popover.clone(), list.clone());
                let keys = gtk::EventControllerKey::new();
                // Before the entry's text, which takes Enter to press OK.
                keys.set_propagation_phase(gtk::PropagationPhase::Capture);
                keys.connect_key_pressed(move |_, key, _, _| {
                    let Some(d) = weak.upgrade() else {
                        return glib::Propagation::Proceed;
                    };
                    if !popover.is_visible() {
                        return glib::Propagation::Proceed;
                    }
                    let n = d.fields[i]
                        .suggest
                        .as_ref()
                        .map_or(0, |(_, _, v)| v.borrow().len()) as i32;
                    let at = list.selected_row().map_or(-1, |r| r.index());
                    match key {
                        gdk::Key::Down => {
                            list.select_row(list.row_at_index((at + 1).min(n - 1)).as_ref());
                        }
                        gdk::Key::Up if at > 0 => {
                            list.select_row(list.row_at_index(at - 1).as_ref());
                        }
                        gdk::Key::Up => list.unselect_all(),
                        gdk::Key::Return | gdk::Key::KP_Enter if at >= 0 => {
                            d.take_suggestion(i, at);
                        }
                        gdk::Key::Escape => popover.popdown(),
                        _ => return glib::Propagation::Proceed,
                    }
                    glib::Propagation::Stop
                });
                f.entry.add_controller(keys);
            }
            {
                // The focus is in the entry's text, a child of it.
                let popover = popover.clone();
                let focus = gtk::EventControllerFocus::new();
                focus.connect_leave(move |_| popover.popdown());
                f.entry.add_controller(focus);
            }
            {
                let popover = popover.clone();
                f.entry.connect_destroy(move |_| popover.unparent());
            }
        }
        {
            let weak = Rc::downgrade(self);
            self.compilation.connect_toggled(move |c| {
                let Some(d) = weak.upgrade() else { return };
                if d.filling.get() {
                    return;
                }
                c.set_inconsistent(false);
                d.compilation_tick.set_active(true);
                d.validate();
            });
            let weak = Rc::downgrade(self);
            self.compilation_tick.connect_toggled(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.validate();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            self.window
                .connect_close_request(move |_| match weak.upgrade() {
                    Some(d) if d.writing.get() => glib::Propagation::Stop,
                    _ => glib::Propagation::Proceed,
                });
        }
        let keys = gtk::ShortcutController::new();
        let weak = self.window.downgrade();
        keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if let Some(w) = weak.upgrade() {
                    w.close();
                }
                glib::Propagation::Stop
            })),
        ));
        self.window.add_controller(keys);
    }

    /// Shows the values of the field at `i` starting with what it holds.
    fn suggest(&self, i: usize) {
        let f = &self.fields[i];
        let Some((popover, list, values)) = &f.suggest else {
            return;
        };
        let found =
            view_model::suggestions(self.completions.of(f.field), &f.entry.text(), SUGGESTIONS);
        while let Some(row) = list.row_at_index(0) {
            list.remove(&row);
        }
        for v in &found {
            let l = gtk::Label::new(Some(v));
            l.set_xalign(0.0);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            l.set_max_width_chars(48);
            list.append(&l);
        }
        let any = !found.is_empty();
        *values.borrow_mut() = found;
        if any
            && f.entry
                .state_flags()
                .contains(gtk::StateFlags::FOCUS_WITHIN)
        {
            popover.popup();
        } else {
            popover.popdown();
        }
    }

    fn take_suggestion(&self, i: usize, at: i32) {
        let f = &self.fields[i];
        let Some((popover, _, values)) = &f.suggest else {
            return;
        };
        let v = values.borrow().get(at as usize).cloned();
        popover.popdown();
        if let Some(v) = v {
            f.entry.set_text(&v);
            f.entry.set_position(-1);
        }
    }

    /// Shows `tracks`: their tags, and for one song its file. A song
    /// `tag_edit` would refuse is shown read-only, with why.
    pub fn fill(&self, tracks: &[TrackRow]) {
        self.filling.set(true);
        let tags: Vec<&iprune_core::tags::Tags> = tracks.iter().map(|t| &t.tags).collect();
        let opened = Opened::new(&tags);
        for f in &self.fields {
            f.entry.set_text(opened.get(f.field).unwrap_or(""));
            f.entry
                .set_placeholder_text(opened.get(f.field).is_none().then_some("Mixed"));
            f.tick.set_active(false);
            if let Some((popover, _, _)) = &f.suggest {
                popover.popdown();
            }
        }
        match opened.compilation {
            Some(on) => {
                self.compilation.set_inconsistent(false);
                self.compilation.set_active(on);
            }
            None => {
                self.compilation.set_active(false);
                self.compilation.set_inconsistent(true);
            }
        }
        self.compilation_tick.set_active(false);
        *self.opened.borrow_mut() = opened;
        let refused: Vec<(&TrackRow, String)> = tracks
            .iter()
            .filter_map(|t| tag_edit::refusal(t).map(|why| (t, why)))
            .collect();
        match tracks {
            [t] => {
                self.window.set_title(Some(&t.display_title()));
                // An empty name shows what the grid shows for it.
                if t.tags.title.is_none() {
                    self.field(InfoField::Title)
                        .entry
                        .set_placeholder_text(Some(&t.display_title()));
                }
                self.path.set_text(&t.path.display().to_string());
                self.path
                    .set_tooltip_text(Some(&t.path.display().to_string()));
                self.facts.set_text(&view_model::facts(t));
            }
            _ => {
                self.window.set_title(Some(&format!(
                    "Multiple Item Information ({} songs)",
                    tracks.len()
                )));
                self.path.set_visible(false);
                self.facts.set_visible(false);
            }
        }
        let read_only = match (tracks.len(), refused.as_slice()) {
            (1, [(_, why)]) => Some(format!("This song cannot be edited: {why}.")),
            (n, r) if !r.is_empty() && r.len() == n => {
                Some("None of these songs can be edited.".to_string())
            }
            _ => None,
        };
        let note = match (&read_only, refused.len()) {
            (Some(r), _) => Some(r.clone()),
            (None, 0) => None,
            (None, n) => Some(format!(
                "{n} of these songs cannot be edited and will be left as they are: {}.",
                refused
                    .iter()
                    .map(|(t, why)| format!("{} ({why})", t.display_title()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
        for f in &self.fields {
            f.entry.set_editable(read_only.is_none());
            f.tick.set_sensitive(read_only.is_none());
        }
        self.compilation.set_sensitive(read_only.is_none());
        self.compilation_tick.set_sensitive(read_only.is_none());
        *self.read_only.borrow_mut() = note;
        self.ok.set_sensitive(read_only.is_none());
        self.filling.set(false);
        self.validate();
    }

    fn field(&self, f: InfoField) -> &Field {
        self.fields
            .iter()
            .find(|x| x.field == f)
            .expect("every field")
    }

    fn entered(&self) -> Entered {
        let mut e = Entered::new(&self.opened.borrow());
        for f in &self.fields {
            e.set(f.field, &f.entry.text());
            e.tick(f.field, f.tick.is_active());
        }
        e.compilation = (!self.compilation.is_inconsistent()).then(|| self.compilation.is_active());
        e.compilation_ticked = self.compilation_tick.is_active();
        e
    }

    /// The edit OK would send, or why it cannot be sent.
    pub fn changes(&self) -> Result<Changes, String> {
        view_model::info_changes(self.several, &self.opened.borrow(), &self.entered())
    }

    /// OK is enabled when the form can be written; the line under it says
    /// why not, or what a cleared field means.
    fn validate(&self) {
        if self.writing.get() {
            return;
        }
        let read_only = self.read_only.borrow().clone();
        let (ok, text) = match self.changes() {
            Err(why) => (false, Some(why)),
            Ok(_) if self.field(InfoField::Title).entry.is_editable() => {
                let clears =
                    view_model::info_clears(self.several, &self.opened.borrow(), &self.entered());
                (
                    true,
                    clears.then(|| "Cleared fields stay as they are on the iPod.".to_string()),
                )
            }
            Ok(_) => (false, None),
        };
        self.ok.set_sensitive(ok);
        let text = [read_only, text]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        self.message.set_text(&text);
    }

    /// Says `text` under the form, in place of the usual line.
    pub fn say(&self, text: &str) {
        self.message.set_text(text);
    }

    /// While the files are written the form and the buttons are off, but
    /// for Stop when several songs are written; the dialog cannot close.
    pub fn set_writing(&self, on: bool) {
        self.writing.set(on);
        self.window.set_deletable(!on);
        for f in &self.fields {
            f.entry.set_sensitive(!on);
            f.tick.set_sensitive(!on);
        }
        self.compilation.set_sensitive(!on);
        self.compilation_tick.set_sensitive(!on);
        for b in [&self.ok, &self.cancel, &self.previous, &self.next] {
            b.set_sensitive(!on);
        }
        self.stop.set_visible(on && self.several);
        self.stop.set_sensitive(on);
        self.progress.set_visible(on && self.several);
        if on {
            self.progress.set_fraction(0.0);
            self.progress.set_text(Some("Writing…"));
            self.message.set_text("");
        } else {
            self.validate();
        }
    }

    pub fn show_progress(&self, done: usize, total: usize) {
        if total > 0 {
            self.progress.set_fraction(done as f64 / total as f64);
        }
        self.progress
            .set_text(Some(&format!("Writing {} of {total}", done + 1)));
    }

    /// Replaces the form by a report and a Close button: after a write of
    /// several songs that did not go through for all of them.
    pub fn show_report(&self, text: &str) {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        page.set_margin_top(16);
        page.set_margin_bottom(16);
        page.set_margin_start(16);
        page.set_margin_end(16);
        let heading = gtk::Label::new(Some("Some songs were not edited"));
        heading.add_css_class("title-2");
        heading.set_xalign(0.0);
        let body = gtk::Label::new(Some(text));
        body.set_wrap(true);
        body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        body.set_xalign(0.0);
        body.set_selectable(true);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(360)
            .child(&body)
            .build();
        let close = gtk::Button::with_label("Close");
        close.set_halign(gtk::Align::End);
        let weak = self.window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.close();
            }
        });
        page.append(&heading);
        page.append(&scroll);
        page.append(&close);
        self.window.set_child(Some(&page));
        self.window.set_default_widget(Some(&close));
        close.grab_focus();
    }
}
