use super::*;

#[test]
fn odd_positions_are_striped_even_are_not() {
    assert!(!is_odd_row(0));
    assert!(is_odd_row(1));
    assert!(!is_odd_row(2));
    assert!(is_odd_row(3));
    assert!(is_odd_row(1_001));
    assert!(!is_odd_row(1_000));
}

// The invalid position is u32::MAX, an odd number: it must not be striped.
#[test]
fn an_unset_position_is_not_striped() {
    assert_eq!(
        gtk::INVALID_LIST_POSITION % 2,
        1,
        "the premise of this test"
    );
    assert!(!is_odd_row(gtk::INVALID_LIST_POSITION));
}

/// Parses `css` and returns every parsing error GTK reports, or None when
/// there is no display to initialise GTK on (nothing is shown either way).
fn parse_errors(css: &str) -> Option<Vec<String>> {
    if gtk::init().is_err() {
        eprintln!("SKIPPED: no display, GTK cannot start; stylesheet not checked");
        return None;
    }
    let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let provider = gtk::CssProvider::new();
    {
        let errors = errors.clone();
        provider.connect_parsing_error(move |_, section, error| {
            errors
                .borrow_mut()
                .push(format!("{}: {error}", section.start_location().lines()));
        });
    }
    provider.load_from_string(css);
    let found = errors.borrow().clone();
    Some(found)
}

// GTK drops a rule it cannot parse and carries on; this makes that loud.
// This is the only test that touches GTK: gtk::init is tied to one thread.
#[test]
fn the_stylesheet_parses_without_errors() {
    for background in [
        None,
        Some(Rgb(0x2b, 0x1d, 0x4e)),
        Some(Rgb(0xf4, 0xe8, 0xc1)),
    ] {
        if let Some(errors) = parse_errors(&stylesheet(background)) {
            assert!(
                errors.is_empty(),
                "{background:?}: parsing errors: {errors:#?}"
            );
        }
    }
}

// GTK resolves `@name` colours at style time, so an undefined one is not a
// parsing error: the declaration is just dropped. Check the names by hand.
// Needs no display.
#[test]
fn every_named_colour_is_defined() {
    const THEME: &[&str] = &[
        "theme_bg_color",
        "theme_base_color",
        "theme_fg_color",
        "theme_selected_bg_color",
        "theme_selected_fg_color",
        "borders",
    ];
    for css in [stylesheet(None), stylesheet(Some(Rgb(0x2b, 0x1d, 0x4e)))] {
        check_colours(&css, THEME);
    }
}

fn check_colours(css: &str, theme: &[&str]) {
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let defined: Vec<&str> = css
        .split("@define-color ")
        .skip(1)
        .map(|rest| rest.split(|c: char| !name_char(c)).next().unwrap())
        .collect();
    assert!(defined.contains(&"ip_grid"), "the scan finds definitions");
    let mut unknown = Vec::new();
    for (i, _) in css.match_indices('@') {
        let name: String = css[i + 1..].chars().take_while(|&c| name_char(c)).collect();
        if name == "define" || name.is_empty() {
            continue;
        }
        if !defined.contains(&name.as_str()) && !theme.contains(&name.as_str()) {
            unknown.push(name);
        }
    }
    assert!(unknown.is_empty(), "undefined colours: {unknown:?}");
}

// ------------------------------------------------------------------ Rgb

#[test]
fn parse_reads_six_hex_digits_in_either_case() {
    assert_eq!(Rgb::parse("#2b1d4e"), Some(Rgb(0x2b, 0x1d, 0x4e)));
    assert_eq!(Rgb::parse("#2B1D4E"), Some(Rgb(0x2b, 0x1d, 0x4e)));
    assert_eq!(Rgb::parse("#AbCdEf"), Some(Rgb(0xab, 0xcd, 0xef)));
    assert_eq!(Rgb::parse("#000000"), Some(Rgb(0, 0, 0)));
    assert_eq!(Rgb::parse("#ffffff"), Some(Rgb(255, 255, 255)));
}

