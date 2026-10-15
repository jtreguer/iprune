//! `closes` must not keep its dialog alive.
//!
//! The Sync and Adopt dialogs clear the window's `busy` flag in a `destroy`
//! handler. A Close button holding a strong reference to its own dialog made a
//! cycle, so `close()` never disposed the dialog and Eject stayed disabled.
//!
//! This needs a display, so it runs against a private `gtk4-broadwayd`. The
//! GTK work happens in a child process (this test binary run again), so the
//! shared test process never initialises GTK or changes its environment.
//! It is the only test in the crate that touches GTK.
//!
//! Set `IPRUNE_REQUIRE_GUI_TESTS` to fail instead of skip when
//! `gtk4-broadwayd` is not installed.

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;

const CHILD_MARKER: &str = "IPRUNE_BROADWAY_CHILD";
/// Printed by the child once its checks have passed.
const CHECKED: &str = "close_tests: checked";
const TEST_NAME: &str = "close_tests::a_closes_button_lets_the_dialog_be_destroyed";

/// A running broadway daemon. Dropping it stops the daemon.
struct Broadway {
    child: Child,
    /// Keeps reading the daemon's stdout until it exits, so it never gets
    /// SIGPIPE.
    reader: Option<JoinHandle<()>>,
    /// The daemon does not remove its socket when stopped; we do.
    socket: Option<PathBuf>,
}

impl Drop for Broadway {
    fn drop(&mut self) {
        stop(&mut self.child);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// SIGTERM first, so the daemon removes its socket, then kill.
fn stop(child: &mut Child) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

enum Start {
    Running(Broadway, u32),
    NotInstalled,
}

/// Starts broadwayd on a display number nobody else uses. Ready means it
/// printed "Listening on <socket>"; a daemon that exits or stays silent for
/// 5 s counts as a failed attempt and the next number is tried.
fn start_broadway() -> Start {
    let base = 200 + std::process::id() % 700;
    for n in base..base + 20 {
        let mut child = match Command::new("gtk4-broadwayd")
            .arg(format!(":{n}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Start::NotInstalled,
            Err(e) => panic!("could not start gtk4-broadwayd: {e}"),
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send(line);
            }
        });
        let mut daemon = Broadway {
            child,
            reader: Some(reader),
            socket: None,
        };
        if let Ok(line) = rx.recv_timeout(Duration::from_secs(5))
            && let Some(path) = line.strip_prefix("Listening on ")
        {
            daemon.socket = Some(PathBuf::from(path.trim()));
            return Start::Running(daemon, n);
        }
        // Dropping `daemon` stops it.
    }
    panic!("gtk4-broadwayd is installed but never reported \"Listening on\"");
}

fn pump() {
    let context = glib::MainContext::default();
    for _ in 0..50 {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The GTK work, run in the child process.
fn check_in_child() {
    gtk::init().expect("gtk::init against broadway");

    let destroyed = Rc::new(Cell::new(false));
    {
        let window = gtk::Window::new();
        let button = gtk::Button::with_label("Close");
        window.set_child(Some(&button));
        crate::closes(&button, &window);
        let seen = destroyed.clone();
        window.connect_destroy(move |_| seen.set(true));
        window.present();
        pump();
        assert!(!destroyed.get(), "destroyed before the click");

        button.emit_clicked();
        pump();
    } // our strong references end here
    pump();

    assert!(destroyed.get(), "the dialog was not destroyed after Close");
    println!("{CHECKED}");
}

fn drain<R: Read + Send + 'static>(from: R) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(from).read_to_string(&mut text);
        text
    })
}

// Prevents: a Close or Cancel button capturing its dialog strongly, so the
// dialog is never destroyed and the busy flag is never cleared.
#[test]
fn a_closes_button_lets_the_dialog_be_destroyed() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        check_in_child();
        return;
    }

    let (_daemon, display) = match start_broadway() {
        Start::Running(daemon, display) => (daemon, display),
        Start::NotInstalled => {
            assert!(
                std::env::var_os("IPRUNE_REQUIRE_GUI_TESTS").is_none(),
                "gtk4-broadwayd is not installed and IPRUNE_REQUIRE_GUI_TESTS is set"
            );
            eprintln!("skipped: gtk4-broadwayd is not installed");
            return;
        }
    };

    let mut child = Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .env("GDK_BACKEND", "broadway")
        .env("BROADWAY_DISPLAY", format!(":{display}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the test binary as a child");
    let out = drain(child.stdout.take().expect("piped"));
    let err = drain(child.stderr.take().expect("piped"));

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for child") {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();

    match status {
        // A TEST_NAME that no longer matches runs nothing and exits 0.
        Some(status) if status.success() => assert!(
            out.contains(CHECKED),
            "the child ran no check (is TEST_NAME still the test's path?)\n--- stdout\n{out}"
        ),
        Some(status) => {
            panic!("child test failed ({status})\n--- stdout\n{out}\n--- stderr\n{err}")
        }
        None => panic!("child test timed out\n--- stdout\n{out}\n--- stderr\n{err}"),
    }
}
