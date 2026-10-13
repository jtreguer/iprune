//! `device::firewire_guid`: read from SysInfoExtended or SysInfo, no database.

use std::fs;
use std::path::Path;

use iprune_core::device::firewire_guid;

fn ipod(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let dev = dir.path().join("iPod_Control/Device");
    fs::create_dir_all(&dev).unwrap();
    for (n, c) in files {
        fs::write(dev.join(n), c).unwrap();
    }
    dir
}

fn plist(guid: &str) -> String {
    format!(
        "<plist><dict><key>Other</key><string>zzz</string><key>FireWireGUID</key>\n<string>{guid}</string></dict></plist>"
    )
}

#[test]
fn sysinfo_gives_upper_case_hex_without_the_prefix() {
    let d = ipod(&[(
        "SysInfo",
        "ModelNumStr: xA623\nFirewireGuid: 0x000a27001234abcd\n",
    )]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A27001234ABCD"));
    let d = ipod(&[("SysInfo", "FirewireGuid: 000A27001234ABCD\n")]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A27001234ABCD"));
}

#[test]
fn sysinfo_extended_wins_over_sysinfo() {
    let d = ipod(&[
        ("SysInfoExtended", &plist("000A270000000001")),
        ("SysInfo", "FirewireGuid: 0x000A270000000002\n"),
    ]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A270000000001"));
}

#[test]
fn an_unusable_extended_value_falls_back_to_sysinfo() {
    let d = ipod(&[
        ("SysInfoExtended", &plist("not hex")),
        ("SysInfo", "FirewireGuid: 0x000A270000000002\n"),
    ]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A270000000002"));
}

#[test]
fn no_guid_is_none() {
    assert_eq!(firewire_guid(ipod(&[]).path()), None);
    assert_eq!(
        firewire_guid(ipod(&[("SysInfo", "FirewireGuid:\n")]).path()),
        None
    );
    assert_eq!(
        firewire_guid(ipod(&[("SysInfo", "FirewireGuid: 0xZZ\n")]).path()),
        None
    );
    assert_eq!(
        firewire_guid(ipod(&[("SysInfo", "Other: 0x12\n")]).path()),
        None
    );
    assert_eq!(firewire_guid(Path::new("/nonexistent/ipod")), None);
}
