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
        skips: 0,
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
    // A link right before the rating, as the snapshot always prints it.
    assert_eq!(
        verify::normalize("T x | art 1 mhii 9 | rating 0 plays 0"),
        "T x | art 1 | rating 0 plays 0"
    );
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

// ---- verify::check: tracks removed, added, copied again and retagged

use iprune_core::verify::Expect;
use std::collections::{HashMap, HashSet};

/// Tracks 1..=3, all in the master playlist; Road (100) [1, 2, 2], Mix (200)
/// [3], and Keep (300) [1, 3], which iprune does not manage.
fn full_before() -> Contents {
    let mut b = before();
    b.master_members = vec![1, 2, 3];
    b.playlists[2] = pl(300, "Keep", &[1, 3]);
    b
}

fn expect() -> Expect {
    Expect {
        managed: managed(),
        ..Expect::default()
    }
}

fn run(after: &Contents, e: &Expect) -> Vec<String> {
    verify::check(&full_before(), after, e, None)
}

#[test]
fn check_with_nothing_expected_passes_an_unchanged_database() {
    assert_eq!(run(&full_before(), &expect()), Vec::<String>::new());
}

#[test]
fn a_removed_track_must_leave_the_tracks_the_master_and_unmanaged_playlists() {
    // Track 3 goes. Mix is managed and says it is empty afterwards.
    let e = Expect {
        removed: HashSet::from([3]),
        managed: vec![ep(100, "Road", &[1, 2, 2]), ep(200, "Mix", &[])],
        ..Expect::default()
    };
    let mut after = full_before();
    after.tracks.pop();
    after.master_members = vec![1, 2];
    after.playlists[1].members = vec![];
    after.playlists[2].members = vec![1];
    assert_eq!(run(&after, &e), Vec::<String>::new());

    // Still in the tracks.
    let mut bad = after.clone();
    bad.tracks.push(track(3));
    has(run(&bad, &e), "the tracks changed (3 before, 3 now)");
    // Still in the master playlist.
    let mut bad = after.clone();
    bad.master_members = vec![1, 2, 3];
    has(run(&bad, &e), "master playlist's members");
    // Still in an unmanaged playlist (which would also dangle).
    let mut bad = after.clone();
    bad.playlists[2].members = vec![1, 3];
    has(run(&bad, &e), "unmanaged playlist \"Keep\" changed");
    // An unmanaged playlist that lost something else as well.
    let mut bad = after.clone();
    bad.playlists[2].members = vec![];
    has(run(&bad, &e), "unmanaged playlist \"Keep\" changed");
}

#[test]
fn a_track_that_vanishes_without_being_expected_to_is_a_problem() {
    let mut after = full_before();
    after.tracks.remove(1);
    after.master_members = vec![1, 3];
    after.playlists[0].members = vec![1];
    let p = run(&after, &expect());
    has(p.clone(), "the tracks changed (3 before, 2 now)");
    has(p, "master playlist's members");
}

#[test]
fn removing_a_track_lets_no_other_track_change() {
    let e = Expect {
        removed: HashSet::from([3]),
        managed: vec![ep(100, "Road", &[1, 2, 2]), ep(200, "Mix", &[])],
        ..Expect::default()
    };
    let mut after = full_before();
    after.tracks.pop();
    after.master_members = vec![1, 2];
    after.playlists[1].members = vec![];
    after.playlists[2].members = vec![1];
    after.tracks[0].adopt.size += 1;
    only(run(&after, &e), "track 0000000000000001 differs");
}

#[test]
fn an_added_track_goes_at_the_end_of_the_tracks_and_the_master_playlist() {
    let e = Expect {
        added: vec![7, 8],
        managed: vec![ep(100, "Road", &[1, 2, 2, 7]), ep(200, "Mix", &[3])],
        ..expect()
    };
    let mut after = full_before();
    after.tracks.extend([track(7), track(8)]);
    after.master_members = vec![1, 2, 3, 7, 8];
    after.playlists[0].members = vec![1, 2, 2, 7];
    assert_eq!(run(&after, &e), Vec::<String>::new());

    // Not at the end of the master playlist, or not in the same order.
    for wrong in [
        vec![7, 8, 1, 2, 3],
        vec![1, 2, 3, 8, 7],
        vec![1, 2, 3, 7],
        vec![1, 2, 3],
    ] {
        let mut bad = after.clone();
        bad.master_members = wrong.clone();
        has(run(&bad, &e), "master playlist's members");
    }
    // One of them missing from the tracks.
    let mut bad = after.clone();
    bad.tracks.pop();
    has(run(&bad, &e), "the tracks changed (3 before, 4 now)");
    // An extra track nobody asked for.
    let mut bad = after.clone();
    bad.tracks.push(track(9));
    has(run(&bad, &e), "1 unexpected");
    // Added tracks are in no unmanaged playlist.
    let mut bad = after.clone();
    bad.playlists[2].members = vec![1, 3, 7];
    has(run(&bad, &e), "unmanaged playlist \"Keep\" changed");
}

