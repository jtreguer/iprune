//! `verify::pre_write`, `normalize` and `differences` on hand-built data.

use iprune_core::adopt::{IpodPlaylist, IpodTrack};
use iprune_core::ipod::{Contents, Track};
use iprune_core::tags::Tags;
use iprune_core::verify::{self, ExpectedPlaylist};

fn track(dbid: u64) -> Track {
    Track {
        adopt: IpodTrack {
            dbid,
            size: 1,
            duration_ms: 1000,
            title: None,
            artist: None,
            album: None,
            playlists: 0,
            playcount: 0,
            hash: None,
        },
        tags: Tags::default(),
        ipod_path: Some(":iPod_Control:Music:F00:A.mp3".into()),
        recent_plays: 0,
        recent_skips: 0,
        rating: 0,
        last_played: 0,
    }
}

fn pl(id: u64, name: &str, members: &[u64]) -> IpodPlaylist {
    IpodPlaylist {
        id,
        name: name.into(),
        members: members.to_vec(),
    }
}

fn ep(ipod_id: u64, name: &str, members: &[u64]) -> ExpectedPlaylist {
    ExpectedPlaylist {
        ipod_id,
        name: name.into(),
        members: members.to_vec(),
    }
}

/// Tracks 1..=3; playlists Road (100) [1, 2, 2], Mix (200) [3], Keep (300) [1].
fn before() -> Contents {
    Contents {
        name: Some("iPod".into()),
        tracks: vec![track(1), track(2), track(3)],
        playlists: vec![
            pl(100, "Road", &[1, 2, 2]),
            pl(200, "Mix", &[3]),
            pl(300, "Keep", &[1]),
        ],
        smart_playlists: vec!["Top".into()],
        ..Contents::default()
    }
}

/// What the managed playlists look like when nothing goes wrong.
fn managed() -> Vec<ExpectedPlaylist> {
    vec![ep(100, "Road", &[1, 2, 2]), ep(200, "Mix", &[3])]
}

fn check(after: &Contents) -> Vec<String> {
    verify::pre_write(&before(), after, &managed(), &[], 0)
}

fn only(problems: Vec<String>, fragment: &str) {
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains(fragment), "{problems:?}");
}

fn has(problems: Vec<String>, fragment: &str) {
    assert!(
        problems.iter().any(|m| m.contains(fragment)),
        "{problems:?}"
    );
}

#[test]
fn an_unchanged_database_passes() {
    assert_eq!(check(&before()), Vec::<String>::new());
}

#[test]
fn a_correct_change_passes() {
    let mut after = before();
    after.playlists[0] = pl(100, "Road 2", &[3, 3]);
    after.playlists[1] = pl(200, "Mix", &[]);
    after.playlists.push(pl(400, "New", &[1]));
    let managed = [
        ep(100, "Road 2", &[3, 3]),
        ep(200, "Mix", &[]),
        ep(400, "New", &[1]),
    ];
    assert_eq!(
        verify::pre_write(&before(), &after, &managed, &[], 1),
        Vec::<String>::new()
    );
}

