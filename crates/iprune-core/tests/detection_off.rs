//! `IPRUNE_NO_DETECT` (set for everything cargo runs, `.cargo/config.toml`)
//! makes iPod detection refuse, so a test can never find the real iPod by
//! looking at the mounts. Where a test must see detection on, it runs in a
//! child process with the variable removed, and only calls `find_ipods`,
//! which reads /proc/self/mountinfo and checks for an iTunesDB file: it
//! never reads or writes an iPod.

use std::process::Command;

use iprune_core::Error;
use iprune_core::config::Config;
use iprune_core::device;

const VAR: &str = "IPRUNE_NO_DETECT";
/// Tells the child run of `child_probe` that it is one.
const CHILD: &str = "IPRUNE_DETECTION_CHILD";

/// Stops a test that would scan the mounts when the variable is not set
/// (cargo's config not applied): better a failure than a look at a real iPod.
fn require_detection_off() {
    assert!(
        std::env::var_os(VAR).is_some(),
        "{VAR} is not set: run through cargo, whose .cargo/config.toml sets it"
    );
}

#[test]
fn find_ipods_refuses_while_the_variable_is_set() {
    require_detection_off();
    let e = device::find_ipods().unwrap_err();
    assert!(matches!(e, Error::DetectionOff), "{e}");
}

#[test]
fn an_empty_mountpoint_is_refused_not_detected_and_a_set_one_is_used() {
    require_detection_off();
    let cfg = Config::default();
    assert!(cfg.device.mountpoint.as_os_str().is_empty());
    let e = device::mountpoint(&cfg).unwrap_err();
    assert!(matches!(e, Error::DetectionOff), "{e}");

    let mut cfg = Config::default();
    cfg.device.mountpoint = "/some/where/ipod".into();
    assert_eq!(
        device::mountpoint(&cfg).unwrap(),
        std::path::Path::new("/some/where/ipod")
    );
}

/// Run as a child of the test below, with the variable removed. On its own
/// (no marker) it does nothing.
#[test]
fn child_probe() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    assert!(std::env::var_os(VAR).is_none(), "the parent removes it");
    // Whatever the machine's mounts hold, it is not the refusal. The result
    // is not looked at further: no mount point is printed or used.
    let found = device::find_ipods();
    assert!(
        !matches!(found, Err(Error::DetectionOff)),
        "detection refused with {VAR} unset"
    );
    println!("probe-ran");
}

/// Run as a child with the variable set to the empty string.
#[test]
fn child_probe_empty() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    assert_eq!(
        std::env::var_os(VAR).as_deref(),
        Some(std::ffi::OsStr::new("")),
        "the parent sets it empty"
    );
    let found = device::find_ipods();
    assert!(
        !matches!(found, Err(Error::DetectionOff)),
        "an empty {VAR} refused detection"
    );
    println!("probe-ran");
}

#[test]
fn an_empty_variable_does_not_turn_detection_off() {
    let exe = std::env::current_exe().unwrap();
    let o = Command::new(&exe)
        .args([
            "--exact",
            "child_probe_empty",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env(VAR, "")
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{text}");
    assert!(text.contains("probe-ran"), "the probe ran: {text}");
}

#[test]
fn with_the_variable_removed_detection_is_not_refused() {
    let exe = std::env::current_exe().unwrap();
    let run = |remove: bool| {
        let mut c = Command::new(&exe);
        c.args(["--exact", "child_probe", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1");
        if remove {
            c.env_remove(VAR);
        } else {
            c.env(VAR, "1");
        }
        c.output().unwrap()
    };
    // Removed: the probe runs find_ipods and does not get the refusal.
    let o = run(true);
    let text =
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{text}");
    assert!(text.contains("probe-ran"), "the probe ran: {text}");
    // Set: the same probe fails, which shows it can tell the two apart.
    let o = run(false);
    assert!(!o.status.success(), "the probe must notice a variable set");
    let text =
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(text.contains("the parent removes it"), "{text}");
}