#[test]
fn an_added_track_without_a_file_is_a_problem_whether_or_files_are_checked() {
    let e = Expect {
        added: vec![7],
        managed: managed(),
        ..Expect::default()
    };
    let mut after = full_before();
    let mut t = track(7);
    t.ipod_path = None;
    after.tracks.push(t);
    after.master_members.push(7);
    only(run(&after, &e), "track 0000000000000007 has no file");
}

#[test]
fn the_file_of_an_added_or_recopied_track_must_exist_when_files_are_checked() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("iPod_Control/Music/F00")).unwrap();
    let e = Expect {
        added: vec![7],
        recopied: HashSet::from([2]),
        managed: managed(),
        ..Expect::default()
    };
    let mut after = full_before();
    after.tracks.push(track(7));
    after.master_members.push(7);
    after.tracks[1].ipod_path = Some(":iPod_Control:Music:F00:B.mp3".into());
    let files = |root: &std::path::Path| verify::check(&full_before(), &after, &e, Some(root));

    // Both files missing: both are named.
    let p = files(root.path());
    assert_eq!(p.len(), 2, "{p:?}");
    has(p.clone(), "the file of track 0000000000000007 is missing");
    has(p, "the file of track 0000000000000002 is missing");
    // Not looking at files, nothing to complain about.
    assert!(run(&after, &e).is_empty());
    // One present.
    std::fs::write(root.path().join("iPod_Control/Music/F00/A.mp3"), b"x").unwrap();
    only(files(root.path()), "track 0000000000000002");
    // Both present.
    std::fs::write(root.path().join("iPod_Control/Music/F00/B.mp3"), b"x").unwrap();
    assert_eq!(files(root.path()), Vec::<String>::new());
    // A directory is not a file.
    std::fs::remove_file(root.path().join("iPod_Control/Music/F00/B.mp3")).unwrap();
    std::fs::create_dir(root.path().join("iPod_Control/Music/F00/B.mp3")).unwrap();
    only(files(root.path()), "track 0000000000000002");
}

#[test]
fn a_path_that_leaves_the_ipod_never_counts_as_a_file() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside.mp3");
    std::fs::write(&outside, b"x").unwrap();
    let mount = root.path().join("ipod");
    std::fs::create_dir(&mount).unwrap();
    let e = Expect {
        added: vec![7],
        managed: managed(),
        ..Expect::default()
    };
    let mut after = full_before();
    let mut t = track(7);
    t.ipod_path = Some(":..:outside.mp3".into());
    after.tracks.push(t);
    after.master_members.push(7);
    let p = verify::check(&full_before(), &after, &e, Some(&mount));
    has(p, "the file of track 0000000000000007 is missing");
}

#[test]
fn a_recopied_track_may_change_its_file_and_size_but_not_lose_it() {
    let e = Expect {
        recopied: HashSet::from([2]),
        managed: managed(),
        ..Expect::default()
    };
    let mut after = full_before();
    after.tracks[1].adopt.size = 5000;
    after.tracks[1].adopt.duration_ms = 9000;
    after.tracks[1].ipod_path = Some(":iPod_Control:Music:F09:NEW.mp3".into());
    assert_eq!(run(&after, &e), Vec::<String>::new());
    // The same change without saying so is refused.
    only(run(&after, &expect()), "track 0000000000000002 differs");
    // A recopied track without a file, reported once.
    after.tracks[1].ipod_path = None;
    only(run(&after, &e), "track 0000000000000002 has no file");
    // A recopied track still on its old file: the file deleted after the
    // write would be its new audio.
    after.tracks[1].ipod_path = full_before().tracks[1].ipod_path.clone();
    only(
        run(&after, &e),
        "recopied track 0000000000000002 still uses its old file",
    );
    // Another track changing alongside is still caught.
    let mut after = full_before();
    after.tracks[1].adopt.size = 5000;
    after.tracks[1].ipod_path = Some(":iPod_Control:Music:F09:NEW.mp3".into());
    after.tracks[2].adopt.size = 5000;
    only(run(&after, &e), "track 0000000000000003 differs");
}

