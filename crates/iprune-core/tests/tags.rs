//! `tags::read` on fixtures built from scratch: ID3v2 tags assembled byte by
//! byte over ffmpeg-generated audio, and an AAC file patched to look FairPlay
//! protected. No real music is used.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Kind, fairplay_m4a, fixture, id3v24_text, mp3_with_id3v2};
use iprune_core::tags::{self, FileKind, FileTags, ReadError, Tags};

fn put(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn title(t: &str) -> Vec<u8> {
    id3v24_text("TIT2", 3, t.as_bytes())
}

/// A TDRC (recording date) frame lofty's default mode rejects and Relaxed skips.
fn bad_date() -> Vec<u8> {
    id3v24_text("TDRC", 3, b"notadate")
}

/// A TRCK frame in UTF-16 with a BOM that is neither FF FE nor FE FF. Even
/// Relaxed mode cannot parse the tag.
fn bad_bom_track() -> Vec<u8> {
    id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00])
}

fn read_ok(p: &std::path::Path) -> FileTags {
    tags::read(p).unwrap_or_else(|e| panic!("read failed: {e:?}"))
}

#[test]
fn clean_mp3_reports_no_tag_error_and_no_drm() {
    let d = tempfile::tempdir().unwrap();
    let r = read_ok(&put(&d, "a.mp3", &mp3_with_id3v2(1, &[title("Plain")])));
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert_eq!(r.tags.title.as_deref(), Some("Plain"));
    assert_eq!(r.tag_error, None);
    assert!(!r.drm);
}

// Prevents: a frame lofty's default mode rejects ("failed to parse Mpeg file")
// making a valid MP3 unreadable.
#[test]
fn bad_frame_is_skipped_and_the_other_tags_survive() {
    let d = tempfile::tempdir().unwrap();
    let bytes = mp3_with_id3v2(
        1,
        &[
            title("Survivor"),
            id3v24_text("TPE1", 3, b"Some Artist"),
            bad_date(),
        ],
    );
    let p = put(&d, "a.mp3", &bytes);

    // Guard: lofty's default mode really does reject this file, so the test
    // is about the Relaxed fallback and not about a fixture lofty accepts.
    assert!(
        lofty::probe::Probe::open(&p)
            .unwrap()
            .guess_file_type()
            .unwrap()
            .read()
            .is_err(),
        "fixture must fail in the default parsing mode"
    );

    let r = read_ok(&p);
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert_eq!(r.tags.title.as_deref(), Some("Survivor"));
    assert_eq!(r.tags.artist.as_deref(), Some("Some Artist"));
    assert_eq!(r.tags.year, None, "the rejected frame is dropped");
    assert_eq!(
        r.tag_error, None,
        "Relaxed parsing succeeded, tags are real"
    );
}

// Prevents: a tag that cannot be parsed at all hiding a playable file.
#[test]
fn unparseable_tag_gives_empty_tags_an_error_and_real_audio_info() {
    let d = tempfile::tempdir().unwrap();
    let p = put(
        &d,
        "a.mp3",
        &mp3_with_id3v2(1, &[title("Lost"), bad_bom_track()]),
    );
    let clean = read_ok(&put(&d, "clean.mp3", &mp3_with_id3v2(1, &[title("x")])));

    let r = read_ok(&p);
    assert_eq!(r.tags, Tags::default(), "nothing half-parsed is kept");
    let why = r.tag_error.expect("tag_error must be set");
    assert!(!why.is_empty());
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert!(
        r.info.duration_ms.is_some_and(|ms| ms > 900),
        "{:?}",
        r.info
    );
    assert_eq!(r.info, clean.info, "same audio, same properties");
    assert!(!r.drm);
}

#[test]
fn garbage_is_still_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let cases: [(&str, Vec<u8>); 5] = [
        ("text.mp3", b"this is not audio at all, just text".to_vec()),
        ("empty.mp3", Vec::new()),
        (
            "junk.m4a",
            b"\0\0\0\x18ftypM4A \0\0\0\0garbage after".to_vec(),
        ),
        // A valid-looking tag header followed by nothing usable.
        ("tagonly.mp3", {
            let mut v = mp3_with_id3v2(1, &[bad_bom_track()]);
            v.truncate(10 + 14);
            v
        }),
        (
            "noise.mp3",
            (0..4096u32).map(|i| (i * 7 + 13) as u8).collect(),
        ),
    ];
    for (name, bytes) in cases {
        let p = put(&d, name, &bytes);
        match tags::read(&p) {
            Err(ReadError::Unreadable(msg)) => assert!(!msg.is_empty(), "{name}"),
            other => panic!("{name}: expected Unreadable, got {other:?}"),
        }
    }
}

#[test]
fn fairplay_m4a_is_aac_and_flagged() {
    let d = tempfile::tempdir().unwrap();
    let plain = read_ok(&fixture(Kind::Aac, 1));
    let p = put(&d, "store.m4a", &fairplay_m4a(1));

    let r = read_ok(&p);
    assert_eq!(r.info.kind, FileKind::Aac);
    assert!(r.drm);
    assert_eq!(r.tag_error, None);
    assert_eq!(r.tags.title.as_deref(), Some("Title1"), "tags still read");
    assert!(
        r.info.duration_ms.is_some_and(|ms| ms > 900),
        "{:?}",
        r.info
    );
    assert!(!plain.drm, "an ordinary AAC file is not flagged");
    assert_eq!(plain.info.kind, FileKind::Aac);
}

#[test]
fn ordinary_and_unsupported_m4a_are_not_mistaken_for_fairplay() {
    assert!(!read_ok(&fixture(Kind::Alac, 1)).drm);
    assert_eq!(read_ok(&fixture(Kind::Alac, 1)).info.kind, FileKind::Alac);
    // Opus and FLAC in MP4 have no recognised codec and are not protected: they
    // must stay unsupported rather than be swept up by the FairPlay rule.
    for k in [Kind::OpusInM4a, Kind::FlacInM4a] {
        match tags::read(&fixture(k, 1)) {
            Err(ReadError::Unsupported(_)) => {}
            other => panic!("{k:?}: expected Unsupported, got {other:?}"),
        }
    }
}