#[test]
fn tracks_must_be_exactly_as_before() {
    let mut removed = before();
    removed.tracks.pop();
    let p = check(&removed);
    assert!(
        p.iter()
            .any(|m| m.contains("the tracks changed (3 before, 2 now)")),
        "{p:?}"
    );

    let mut added = before();
    added.tracks.push(track(4));
    only(check(&added), "the tracks changed (3 before, 4 now)");

    type Edit = (&'static str, fn(&mut Track));
    let edits: [Edit; 7] = [
        ("size", |t| t.adopt.size += 1),
        ("duration", |t| t.adopt.duration_ms += 1),
        ("title", |t| t.tags.title = Some("Changed".into())),
        ("rating", |t| t.rating = 80),
        ("last played", |t| t.last_played = 5),
        ("path", |t| t.ipod_path = None),
        ("recent plays", |t| t.recent_plays = 1),
    ];
    for (what, edit) in edits {
        let mut after = before();
        edit(&mut after.tracks[1]);
        let p = check(&after);
        assert_eq!(p.len(), 1, "{what}: {p:?}");
        assert!(p[0].contains("the tracks changed"), "{what}: {p:?}");
    }
}

#[test]
fn the_count_of_playlists_a_track_is_in_may_change() {
    // A SetMembers adding track 3 to Road changes track 3's playlist count
    // by design, and must not fail the check.
    let mut b = before();
    b.tracks[2].adopt.playlists = 1;
    let mut after = b.clone();
    after.tracks[2].adopt.playlists = 2;
    after.playlists[0].members = vec![1, 2, 2, 3];
    let managed = [ep(100, "Road", &[1, 2, 2, 3]), ep(200, "Mix", &[3])];
    assert_eq!(
        verify::pre_write(&b, &after, &managed, &[], 0),
        Vec::<String>::new()
    );
    // The same change to a tag still fails.
    after.tracks[2].tags.artist = Some("Other".into());
    let p = verify::pre_write(&b, &after, &managed, &[], 0);
    only(p, "the tracks changed");
    // And so does a size.
    after.tracks[2].tags.artist = None;
    after.tracks[2].adopt.size = 999;
    let p = verify::pre_write(&b, &after, &managed, &[], 0);
    only(p, "the tracks changed");
}

#[test]
fn the_master_playlists_members_must_be_unchanged() {
    let mut b = before();
    b.master_members = vec![1, 2, 3];
    let mut after = b.clone();
    assert!(verify::pre_write(&b, &after, &managed(), &[], 0).is_empty());
    after.master_members = vec![1, 2];
    only(
        verify::pre_write(&b, &after, &managed(), &[], 0),
        "master playlist's members",
    );
    after.master_members = vec![3, 2, 1];
    only(
        verify::pre_write(&b, &after, &managed(), &[], 0),
        "master playlist's members",
    );
}

#[test]
fn the_playlist_count_must_be_before_plus_created_minus_deleted() {
    // One more than expected, and it is a managed one so nothing else objects.
    let mut after = before();
    after.playlists.push(pl(400, "New", &[]));
    let mut m = managed();
    m.push(ep(400, "New", &[]));
    let p = verify::pre_write(&before(), &after, &m, &[], 0);
    only(p, "4 regular playlists, expected 3");
    assert!(verify::pre_write(&before(), &after, &m, &[], 1).is_empty());
    // Created two but only one appeared.
    let p = verify::pre_write(&before(), &after, &m, &[], 2);
    only(p, "4 regular playlists, expected 5");
}

#[test]
fn a_playlist_that_is_neither_old_nor_managed_is_refused() {
    let mut after = before();
    after.playlists.push(pl(400, "Ghost", &[1]));
    // The count allows one creation, so only the origin is wrong.
    let p = verify::pre_write(&before(), &after, &managed(), &[], 1);
    only(p, "\"Ghost\" appeared from nowhere");
}

#[test]
fn a_managed_playlist_must_exist() {
    let mut after = before();
    after.playlists.remove(1);
    has(check(&after), "\"Mix\" is missing");
}

#[test]
fn a_managed_playlist_must_have_the_expected_name() {
    let mut after = before();
    after.playlists[0].name = "Rod".into();
    only(check(&after), "\"Road\" is named \"Rod\"");
}

#[test]
fn a_managed_playlist_must_have_the_expected_members_in_order_with_repeats() {
    for members in [
        vec![2, 1, 2], // order
        vec![1, 2],    // a repeat lost
        vec![1, 2, 2, 2],
        vec![],
    ] {
        let mut after = before();
        after.playlists[0].members = members.clone();
        only(check(&after), "\"Road\" has");
    }
}

#[test]
fn a_playlist_member_must_be_a_track_of_the_database() {
    let mut after = before();
    after.playlists[2].members = vec![1, 77]; // unmanaged playlist Keep
    // Keep also differs from before, so two problems; the dangling one is named.
    let p = check(&after);
    assert!(
        p.iter()
            .any(|m| m.contains("\"Keep\" holds 000000000000004d")),
        "{p:?}"
    );
}

#[test]
fn an_unmanaged_playlist_must_be_unchanged() {
    let mut renamed = before();
    renamed.playlists[2].name = "Kept".into();
    only(check(&renamed), "unmanaged playlist \"Keep\" changed");

    let mut members = before();
    members.playlists[2].members = vec![1, 1];
    only(check(&members), "unmanaged playlist \"Keep\" changed");

    let mut gone = before();
    gone.playlists.remove(2);
    has(check(&gone), "unmanaged playlist \"Keep\" changed");
}

#[test]
fn a_deleted_playlist_must_be_gone_and_may_not_be_reported_as_changed() {
    let managed = [ep(100, "Road", &[1, 2, 2]), ep(200, "Mix", &[3])];
    let mut after = before();
    after.playlists.remove(2);
    assert!(verify::pre_write(&before(), &after, &managed, &[300], 0).is_empty());
    // Deleting it was asked for, but it is still there.
    let p = verify::pre_write(&before(), &before(), &managed, &[300], 0);
    assert!(
        p.iter()
            .any(|m| m.contains("000000000000012c was not deleted")),
        "{p:?}"
    );
}

#[test]
fn smart_playlists_and_the_master_must_be_unchanged() {
    let mut smart = before();
    smart.smart_playlists.push("Extra".into());
    only(check(&smart), "smart playlists changed");
    let mut smart = before();
    smart.smart_playlists.clear();
    only(check(&smart), "smart playlists changed");
    let mut master = before();
    master.name = Some("Renamed".into());
    only(check(&master), "iPod's name changed");
}

#[test]
fn several_problems_are_all_reported() {
    let mut after = before();
    after.playlists[0].name = "Rod".into();
    after.name = Some("Renamed".into());
    after.tracks[1].ipod_path = None;
    assert_eq!(check(&after).len(), 3, "{:?}", check(&after));
}

// ---- normalize and differences

const LINE: &str = "T 0000000000000001 :iPod_Control:Music:F00:A.mp3 | A | B | C | 10 | art 1 mhii 4242 | rating 0 plays 0";
const LINE_NO_MHII: &str =
    "T 0000000000000001 :iPod_Control:Music:F00:A.mp3 | A | B | C | 10 | art 1 | rating 0 plays 0";

#[test]
fn normalize_drops_only_the_artwork_link_value() {
    assert_eq!(verify::normalize(LINE), LINE_NO_MHII);
    let text = format!("tracks 1 playlists 1\n{LINE}\nP Road 0\n  0000000000000001");
    assert_eq!(
        verify::normalize(&text),
        format!("tracks 1 playlists 1\n{LINE_NO_MHII}\nP Road 0\n  0000000000000001")
    );
    // A link at the end of the line, with nothing after it.
    assert_eq!(verify::normalize("T x | art 1 mhii 9"), "T x | art 1");
    // Nothing to drop.
    assert_eq!(verify::normalize("P a 0\n  1"), "P a 0\n  1");
}

#[test]
fn differences_ignores_the_artwork_link_and_finds_real_changes() {
    let a = format!("head\n{LINE}\ntail");
    let b = a.replace("4242", "1");
    assert!(verify::differences(&a, &b, 20).is_empty());
    assert!(verify::differences(&a, &a, 20).is_empty());

    let c = a.replace("tail", "tale");
    let d = verify::differences(&a, &c, 20);
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(d[0].starts_with("line 3:"), "{d:?}");
    assert!(d[0].contains("-tail") && d[0].contains("+tale"), "{d:?}");
}

#[test]
fn differences_reports_missing_and_extra_lines() {
    let short = verify::differences("a\nb\nc", "a\nb", 20);
    assert_eq!(short.len(), 1, "{short:?}");
    assert!(
        short[0].starts_with("line 3:") && short[0].contains("-c"),
        "{short:?}"
    );
    let long = verify::differences("a", "a\nb", 20);
    assert_eq!(long.len(), 1, "{long:?}");
    assert!(
        long[0].starts_with("line 2:") && long[0].contains("+b"),
        "{long:?}"
    );
}

#[test]
fn differences_stops_at_the_limit() {
    let a = (0..50)
        .map(|i| format!("x{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let b = (0..50)
        .map(|i| format!("y{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(verify::differences(&a, &b, 20).len(), 20);
    assert_eq!(verify::differences(&a, &b, 3).len(), 3);
    assert_eq!(verify::differences(&a, &b, 100).len(), 50);
    assert!(verify::differences(&a, &b, 3)[2].starts_with("line 3:"));
}

#[test]
fn more_deletions_than_playlists_reports_a_problem_instead_of_panicking() {
    let p = verify::pre_write(&before(), &before(), &managed(), &[1, 2, 3, 4, 5], 0);
    assert!(!p.is_empty());
}