fn retag_expect(id: u64, tags: Tags) -> Expect {
    Expect {
        retagged: HashMap::from([(id, tags)]),
        managed: managed(),
        ..Expect::default()
    }
}

#[test]
fn a_retagged_track_must_have_exactly_the_tags_set_and_nothing_else_changed() {
    let want = Tags {
        title: Some("New".into()),
        year: Some(2001),
        ..Tags::default()
    };
    let e = retag_expect(2, want.clone());
    let mut after = full_before();
    after.tracks[1].tags = want.clone();
    after.tracks[1].adopt.title = want.title.clone();
    assert_eq!(run(&after, &e), Vec::<String>::new());

    // Tags not the ones set: the old ones, or one field off.
    let mut bad = after.clone();
    bad.tracks[1].tags = Tags::default();
    only(
        run(&bad, &e),
        "track 0000000000000002 does not have the tags set",
    );
    let mut bad = after.clone();
    bad.tracks[1].tags.year = Some(2002);
    only(run(&bad, &e), "does not have the tags set");
    // The right tags, but the file or the size moved too.
    let mut bad = after.clone();
    bad.tracks[1].adopt.size += 1;
    only(
        run(&bad, &e),
        "track 0000000000000002 changed more than its tags",
    );
    let mut bad = after.clone();
    bad.tracks[1].ipod_path = Some(":iPod_Control:Music:F00:OTHER.mp3".into());
    only(run(&bad, &e), "changed more than its tags");
    // Another track's tags changing is not allowed by this retag.
    let mut bad = after.clone();
    bad.tracks[0].tags.artist = Some("Sneaky".into());
    only(run(&bad, &e), "track 0000000000000001 differs");
}

#[test]
fn a_track_not_listed_as_retagged_may_not_change_its_tags() {
    let mut after = full_before();
    after.tracks[1].tags.title = Some("New".into());
    only(run(&after, &expect()), "track 0000000000000002 differs");
}

#[test]
fn playlists_created_deleted_and_track_changes_are_checked_together() {
    // Remove track 3 (in Mix and Keep), delete Mix, create Fresh with the
    // new track 7, all at once.
    let e = Expect {
        removed: HashSet::from([3]),
        added: vec![7],
        deleted: vec![200],
        created: 1,
        managed: vec![ep(100, "Road", &[1, 2, 2]), ep(400, "Fresh", &[7, 1])],
        ..Expect::default()
    };
    let mut after = full_before();
    after.tracks = vec![track(1), track(2), track(7)];
    after.master_members = vec![1, 2, 7];
    after.playlists = vec![
        pl(100, "Road", &[1, 2, 2]),
        pl(300, "Keep", &[1]),
        pl(400, "Fresh", &[7, 1]),
    ];
    assert_eq!(run(&after, &e), Vec::<String>::new());
    // Mix not deleted.
    let mut bad = after.clone();
    bad.playlists.push(pl(200, "Mix", &[]));
    let p = run(&bad, &e);
    has(p.clone(), "playlist 00000000000000c8 was not deleted");
    has(p, "regular playlists, expected 3");
}

// ---- normalize with " mhii " in a tag (audit L50)

#[test]
fn normalize_cuts_the_artwork_link_from_the_end_of_a_track_line() {
    let line = |title: &str, mhii: u32| {
        format!(
            "T 0000000000000001 :iPod_Control:Music:F00:A.mp3 | A | B | {title} | 10 | art 1 mhii {mhii} | rating 0 plays 0"
        )
    };
    let (a, b) = (line("Song mhii 77 x", 100), line("Song mhii 77 x", 101));
    assert_eq!(verify::normalize(&a), verify::normalize(&b));
    assert_eq!(
        verify::normalize(&a),
        "T 0000000000000001 :iPod_Control:Music:F00:A.mp3 | A | B | Song mhii 77 x | 10 | art 1 | rating 0 plays 0"
    );
    // The tag stays in the comparison: a different title is a difference.
    assert!(verify::differences(&a, &line("Song mhii 88 x", 100), 20).len() == 1);
    // A title that ends with the link text, and one that is only that text.
    let (c, d) = (line("a mhii 5", 1), line("a mhii 5", 2));
    assert_eq!(verify::normalize(&c), verify::normalize(&d));
    assert!(verify::differences(&line(" mhii 5", 1), &line(" mhii 5", 2), 20).is_empty());
    assert!(verify::differences(&c, &line("a mhii 6", 1), 20).len() == 1);
}

