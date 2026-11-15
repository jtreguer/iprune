//! `Autotag::killer` (what the window's Stop uses on a lookup that does not
//! end by itself) against the fake autotag, whose `hang` quirk never answers
//! (it gives up and exits after 5 s, so a kill that does nothing is a failure
//! here and not a hung test run). No network and no real autotag.

mod common;

use std::time::{Duration, Instant};

use common::import_env::{Imp, fake_pids, process_exists, requests, scenario};
use iprune_core::Error;
use serde_json::json;

/// Gone, or a zombie waiting to be reaped.
fn dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat[stat.rfind(')').unwrap() + 2..].starts_with('Z'),
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn hanging(imp: &Imp, wrapper: bool) {
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["quirks"] = json!({"lookup:a.mp3": "hang"});
    if wrapper {
        sc["wrapper"] = json!(true);
    }
    imp.scenario(sc);
}

// Prevents Stop from waiting for a lookup that never answers: killing from
// another thread must fail the request at once, not after the process dies
// of its own accord.
#[test]
fn killing_from_another_thread_fails_the_hung_request_with_an_autotag_error() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    hanging(&imp, false);
    let mut autotag = imp.start().unwrap();
    let killer = autotag.killer();
    let (pid, pgid, _) = fake_pids(imp.music());
    let root = imp.music().to_owned();

    let (result, waited) = std::thread::scope(|s| {
        let kill = s.spawn(|| {
            wait_until("the request to reach autotag", || {
                requests(&root).iter().any(|r| r["op"] == "lookup")
            });
            let at = Instant::now();
            killer.kill();
            at
        });
        let result = autotag.lookup(&file, None);
        (result, kill.join().unwrap().elapsed())
    });

    match result {
        Err(Error::Autotag(_)) => {}
        other => panic!("expected Error::Autotag, got {other:?}"),
    }
    assert!(
        waited < Duration::from_secs(3),
        "the request failed {waited:?} after the kill: it waited for the fake's own exit"
    );
    assert_eq!(pgid, pid, "autotag leads its own process group");
    wait_until("autotag to die", || dead(pid));
    drop(autotag);
    assert!(!process_exists(pid), "reaped by the drop");
}

// `uv run` keeps python as a child of the process iprune started: Stop must
// end both, not only the one it knows.
#[test]
fn killing_ends_the_whole_group_when_autotag_is_a_wrapper_around_python() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    hanging(&imp, true);
    let mut autotag = imp.start().unwrap();
    let killer = autotag.killer();
    let (inner, _, parent) = fake_pids(imp.music());
    let root = imp.music().to_owned();
    let wrapper: Vec<u32> = std::fs::read_to_string(
        common::import_env::scenario_dir(imp.music()).join("fake-autotag.wrapper.pid"),
    )
    .unwrap()
    .split_whitespace()
    .map(|n| n.parse().unwrap())
    .collect();
    assert_eq!(parent, wrapper[0], "python is the wrapper's child");

    let killed = std::thread::scope(|s| {
        let kill = s.spawn(|| {
            wait_until("the request to reach autotag", || {
                requests(&root).iter().any(|r| r["op"] == "lookup")
            });
            killer.kill();
            Instant::now()
        });
        assert!(matches!(
            autotag.lookup(&file, None),
            Err(Error::Autotag(_))
        ));
        kill.join().unwrap()
    });
    assert!(
        killed.elapsed() < Duration::from_secs(3),
        "waited for the fake's own exit"
    );
    // Before the drop, which would end the group itself after its grace.
    wait_until("python to die", || dead(inner));
    wait_until("the wrapper to die", || dead(wrapper[0]));
    drop(autotag);
}

#[test]
fn a_killer_for_a_process_that_already_ended_is_harmless() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["quirks"] = json!({"1": "exit"});
    imp.scenario(sc);
    let mut autotag = imp.start().unwrap();
    let killer = autotag.killer();
    assert!(matches!(
        autotag.lookup(&file, None),
        Err(Error::Autotag(_))
    ));
    let (pid, _, _) = fake_pids(imp.music());
    wait_until("autotag to end", || dead(pid));

    // Twice, and on a clone: nothing happens, and we are still here.
    killer.kill();
    killer.clone().kill();
    drop(autotag);
}

#[test]
fn a_killer_that_is_not_used_leaves_autotag_working() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));
    let mut autotag = imp.start().unwrap();
    let _killer = autotag.killer();
    let (lookup, _) = autotag.lookup(&file, None).unwrap().unwrap();
    assert_eq!(lookup.candidates.len(), 1);
}

// -- A killer that outlives its process ---------------------------------------

// Once the process is reaped its id may name another one: the killer must
// have forgotten it. The pid cannot be made to be reused here, so this reads
// it from the killer's Debug text.
#[test]
fn a_killer_kept_after_the_drop_has_forgotten_the_process() {
    let imp = Imp::new();
    imp.scenario(scenario(&[]));
    let autotag = imp.start().unwrap();
    let killer = autotag.killer();
    let (pid, _, _) = fake_pids(imp.music());
    assert!(
        format!("{killer:?}").contains(&pid.to_string()),
        "{killer:?}"
    );
    drop(autotag);
    assert!(!format!("{killer:?}").contains("Some"), "{killer:?}");
    killer.kill();
    killer.clone().kill();
}

#[test]
fn a_killer_forgets_a_process_whose_death_was_read_by_a_failed_request() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["quirks"] = json!({"1": "exit"});
    imp.scenario(sc);
    let mut autotag = imp.start().unwrap();
    let killer = autotag.killer();
    assert!(matches!(
        autotag.lookup(&file, None),
        Err(Error::Autotag(_))
    ));
    // The failure reaped it, with the Autotag still alive.
    assert!(!format!("{killer:?}").contains("Some"), "{killer:?}");
    killer.kill();
    drop(autotag);
}

// The drop's own kill (autotag that will not end) and killers on other
// threads share one lock: neither deadlocks, and the process is gone.
#[test]
fn killers_hammering_while_the_drop_kills_a_stuck_autotag_do_not_deadlock() {
    let imp = Imp::new();
    let mut sc = scenario(&[]);
    sc["hang_on_eof"] = json!(true);
    imp.scenario(sc);
    let autotag = imp.start().unwrap();
    let (pid, _, _) = fake_pids(imp.music());
    let killers: Vec<_> = (0..4).map(|_| autotag.killer()).collect();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        for k in &killers {
            s.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    k.kill();
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        }
        // Either a killer or the drop (after its grace) ends it.
        let began = Instant::now();
        drop(autotag);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(began.elapsed() < Duration::from_secs(8));
    });
    assert!(!process_exists(pid), "reaped");
}

// Nobody kills it: the drop does, after its grace, and takes the process id
// out of the killers' hands as it reaps.
#[test]
fn the_drops_own_kill_of_a_stuck_autotag_leaves_the_killers_empty() {
    let imp = Imp::new();
    let mut sc = scenario(&[]);
    sc["hang_on_eof"] = json!(true);
    imp.scenario(sc);
    let autotag = imp.start().unwrap();
    let (pid, _, _) = fake_pids(imp.music());
    let killer = autotag.killer();
    assert!(format!("{killer:?}").contains("Some"), "{killer:?}");
    drop(autotag);
    assert!(!process_exists(pid), "killed and reaped by the drop");
    assert!(!format!("{killer:?}").contains("Some"), "{killer:?}");
}
