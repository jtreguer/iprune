//! `payload` must be byte-identical to what libgpod's `sync_time` builds
//! (`tests/c/ipod_time.c` holds its struct and fill logic, run with the
//! same `TZ`), across zones, signs, half hours, daylight saving and the
//! edges of a year. Needs gcc and the zoneinfo files. Nothing is sent to any
//! device.

use std::path::Path;
use std::process::Command;

use iprune_clock::{LocalTime, payload};
use jiff::Timestamp;
use jiff::civil::date;
use jiff::tz::TimeZone;

fn compile(out: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/c/ipod_time.c");
    let status = Command::new("gcc")
        .args(["-Wall", "-Werror", "-o"])
        .arg(out)
        .arg(&src)
        .status()
        .expect("gcc");
    assert!(status.success(), "compiling {}", src.display());
}

fn c_bytes(bin: &Path, zone: &str, epoch: i64) -> String {
    let out = Command::new(bin)
        .arg(epoch.to_string())
        .env_clear()
        .env("TZ", zone)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{zone} {epoch}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn rust_bytes(zone: &str, epoch: i64) -> String {
    let t = Timestamp::from_second(epoch)
        .unwrap()
        .to_zoned_datetime(TimeZone::get(zone).unwrap());
    payload(&LocalTime::of(&t))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The instant of a civil time in `zone`.
fn at(zone: &str, y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> i64 {
    date(y, mo, d)
        .at(h, mi, s, 0)
        .to_zoned_datetime(TimeZone::get(zone).unwrap())
        .unwrap()
        .timestamp()
        .as_second()
}

fn utc(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> i64 {
    at("UTC", y, mo, d, h, mi, s)
}

#[test]
fn payload_is_byte_identical_to_libgpods_across_zones_and_edges() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("ipod_time");
    compile(&bin);

    let cases: Vec<(&str, &str, i64)> = vec![
        (
            "Paris winter",
            "Europe/Paris",
            at("Europe/Paris", 2026, 11, 15, 23, 41, 7),
        ),
        (
            "Paris summer",
            "Europe/Paris",
            at("Europe/Paris", 2026, 7, 1, 12, 0, 0),
        ),
        (
            "St John's winter",
            "America/St_Johns",
            at("America/St_Johns", 2026, 1, 20, 8, 30, 0),
        ),
        (
            "St John's summer",
            "America/St_Johns",
            at("America/St_Johns", 2026, 7, 20, 8, 30, 0),
        ),
        (
            "Kathmandu +5:45",
            "Asia/Kathmandu",
            at("Asia/Kathmandu", 2026, 5, 5, 5, 5, 5),
        ),
        ("UTC", "UTC", utc(2026, 11, 15, 23, 41, 7)),
        (
            "Lord Howe summer",
            "Australia/Lord_Howe",
            at("Australia/Lord_Howe", 2026, 1, 15, 12, 0, 0),
        ),
        (
            "Lord Howe winter",
            "Australia/Lord_Howe",
            at("Australia/Lord_Howe", 2026, 7, 1, 12, 0, 0),
        ),
        (
            "Los Angeles winter",
            "America/Los_Angeles",
            at("America/Los_Angeles", 2026, 12, 25, 18, 0, 0),
        ),
        ("Tokyo", "Asia/Tokyo", at("Asia/Tokyo", 2026, 2, 3, 4, 5, 6)),
        (
            "Dublin winter",
            "Europe/Dublin",
            at("Europe/Dublin", 2026, 1, 10, 10, 0, 0),
        ),
        (
            "Dublin summer",
            "Europe/Dublin",
            at("Europe/Dublin", 2026, 7, 10, 10, 0, 0),
        ),
        // The first and last seconds of years, in zones either side of UTC.
        (
            "1 January 00:00:00 Paris",
            "Europe/Paris",
            at("Europe/Paris", 2026, 1, 1, 0, 0, 0),
        ),
        (
            "1 January 00:00:00 Los Angeles",
            "America/Los_Angeles",
            at("America/Los_Angeles", 2027, 1, 1, 0, 0, 0),
        ),
        (
            "31 December leap year",
            "UTC",
            utc(2028, 12, 31, 23, 59, 59),
        ),
        (
            "31 December leap year, Tokyo",
            "Asia/Tokyo",
            at("Asia/Tokyo", 2028, 12, 31, 23, 59, 59),
        ),
        (
            "31 December common year",
            "UTC",
            utc(2027, 12, 31, 23, 59, 59),
        ),
        (
            "29 February",
            "Europe/Paris",
            at("Europe/Paris", 2028, 2, 29, 12, 0, 0),
        ),
        (
            "1 March after a leap day",
            "Europe/Paris",
            at("Europe/Paris", 2028, 3, 1, 12, 0, 0),
        ),
        (
            "1 March, common year",
            "Europe/Paris",
            at("Europe/Paris", 2027, 3, 1, 12, 0, 0),
        ),
        // Spring forward in Paris, 29 March 2026: 01:00:00 UTC is 03:00:00.
        (
            "Paris, last second before spring forward",
            "Europe/Paris",
            utc(2026, 3, 29, 0, 59, 59),
        ),
        (
            "Paris, first second after spring forward",
            "Europe/Paris",
            utc(2026, 3, 29, 1, 0, 0),
        ),
        // Fall back, 25 October 2026: 01:00:00 UTC is 02:00:00 again.
        (
            "Paris, last second before fall back",
            "Europe/Paris",
            utc(2026, 10, 25, 0, 59, 59),
        ),
        (
            "Paris, first second after fall back",
            "Europe/Paris",
            utc(2026, 10, 25, 1, 0, 0),
        ),
        // Lord Howe's 30 minute shift: 2026-04-05 02:00 local (+11) goes to
        // 01:30 (+10:30), at 15:00:00 UTC on the 4th; and 2026-10-04 02:00
        // (+10:30) goes to 02:30 (+11), at 15:30:00 UTC on the 3rd.
        (
            "Lord Howe, before fall back",
            "Australia/Lord_Howe",
            utc(2026, 4, 4, 14, 59, 59),
        ),
        (
            "Lord Howe, after fall back",
            "Australia/Lord_Howe",
            utc(2026, 4, 4, 15, 0, 0),
        ),
        (
            "Lord Howe, before spring forward",
            "Australia/Lord_Howe",
            utc(2026, 10, 3, 15, 29, 59),
        ),
        (
            "Lord Howe, after spring forward",
            "Australia/Lord_Howe",
            utc(2026, 10, 3, 15, 30, 0),
        ),
    ];

    let mut bad = Vec::new();
    for (name, zone, epoch) in &cases {
        let (c, rust) = (c_bytes(&bin, zone, *epoch), rust_bytes(zone, *epoch));
        if c != rust {
            bad.push(format!("{name} ({zone}, {epoch}): C {c}, Rust {rust}"));
        }
    }
    assert!(bad.is_empty(), "differences:\n{}", bad.join("\n"));

    // Both sides agreeing on nothing, or on the wrong thing, would pass the
    // loop: pin the C side's bytes for two cases.
    let paris = at("Europe/Paris", 2026, 7, 1, 12, 0, 0);
    assert_eq!(
        c_bytes(&bin, "Europe/Paris", paris),
        "07ea00b5080c000001000000"
    );
    let johns = at("America/St_Johns", 2026, 1, 1, 0, 0, 0);
    assert_eq!(
        c_bytes(&bin, "America/St_Johns", johns),
        "07ea0000f200000000000000"
    );
}