// After `without_artwork` a track line has no link field, and a title may
// hold " mhii 5 b": that text must stay in the comparison.
#[test]
fn normalize_does_not_eat_title_words_after_mhii_when_the_link_is_gone() {
    let line = |title: &str| {
        format!(
            "T 0000000000000001 :iPod_Control:Music:F00:A.mp3 | A | B | {title} | 10 | rating 0 plays 0"
        )
    };
    assert_ne!(
        verify::normalize(&line("x mhii 5 b")),
        verify::normalize(&line("x mhii 6 b"))
    );
    assert_eq!(
        verify::differences(&line("x mhii 5 b"), &line("x mhii 6 b"), 20).len(),
        1
    );
    assert_eq!(verify::normalize(&line("x mhii 5 b")), line("x mhii 5 b"));
    // Through without_artwork, from a line that has the real field too.
    let full = |title: &str, mhii: u32| {
        format!(
            "T 0000000000000001 :iPod_Control:Music:F00:A.mp3 | A | B | {title} | 10 | art 1 mhii {mhii} | rating 0 plays 0"
        )
    };
    let stripped = |t: &str| verify::without_artwork(&full(t, 7));
    assert_eq!(
        verify::differences(&stripped("x mhii 5 b"), &stripped("x mhii 6 b"), 20).len(),
        1
    );
    // A non-numeric word after " mhii " in the title is kept too.
    assert_eq!(
        verify::normalize(&full("x mhii abc", 7)),
        full("x mhii abc", 7).replace(" mhii 7", "")
    );
}

#[test]
fn normalize_leaves_playlist_lines_alone_even_with_mhii_in_the_name() {
    let text = "P Mix mhii 4 x 2\n  0000000000000001\nP a mhii 9 1";
    assert_eq!(verify::normalize(text), text);
}

// ---- sort_members (audit H8)

#[test]
fn sort_members_sorts_only_the_listed_positions() {
    let text = "tracks 3 playlists 3\nT x | 1\nP Podcasts 3\n  c\n  a\n  b\nP Road 3\n  z\n  y\n  x\nP Other 2\n  q\n  p";
    let sorted_first = "tracks 3 playlists 3\nT x | 1\nP Podcasts 3\n  a\n  b\n  c\nP Road 3\n  z\n  y\n  x\nP Other 2\n  q\n  p";
    assert_eq!(verify::sort_members(text, &[0]), sorted_first);
    // Nothing listed, or a position past the end: unchanged.
    assert_eq!(verify::sort_members(text, &[]), text);
    assert_eq!(verify::sort_members(text, &[3, 9]), text);
    // Two listed, the middle one keeps its order.
    assert_eq!(
        verify::sort_members(text, &[0, 2]),
        sorted_first.replace("  q\n  p", "  p\n  q")
    );
    // The last one alone.
    assert_eq!(
        verify::sort_members(text, &[2]),
        text.replace("  q\n  p", "  p\n  q")
    );
}

#[test]
fn sort_members_keeps_duplicates_and_the_count_line() {
    let text = "P Podcasts 4\n  b\n  a\n  b\n  a";
    assert_eq!(
        verify::sort_members(text, &[0]),
        "P Podcasts 4\n  a\n  a\n  b\n  b"
    );
}

#[test]
fn sort_members_counts_every_p_line_whatever_the_name_or_flags() {
    // Names that look like other lines of the format, a master and a smart
    // playlist (no members), all count as one position each.
    let text = "P   0000000000000009 2\n  b\n  a\nP T x | y 2\n  d\n  c\nP Master [master] 3\nP Smart [smart] 1\nP P 7 2\n  f\n  e\nP 0000000000000002 2\n  h\n  g";
    let at = |k: usize| verify::sort_members(text, &[k]);
    let mut want0 = text.to_owned();
    want0 = want0.replacen("  b\n  a", "  a\n  b", 1);
    assert_eq!(at(0), want0);
    assert_eq!(at(1), text.replace("  d\n  c", "  c\n  d"));
    assert_eq!(at(2), text, "master");
    assert_eq!(at(3), text, "smart");
    assert_eq!(at(4), text.replace("  f\n  e", "  e\n  f"));
    assert_eq!(at(5), text.replace("  h\n  g", "  g\n  h"));
}