#[test]
fn parse_rejects_anything_else_without_panicking() {
    for bad in [
        "",
        "#",
        "2b1d4e",       // no '#'
        "#2b1d4",       // too short
        "#2b1d4e0",     // too long
        "#2b1d4eff",    // with alpha
        "#gggggg",      // not hex
        "#2b1d4g",      // one bad digit
        "# 2b1d4e",     // space
        " #2b1d4e",     // leading space
        "#2b1d4e ",     // trailing space
        "#+b1d4e",      // sign accepted by from_str_radix
        "##2b1d4e",     // two marks
        "#ééé",         // six bytes, three characters, slicing would panic
        "#é2b1d",       // multi-byte at a slice boundary
        "#2b1d4é",      // multi-byte last
        "#日本",        // six bytes
        "#\u{1F3A8}ab", // four-byte character
    ] {
        assert_eq!(Rgb::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn hex_is_lowercase_padded_and_round_trips() {
    assert_eq!(Rgb(0, 0, 0).hex(), "#000000");
    assert_eq!(Rgb(1, 2, 3).hex(), "#010203");
    assert_eq!(Rgb(0xab, 0xcd, 0xef).hex(), "#abcdef");
    for c in [
        Rgb(0, 0, 0),
        Rgb(255, 255, 255),
        Rgb(0x2b, 0x1d, 0x4e),
        Rgb(7, 128, 250),
    ] {
        assert_eq!(Rgb::parse(&c.hex()), Some(c));
    }
}

#[test]
fn rgba_round_trips_every_channel_value() {
    for v in 0..=255u8 {
        let c = Rgb(v, 255 - v, v / 2);
        assert_eq!(Rgb::from_rgba(&c.to_rgba()), c, "{c:?}");
    }
    assert_eq!(Rgb(10, 20, 30).to_rgba().alpha(), 1.0);
}

#[test]
fn from_rgba_rounds_clamps_and_drops_alpha() {
    // 0.5 * 255 = 127.5 rounds up, not down
    assert_eq!(
        Rgb::from_rgba(&gdk::RGBA::new(0.5, 0.5, 0.5, 1.0)),
        Rgb(128, 128, 128)
    );
    // just under a half stays down
    assert_eq!(
        Rgb::from_rgba(&gdk::RGBA::new(0.498, 0.0, 0.0, 1.0)),
        Rgb(127, 0, 0)
    );
    // out of range values clamp instead of wrapping
    assert_eq!(
        Rgb::from_rgba(&gdk::RGBA::new(1.5, -0.5, 2.0, 1.0)),
        Rgb(255, 0, 255)
    );
    // the alpha does not change the colour
    assert_eq!(
        Rgb::from_rgba(&gdk::RGBA::new(1.0, 0.0, 0.0, 0.2)),
        Rgb(255, 0, 0)
    );
}

#[test]
fn text_is_dark_on_light_and_light_on_dark() {
    for light in [
        Rgb(255, 255, 255),
        Rgb(0xf4, 0xe8, 0xc1),
        Rgb(0xcc, 0xdd, 0xff),
    ] {
        assert_eq!(light.text(), DARK_TEXT, "{light:?}");
    }
    for dark in [Rgb(0, 0, 0), Rgb(0x2b, 0x1d, 0x4e), Rgb(0x10, 0x30, 0x60)] {
        assert_eq!(dark.text(), LIGHT_TEXT, "{dark:?}");
    }
}

#[test]
fn text_never_has_less_contrast_than_the_other_choice() {
    for g in 0..=255u8 {
        for bg in [Rgb(g, g, g), Rgb(g, 0, 255 - g), Rgb(255 - g, g, 0)] {
            let (chosen, other) = if bg.text() == DARK_TEXT {
                (DARK_TEXT, LIGHT_TEXT)
            } else {
                (LIGHT_TEXT, DARK_TEXT)
            };
            assert!(bg.contrast(chosen) >= bg.contrast(other), "{bg:?}");
        }
    }
}

#[test]
fn text_meets_wcag_aa_on_every_colour() {
    // Black and white text: the worst background (luminance near 0.18)
    // still gets about 4.58, above WCAG AA's 4.5.
    let steps = (0..=255u8).step_by(5).chain([255]);
    for r in steps.clone() {
        for g in steps.clone() {
            for b in steps.clone() {
                let bg = Rgb(r, g, b);
                let ratio = bg.contrast(bg.text());
                assert!(ratio >= 4.5, "{bg:?}: {ratio}");
            }
        }
    }
    for g in 0..=255u8 {
        let bg = Rgb(g, g, g);
        assert!(bg.contrast(bg.text()) >= 4.5, "{bg:?}");
    }
}

#[test]
fn mix_returns_each_end_at_zero_and_one() {
    let (a, b) = (Rgb(10, 200, 33), Rgb(250, 0, 128));
    assert_eq!(a.mix(b, 0.0), a);
    assert_eq!(a.mix(b, 1.0), b);
    assert_eq!(b.mix(a, 0.0), b);
    assert_eq!(b.mix(a, 1.0), a);
}

#[test]
fn mix_moves_each_channel_on_its_own_and_in_either_direction() {
    let black = Rgb(0, 0, 0);
    let white = Rgb(255, 255, 255);
    // 255 * 0.2 = 51 exactly; a decreasing channel moves down
    assert_eq!(black.mix(white, 0.2), Rgb(51, 51, 51));
    assert_eq!(white.mix(black, 0.2), Rgb(204, 204, 204));
    assert_eq!(
        Rgb(0, 100, 255).mix(Rgb(100, 0, 255), 0.5),
        Rgb(50, 50, 255)
    );
}

#[test]
fn mix_rounds_to_the_nearest_channel_value() {
    // the midpoint of 0 and 255 is 127.5, which rounds up
    assert_eq!(
        Rgb(0, 0, 0).mix(Rgb(255, 255, 255), 0.5),
        Rgb(128, 128, 128)
    );
    assert_eq!(
        Rgb(255, 255, 255).mix(Rgb(0, 0, 0), 0.5),
        Rgb(128, 128, 128)
    );
    // 10 * 0.14 = 1.4 down, 10 * 0.16 = 1.6 up
    assert_eq!(Rgb(0, 0, 0).mix(Rgb(10, 10, 10), 0.14), Rgb(1, 1, 1));
    assert_eq!(Rgb(0, 0, 0).mix(Rgb(10, 10, 10), 0.16), Rgb(2, 2, 2));
}

#[test]
fn contrast_is_symmetric_and_spans_one_to_twenty_one() {
    let (b, w) = (Rgb(0, 0, 0), Rgb(255, 255, 255));
    assert!((b.contrast(w) - 21.0).abs() < 1e-9);
    assert!((w.contrast(b) - 21.0).abs() < 1e-9);
    assert!((w.contrast(w) - 1.0).abs() < 1e-9);
}

// ------------------------------------------------------------ stylesheet

#[test]
fn a_picked_background_is_defined_ahead_of_the_rules() {
    let bg = Rgb(0x2b, 0x1d, 0x4e);
    let css = stylesheet(Some(bg));
    let bg_def = format!("@define-color ip_bg {};", bg.hex());
    let fg_def = format!("@define-color ip_fg {};", bg.text().hex());
    assert!(css.starts_with(&bg_def), "{}", &css[..80]);
    let fg_at = css.find(&fg_def).expect("ip_fg is defined");
    assert!(fg_at > 0);
    // both come before the first rule
    let first_rule = css.find("window.background").expect("rules are present");
    assert!(fg_at < first_rule);
    assert_eq!(css.matches("@define-color ip_bg ").count(), 1);
    assert_eq!(css.matches("@define-color ip_fg ").count(), 1);
    // and the theme palette is not also loaded
    assert!(!css.contains(THEME_PALETTE));
    assert!(css.contains(PICKED_PALETTE));
    assert!(css.ends_with(RULES));
}

#[test]
fn the_theme_stylesheet_has_no_picked_colour() {
    let css = stylesheet(None);
    assert!(!css.contains("@define-color ip_bg "));
    assert!(css.contains(THEME_PALETTE));
    assert!(!css.contains(PICKED_PALETTE));
    assert!(css.ends_with(RULES));
}

#[test]
fn light_and_dark_picks_get_different_text() {
    let light = stylesheet(Some(Rgb(255, 255, 255)));
    let dark = stylesheet(Some(Rgb(0, 0, 0)));
    assert!(light.contains(&format!("ip_fg {};", DARK_TEXT.hex())));
    assert!(dark.contains(&format!("ip_fg {};", LIGHT_TEXT.hex())));
}

// ------------------------------------------------------------- stripes

/// Backgrounds to try: a 16-step cube of colours, and every grey.
fn stripe_backgrounds() -> (Vec<Rgb>, Vec<Rgb>) {
    let steps: Vec<u8> = (0..=255u8).step_by(17).collect();
    let mut cube = Vec::new();
    for &r in &steps {
        for &g in &steps {
            for &b in &steps {
                cube.push(Rgb(r, g, b));
            }
        }
    }
    let greys: Vec<Rgb> = (0..=255u8).map(|g| Rgb(g, g, g)).collect();
    (cube, greys)
}

/// The text and away colours `stylesheet` hands to `stripes`.
fn stripes_for(bg: Rgb) -> (Rgb, Rgb, Rgb, Rgb) {
    let text = bg.text();
    let away = if text == DARK_TEXT {
        LIGHT_TEXT
    } else {
        DARK_TEXT
    };
    let (stripe, hover) = stripes(bg, text, away);
    (text, away, stripe, hover)
}

// Measured on the grid below: the lowest stripe/background ratio is 1.058
// and the lowest hover/background 1.041, both on colours; on greys 1.24 and
// 1.14. The floors sit just under those.
#[test]
fn stripe_and_hover_are_visible_and_distinct_on_every_background() {
    let (cube, greys) = stripe_backgrounds();
    for (bg, stripe_floor, hover_floor) in cube
        .iter()
        .map(|&bg| (bg, 1.05, 1.04))
        .chain(greys.iter().map(|&bg| (bg, 1.2, 1.1)))
    {
        let (_, _, stripe, hover) = stripes_for(bg);
        let (s, h) = (bg.contrast(stripe), bg.contrast(hover));
        assert!(s >= stripe_floor, "{bg:?}: stripe {stripe:?} only {s}");
        assert!(h >= hover_floor, "{bg:?}: hover {hover:?} only {h}");
        assert!(
            stripe.contrast(hover) >= 1.05,
            "{bg:?}: hover {hover:?} too close to stripe {stripe:?}"
        );
        assert_ne!(hover, bg, "{bg:?}");
        assert_ne!(hover, stripe, "{bg:?}");
    }
}

#[test]
fn stripe_and_hover_keep_aa_text_contrast_on_every_background() {
    let (cube, greys) = stripe_backgrounds();
    for bg in cube.into_iter().chain(greys) {
        let (text, _, stripe, hover) = stripes_for(bg);
        assert!(text.contrast(stripe) >= 4.5, "{bg:?}: stripe {stripe:?}");
        assert!(text.contrast(hover) >= 4.5, "{bg:?}: hover {hover:?}");
    }
}

// The stripe used to be mix(bg, away, 0.07): on a pale pick `away` is white,
// so the stripe was invisible. A pale pick needs a darker stripe.
#[test]
fn a_pale_pick_gets_a_darker_stripe_and_black_a_lighter_one() {
    for pale in [
        Rgb(0xf3, 0xec, 0xd8),
        Rgb(255, 255, 255),
        Rgb(0xf4, 0xe8, 0xc1),
    ] {
        let (_, _, stripe, hover) = stripes_for(pale);
        assert!(
            stripe.contrast(Rgb(0, 0, 0)) < pale.contrast(Rgb(0, 0, 0)),
            "{pale:?}: {stripe:?}"
        );
        assert!(
            stripe.0 < pale.0 && stripe.1 < pale.1 && stripe.2 < pale.2,
            "{pale:?}: {stripe:?}"
        );
        assert!(hover.0 < pale.0, "{pale:?}: hover {hover:?}");
    }
    let (_, _, stripe, hover) = stripes_for(Rgb(0, 0, 0));
    assert!(stripe.0 > 0 && stripe.1 > 0 && stripe.2 > 0, "{stripe:?}");
    assert!(hover.0 > stripe.0, "hover {hover:?} vs stripe {stripe:?}");
}

// Away from the text is the default: it can only add text contrast. Grey
// 100 takes white text, so its stripe is darker, one step of 0.15, and the
// hover two steps.
#[test]
fn a_mid_tone_stripe_moves_away_from_the_text() {
    let bg = Rgb(100, 100, 100);
    let (text, away, stripe, hover) = stripes_for(bg);
    assert_eq!((text, away), (LIGHT_TEXT, DARK_TEXT));
    assert_eq!(stripe, bg.mix(away, STRIPE_STEP));
    assert_eq!(hover, bg.mix(away, 2.0 * STRIPE_STEP));
}

// Grey 94 takes white text, but a step towards white is a bigger jump than
// one towards black and still reads at AA, so the stripe goes lighter.
// Two steps that way would drop under 4.5, so the hover goes two steps the
// other way, towards black.
#[test]
fn hover_falls_back_to_the_other_way_when_two_steps_break_aa() {
    let bg = Rgb(94, 94, 94);
    let (text, away, stripe, hover) = stripes_for(bg);
    assert_eq!(text, LIGHT_TEXT);
    assert_eq!(stripe, bg.mix(text, STRIPE_STEP));
    // the premise: the doubled step towards the text would break AA
    assert!(text.contrast(bg.mix(text, 2.0 * STRIPE_STEP)) < 4.5);
    assert_eq!(hover, bg.mix(away, 2.0 * STRIPE_STEP));
    assert!(text.contrast(hover) >= 4.5);
}

// The sidebar and title bar are mix(bg, away, 0.1) in picked.css and take
// @ip_hover on their rows and column titles. A hover that fell back only one
// step towards `away` sat next to them, about 1.05 on saturated blues and
// greens. Measured minimum on this grid: 1.13.
#[test]
fn hover_shows_on_the_sidebar_and_title_bar() {
    let (cube, greys) = stripe_backgrounds();
    for bg in cube.into_iter().chain(greys) {
        let (_, away, _, hover) = stripes_for(bg);
        let sidebar = bg.mix(away, 0.1);
        let c = hover.contrast(sidebar);
        assert!(
            c >= 1.1,
            "{bg:?}: hover {hover:?} on sidebar {sidebar:?} only {c}"
        );
    }
}

#[test]
fn a_picked_stylesheet_defines_the_stripe_and_hover_once_ahead_of_the_rules() {
    for bg in [
        Rgb(0x2b, 0x1d, 0x4e),
        Rgb(0xf3, 0xec, 0xd8),
        Rgb(255, 255, 255),
        Rgb(0, 0, 0),
        Rgb(0x75, 0x75, 0x75),
    ] {
        let css = stylesheet(Some(bg));
        let (_, _, stripe, hover) = stripes_for(bg);
        let first_rule = css.find("window.background").expect("rules are present");
        for (name, value) in [("ip_grid_odd", stripe), ("ip_hover", hover)] {
            let def = format!("@define-color {name} {};", value.hex());
            assert_eq!(
                css.matches(&format!("@define-color {name} ")).count(),
                1,
                "{bg:?} {name}"
            );
            let at = css
                .find(&def)
                .unwrap_or_else(|| panic!("{bg:?}: {def} missing"));
            assert!(at < first_rule, "{bg:?}: {name} comes after the rules");
        }
    }
}

// The theme stylesheet defines the stripe in palette.css, once.
#[test]
fn the_theme_stylesheet_defines_the_stripe_and_hover_once() {
    let css = stylesheet(None);
    assert_eq!(css.matches("@define-color ip_grid_odd ").count(), 1);
    assert_eq!(css.matches("@define-color ip_hover ").count(), 1);
}

// Prevents: the header badge's rule being dropped from the sheet. The test
// above only says the sheet parses; the colour on a real label is checked in
// close_tests, which needs a display.
#[test]
fn the_sheet_has_the_orange_badge_rule() {
    for background in [
        None,
        Some(Rgb(0x2b, 0x1d, 0x4e)),
        Some(Rgb(0xf4, 0xe8, 0xc1)),
    ] {
        let sheet = stylesheet(background);
        let at = sheet
            .find("headerbar .app-mark {")
            .unwrap_or_else(|| panic!("{background:?}: no `headerbar .app-mark` rule"));
        let rule = &sheet[at..];
        let rule = &rule[..rule.find('}').expect("rule is closed")];
        assert!(rule.contains("color: #f57900;"), "{background:?}: {rule}");
    }
}
