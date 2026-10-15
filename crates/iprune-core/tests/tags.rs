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

// ---- multi-value text tags (audit M15)

fn mp3_tags(frames: &[Vec<u8>]) -> Tags {
    let d = tempfile::tempdir().unwrap();
    read_ok(&put(&d, "a.mp3", &mp3_with_id3v2(1, frames))).tags
}

// Prevents: ID3v2.4 frames holding several NUL-separated values reaching the
// iPod cut to their first value.
#[test]
fn nul_separated_values_of_every_text_tag_are_joined_with_a_slash() {
    let t = mp3_tags(&[
        id3v24_text("TIT2", 3, b"One\0Two"),
        id3v24_text("TPE1", 3, b"Artist A\0Artist B"),
        id3v24_text("TALB", 3, b"Album\0Second"),
        id3v24_text("TPE2", 3, b"Band\0Guest"),
        id3v24_text("TCOM", 3, b"Writer 1\0Writer 2\0Writer 3"),
        id3v24_text("TCON", 3, b"Rock\0Pop"),
    ]);
    assert_eq!(t.title.as_deref(), Some("One/Two"));
    assert_eq!(t.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(t.album.as_deref(), Some("Album/Second"));
    assert_eq!(t.album_artist.as_deref(), Some("Band/Guest"));
    assert_eq!(t.composer.as_deref(), Some("Writer 1/Writer 2/Writer 3"));
    assert_eq!(t.genre.as_deref(), Some("Rock/Pop"));
}

#[test]
fn a_single_value_is_unchanged_and_the_join_changes_the_tag_hash() {
    let one = mp3_tags(&[id3v24_text("TPE1", 3, b"Artist A")]);
    let two = mp3_tags(&[id3v24_text("TPE1", 3, b"Artist A\0Artist B")]);
    assert_eq!(one.artist.as_deref(), Some("Artist A"));
    assert_ne!(one.hash(), two.hash());
}

// Prevents: blanks and empty values leaking into the joined string ("A//B",
// "A/ ") or a blank-only tag counting as a value.
#[test]
fn values_are_trimmed_and_empty_ones_dropped() {
    let t = mp3_tags(&[
        id3v24_text("TPE1", 3, b"  Artist A \0\0 \0Artist B  "),
        id3v24_text("TALB", 3, b"  Padded  "),
        id3v24_text("TCOM", 3, b"   \0  "),
        id3v24_text("TCON", 3, b"\0Rock"),
    ]);
    assert_eq!(t.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(t.album.as_deref(), Some("Padded"), "stored trimmed");
    assert_eq!(t.composer, None, "only blanks is no value");
    assert_eq!(t.genre.as_deref(), Some("Rock"));
}

// A numeric genre of ID3v1 style still names the genre.
#[test]
fn a_numeric_genre_still_maps_to_its_name() {
    let t = mp3_tags(&[id3v24_text("TCON", 3, b"(17)")]);
    assert_eq!(t.genre.as_deref(), Some("Rock"));
}

// Vorbis comments (FLAC, Ogg) are not read at all: those files are
// unsupported by the iPod. An MP4 atom holding two values is the other
// multi-value case of a supported format.
#[test]
fn an_m4a_atom_with_two_values_is_joined() {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::prelude::{ItemKey, TagExt};
    use lofty::tag::{ItemValue, TagItem};
    let d = tempfile::tempdir().unwrap();
    let p = put(&d, "a.m4a", &fs::read(fixture(Kind::Aac, 1)).unwrap());
    let mut f = lofty::read_from_path(&p).unwrap();
    let tag = f.primary_tag_mut().expect("the fixture has an ilst");
    tag.remove_key(ItemKey::TrackArtist);
    for a in ["Artist A", "Artist B"] {
        tag.push(TagItem::new(
            ItemKey::TrackArtist,
            ItemValue::Text(a.to_owned()),
        ));
    }
    tag.save_to_path(&p, WriteOptions::default()).unwrap();
    let r = read_ok(&p);
    assert_eq!(r.info.kind, FileKind::Aac);
    assert_eq!(r.tags.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(r.tags.title.as_deref(), Some("Title1"));
}

// Prevents: lofty reading a genre "(17)Rock" as "Rock" twice and the join
// sending "Rock/Rock" to the iPod.
#[test]
fn a_numeric_genre_followed_by_its_name_is_not_repeated() {
    let t = mp3_tags(&[id3v24_text("TCON", 3, b"(17)Rock")]);
    assert_eq!(t.genre.as_deref(), Some("Rock"));
}

#[test]
fn two_numeric_genres_give_two_names() {
    let t = mp3_tags(&[id3v24_text("TCON", 3, b"(17)(13)")]);
    assert_eq!(t.genre.as_deref(), Some("Rock/Pop"));
}

#[test]
fn repeated_values_differing_only_in_case_are_kept_once_the_first_spelling_wins() {
    let t = mp3_tags(&[
        id3v24_text("TPE1", 3, b"A\0a\0B"),
        id3v24_text("TALB", 3, b"b\0B\0a"),
    ]);
    assert_eq!(t.artist.as_deref(), Some("A/B"));
    assert_eq!(t.album.as_deref(), Some("b/a"));
}
