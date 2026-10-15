//! The window's colours, row stripes and sidebar, from `style.css`, on top
//! of the GTK theme. The colours come from `palette.css`, which follows the
//! theme, or from `picked.css` for a background colour picked from the menu.
//! Which rows are odd is set in `main::stripe`; on a picked colour, `stripes`
//! gives their colour.

use std::cell::OnceCell;

use gtk::gdk;

const THEME_PALETTE: &str = include_str!("palette.css");
const PICKED_PALETTE: &str = include_str!("picked.css");
const RULES: &str = include_str!("style.css");

/// Text on a picked background: whichever of the two reads better on it.
/// Pure black and white: any softer pair falls short of WCAG AA (4.5:1) on
/// mid-tones such as grey 119 or pure red.
const DARK_TEXT: Rgb = Rgb(0, 0, 0);
const LIGHT_TEXT: Rgb = Rgb(0xff, 0xff, 0xff);

/// How far an odd row moves from a picked background; a row under the
/// mouse moves twice as far.
const STRIPE_STEP: f64 = 0.15;

/// Text contrast the stripe and hover colours keep: WCAG AA.
const MIN_TEXT_CONTRAST: f64 = 4.5;

thread_local! {
    /// The one provider, reloaded when the background changes.
    static PROVIDER: OnceCell<gtk::CssProvider> = const { OnceCell::new() };
}

/// Loads the style sheet, with the theme's colours, for every window of the
/// application.
pub fn install() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&stylesheet(None));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    PROVIDER.with(|p| {
        let _ = p.set(provider);
    });
}

/// Repaints every window on `background`, or on the theme's colours.
pub fn set_background(background: Option<Rgb>) {
    PROVIDER.with(|p| {
        if let Some(provider) = p.get() {
            provider.load_from_string(&stylesheet(background));
        }
    });
}

/// The whole style sheet: a palette, then the rules that use it.
pub fn stylesheet(background: Option<Rgb>) -> String {
    match background {
        None => format!("{THEME_PALETTE}\n{RULES}"),
        Some(bg) => {
            let (text, away) = match bg.text() {
                DARK_TEXT => (DARK_TEXT, LIGHT_TEXT),
                _ => (LIGHT_TEXT, DARK_TEXT),
            };
            let (stripe, hover) = stripes(bg, text, away);
            format!(
                "@define-color ip_bg {};\n@define-color ip_fg {};\n\
                 @define-color ip_away {};\n@define-color ip_grid_odd {};\n\
                 @define-color ip_hover {};\n{PICKED_PALETTE}\n{RULES}",
                bg.hex(),
                text.hex(),
                away.hex(),
                stripe.hex(),
                hover.hex(),
            )
        }
    }
}

/// The odd rows' colour and the hovered row's on a picked background.
/// The stripe moves one step from the background, towards whichever of the
/// text or the other colour gives the bigger change, but towards the text
/// only while the text keeps AA on it: pale picks and saturated mid-tones go
/// towards the text, the rest away from it, which can only add contrast.
/// The hover goes two steps the same way, or two steps away from the text
/// when going towards it would drop below AA. Either way it stays clear of
/// the sidebar and title bar, one small step away from the text.
pub fn stripes(bg: Rgb, text: Rgb, away: Rgb) -> (Rgb, Rgb) {
    let (toward_away, toward_text) = (bg.mix(away, STRIPE_STEP), bg.mix(text, STRIPE_STEP));
    let (target, other) = if bg.contrast(toward_text) > bg.contrast(toward_away)
        && text.contrast(toward_text) >= MIN_TEXT_CONTRAST
    {
        (text, away)
    } else {
        (away, text)
    };
    let hover = bg.mix(target, 2.0 * STRIPE_STEP);
    let hover = if text.contrast(hover) >= MIN_TEXT_CONTRAST {
        hover
    } else {
        bg.mix(other, 2.0 * STRIPE_STEP)
    };
    (bg.mix(target, STRIPE_STEP), hover)
}

/// An opaque colour, saved in `ui.toml` as `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Reads `#rrggbb`, either case; anything else is None.
    pub fn parse(text: &str) -> Option<Rgb> {
        let hex = text.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Rgb(byte(0)?, byte(2)?, byte(4)?))
    }

    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// The colour a colour dialog returned, without its alpha.
    pub fn from_rgba(c: &gdk::RGBA) -> Rgb {
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Rgb(byte(c.red()), byte(c.green()), byte(c.blue()))
    }

    pub fn to_rgba(self) -> gdk::RGBA {
        let f = |v: u8| f32::from(v) / 255.0;
        gdk::RGBA::new(f(self.0), f(self.1), f(self.2), 1.0)
    }

    /// `t` of the way from this colour to `other`, channel by channel.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let channel =
            |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Rgb(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }

    /// Relative luminance, as WCAG defines it: 0 for black, 1 for white.
    fn luminance(self) -> f64 {
        let channel = |v: u8| {
            let c = f64::from(v) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.0) + 0.7152 * channel(self.1) + 0.0722 * channel(self.2)
    }

    /// The WCAG contrast ratio between two colours, from 1 to 21.
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Text for this background: dark or light, whichever contrasts more.
    pub fn text(self) -> Rgb {
        if self.contrast(DARK_TEXT) >= self.contrast(LIGHT_TEXT) {
            DARK_TEXT
        } else {
            LIGHT_TEXT
        }
    }
}

/// True for the rows that get the `odd` stripe: every second row by its
/// position in the list. An unset position (a row not in the list yet) is
/// never striped, though `INVALID_LIST_POSITION` is itself an odd number.
pub fn is_odd_row(position: u32) -> bool {
    position != gtk::INVALID_LIST_POSITION && position % 2 == 1
}

#[cfg(test)]
mod tests;