#[test]
fn sort_members_with_an_empty_playlist_does_not_touch_the_next_one() {
    let text = "P Podcasts 0\nP Road 2\n  b\n  a";
    assert_eq!(verify::sort_members(text, &[0]), text);
    let last = "P Road 2\n  b\n  a\nP Podcasts 0";
    assert_eq!(verify::sort_members(last, &[1]), last);
    assert_eq!(
        verify::sort_members("P Podcasts 0\nP Shows 2\n  b\n  a", &[0, 1]),
        "P Podcasts 0\nP Shows 2\n  a\n  b"
    );
}

// A regular playlist named like the Podcasts one: only the Podcasts one (by
// position) is compared as a multiset; a reorder of the other is a difference.
#[test]
fn a_regular_playlist_named_like_the_podcasts_one_keeps_its_order_checked() {
    let text = |regular: &[&str]| {
        let body: String = regular.iter().map(|m| format!("\n  {m}")).collect();
        format!(
            "P Podcasts 3\n  c\n  a\n  b\nP Podcasts {}{body}",
            regular.len()
        )
    };
    let cmp = |a: &str, b: &str| {
        verify::differences(
            &verify::sort_members(a, &[0]),
            &verify::sort_members(b, &[0]),
            20,
        )
    };
    let (ab, ba) = (text(&["x", "y"]), text(&["y", "x"]));
    let d = cmp(&ab, &ba);
    assert_eq!(d.len(), 2, "{d:?}");
    // The special one reordered alone is still fine.
    let other_order = ab.replace("  c\n  a\n  b", "  b\n  c\n  a");
    assert!(cmp(&ab, &other_order).is_empty());
    // And the position of the regular one is what is checked.
    assert!(
        verify::differences(
            &verify::sort_members(&ab, &[1]),
            &verify::sort_members(&ba, &[1]),
            20
        )
        .is_empty()
    );
}

// A Podcasts playlist whose name is NULL prints as "(null)".
#[test]
fn a_podcasts_playlist_without_a_name_is_still_sorted() {
    let text = "P Road 1\n  r\nP (null) 3\n  c\n  a\n  b";
    assert_eq!(
        verify::sort_members(text, &[1]),
        "P Road 1\n  r\nP (null) 3\n  a\n  b\n  c"
    );
    let other = "P Road 1\n  r\nP (null) 3\n  b\n  c\n  a";
    assert!(
        verify::differences(
            &verify::sort_members(text, &[1]),
            &verify::sort_members(other, &[1]),
            20
        )
        .is_empty()
    );
    // Dropping a member is still seen.
    let dropped = "P Road 1\n  r\nP (null) 2\n  b\n  c";
    assert!(
        !verify::differences(
            &verify::sort_members(text, &[1]),
            &verify::sort_members(dropped, &[1]),
            20
        )
        .is_empty()
    );
}

#[test]
fn podcast_members_are_compared_as_a_multiset() {
    let with = |members: &[&str]| {
        let body: String = members.iter().map(|m| format!("\n  {m}")).collect();
        format!("P Podcasts {}{body}\nP Road 1\n  r", members.len())
    };
    let cmp = |a: &[&str], b: &[&str]| {
        verify::differences(
            &verify::sort_members(&with(a), &[0]),
            &verify::sort_members(&with(b), &[0]),
            20,
        )
    };
    assert!(cmp(&["a", "b", "c"], &["c", "a", "b"]).is_empty());
    assert!(!cmp(&["a", "b", "c"], &["a", "b"]).is_empty());
    assert!(!cmp(&["a", "b"], &["a", "b", "c"]).is_empty());
    assert!(!cmp(&["a", "b", "c"], &["a", "b", "d"]).is_empty());
    assert!(!cmp(&["a", "a", "b"], &["a", "b", "b"]).is_empty());
    // The order of the other playlists still matters.
    let swapped = with(&["a"]).replace("P Road 1\n  r", "P Road 2\n  s\n  r");
    let plain = with(&["a"]).replace("P Road 1\n  r", "P Road 2\n  r\n  s");
    assert!(
        !verify::differences(
            &verify::sort_members(&swapped, &[0]),
            &verify::sort_members(&plain, &[0]),
            20
        )
        .is_empty()
    );
}
