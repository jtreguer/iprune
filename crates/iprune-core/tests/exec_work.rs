//! The pure parts of `exec`: `playlist_work` and `check_removals`.

use iprune_core::Error;
use iprune_core::adopt::{IpodPlaylist, IpodTrack};
use iprune_core::exec::{self, Op, Options, Work};
use iprune_core::ipod::{Contents, Track};
use iprune_core::plan::{
    Expected, ExpectedPlaylist, Member, Plan, PlaylistDeleteReason, PlaylistOp,
};
use iprune_core::store::devices::TrackMapping;
use iprune_core::tags::Tags;

fn track(dbid: u64) -> Track {
    Track {
        adopt: IpodTrack {
            dbid,
            size: 1,
            duration_ms: 1,
            title: None,
            artist: None,
            album: None,
            playlists: 0,
            playcount: 0,
            hash: None,
        },
        tags: Tags::default(),
        ipod_path: None,
        recent_plays: 0,
        recent_skips: 0,
        skips: 0,
        rating: 0,
        last_played: 0,
    }
}

fn map(track_id: i64, dbid: u64) -> TrackMapping {
    TrackMapping {
        track_id,
        dbid,
        synced_tags_hash: [0; 32],
        synced_size: 1,
        synced_duration_ms: None,
    }
}

/// An iPod with tracks 1..=4 and playlists 100 [1, 2] and 200 [3].
fn ipod() -> Contents {
    Contents {
        tracks: (1..=4).map(track).collect(),
        playlists: vec![
            IpodPlaylist {
                id: 100,
                name: "Road".into(),
                members: vec![1, 2],
            },
            IpodPlaylist {
                id: 200,
                name: "Mix".into(),
                members: vec![3],
            },
        ],
        ..Contents::default()
    }
}

/// Library tracks 11, 12, 13 are mapped to dbids 1, 2, 99; 99 is not on the
/// iPod (a stale mapping). Track 14 is not mapped.
fn mappings() -> Vec<TrackMapping> {
    vec![map(11, 1), map(12, 2), map(13, 99)]
}

fn ep(
    playlist_id: i64,
    ipod_id: Option<u64>,
    name: &str,
    members: Vec<Member>,
) -> ExpectedPlaylist {
    ExpectedPlaylist {
        playlist_id,
        ipod_id,
        name: name.into(),
        members,
    }
}

fn plan(playlists: Vec<PlaylistOp>, expected: Vec<ExpectedPlaylist>) -> Plan {
    Plan {
        playlists,
        expected: Expected {
            track_count: 4,
            playlists: expected,
        },
        ..Plan::default()
    }
}

use Member::{New, Track as T};

#[test]
fn new_members_are_resolved_through_the_mapping_or_left_out() {
    let members = vec![T(4), New(11), New(14), New(13), New(12)];
    let p = plan(
        vec![PlaylistOp::Create {
            playlist_id: 1,
            name: "Fresh".into(),
            members: members.clone(),
        }],
        vec![ep(1, None, "Fresh", members)],
    );
    let w = exec::playlist_work(&p, &mappings(), &ipod());
    // 14 is unmapped and 13 maps to a dbid that is not on the iPod.
    assert_eq!(
        w.ops,
        vec![Op::Create {
            playlist_id: 1,
            name: "Fresh".into(),
            members: vec![4, 1, 2]
        }]
    );
    assert_eq!(w.managed.len(), 1);
    assert_eq!(w.managed[0].members, vec![4, 1, 2]);
    assert_eq!((w.managed[0].playlist_id, w.managed[0].ipod_id), (1, None));
    assert_eq!(w.left_out, 2);
}

#[test]
fn left_out_counts_every_entry_once_per_playlist_including_repeats() {
    let a = vec![New(14), New(14), T(1)];
    let b = vec![New(14), T(2)];
    let p = plan(
        vec![],
        vec![ep(1, Some(100), "Road", a), ep(2, Some(200), "Mix", b)],
    );
    let w = exec::playlist_work(&p, &mappings(), &ipod());
    assert_eq!(w.left_out, 3);
    assert_eq!(w.managed[0].members, vec![1]);
    assert_eq!(w.managed[1].members, vec![2]);
    assert!(
        w.ops.is_empty(),
        "left_out comes from the expected lists, not the ops"
    );
}

#[test]
fn a_set_members_that_changes_nothing_is_dropped_and_one_that_changes_is_kept() {
    let p = plan(
        vec![
            // Road: [1, 2] already; resolves to the same list.
            PlaylistOp::SetMembers {
                playlist_id: 1,
                ipod_id: 100,
                name: "Road".into(),
                members: vec![T(1), New(12)],
            },
            // Mix: [3] -> [3, 3].
            PlaylistOp::SetMembers {
                playlist_id: 2,
                ipod_id: 200,
                name: "Mix".into(),
                members: vec![T(3), T(3)],
            },
        ],
        vec![
            ep(1, Some(100), "Road", vec![T(1), New(12)]),
            ep(2, Some(200), "Mix", vec![T(3), T(3)]),
        ],
    );
    let w = exec::playlist_work(&p, &mappings(), &ipod());
    assert_eq!(
        w.ops,
        vec![Op::SetMembers {
            ipod_id: 200,
            name: "Mix".into(),
            members: vec![3, 3]
        }]
    );
    // Both are still expected, with their final members.
    assert_eq!(w.managed.len(), 2);
    assert_eq!(w.managed[0].members, vec![1, 2]);
}

#[test]
fn a_set_members_is_dropped_only_when_the_whole_list_is_equal() {
    for new in [vec![T(2), T(1)], vec![T(1)], vec![T(1), T(2), T(2)], vec![]] {
        let p = plan(
            vec![PlaylistOp::SetMembers {
                playlist_id: 1,
                ipod_id: 100,
                name: "Road".into(),
                members: new.clone(),
            }],
            vec![ep(1, Some(100), "Road", new.clone())],
        );
        let w = exec::playlist_work(&p, &mappings(), &ipod());
        assert_eq!(w.ops.len(), 1, "{new:?}");
    }
}

#[test]
fn a_set_members_for_a_playlist_not_on_the_ipod_is_kept() {
    let p = plan(
        vec![PlaylistOp::SetMembers {
            playlist_id: 1,
            ipod_id: 555,
            name: "Gone".into(),
            members: vec![],
        }],
        vec![],
    );
    let w = exec::playlist_work(&p, &mappings(), &ipod());
    assert_eq!(
        w.ops,
        vec![Op::SetMembers {
            ipod_id: 555,
            name: "Gone".into(),
            members: vec![]
        }]
    );
}

#[test]
fn ops_keep_the_plans_order_and_deletes_are_collected() {
    let p = plan(
        vec![
            PlaylistOp::Delete {
                playlist_id: Some(9),
                ipod_id: 200,
                name: "Mix".into(),
                reason: PlaylistDeleteReason::FileDeleted,
            },
            PlaylistOp::Rename {
                playlist_id: 1,
                ipod_id: 100,
                from: "Road".into(),
                to: "Road 2".into(),
            },
            PlaylistOp::Create {
                playlist_id: 2,
                name: "New".into(),
                members: vec![T(4)],
            },
            PlaylistOp::SetMembers {
                playlist_id: 1,
                ipod_id: 100,
                name: "Road 2".into(),
                members: vec![T(4)],
            },
            PlaylistOp::Delete {
                playlist_id: None,
                ipod_id: 300,
                name: "Stray".into(),
                reason: PlaylistDeleteReason::Unmanaged,
            },
        ],
        vec![
            ep(1, Some(100), "Road 2", vec![T(4)]),
            ep(2, None, "New", vec![T(4)]),
        ],
    );
    let w = exec::playlist_work(&p, &mappings(), &ipod());
    let kinds: Vec<&str> = w
        .ops
        .iter()
        .map(|o| match o {
            Op::Delete { .. } => "delete",
            Op::Rename { .. } => "rename",
            Op::Create { .. } => "create",
            Op::SetMembers { .. } => "set",
        })
        .collect();
    assert_eq!(kinds, ["delete", "rename", "create", "set", "delete"]);
    assert_eq!(w.deleted, vec![200, 300]);
    assert_eq!(
        w.ops[1],
        Op::Rename {
            ipod_id: 100,
            to: "Road 2".into()
        }
    );
    assert_eq!(w.managed.len(), 2);
    assert_eq!(
        (w.managed[0].ipod_id, w.managed[0].name.as_str()),
        (Some(100), "Road 2")
    );
}

#[test]
fn an_empty_plan_gives_empty_work() {
    let w = exec::playlist_work(&plan(vec![], vec![]), &mappings(), &ipod());
    assert_eq!(w, Work::default());
}

#[test]
fn playlist_work_is_deterministic() {
    let p = plan(
        vec![PlaylistOp::Create {
            playlist_id: 1,
            name: "N".into(),
            members: vec![New(11), T(3)],
        }],
        vec![ep(1, None, "N", vec![New(11), T(3)])],
    );
    assert_eq!(
        exec::playlist_work(&p, &mappings(), &ipod()),
        exec::playlist_work(&p, &mappings(), &ipod())
    );
}

// ---- check_removals

fn deleting(n: usize) -> Work {
    Work {
        deleted: (0..n as u64).collect(),
        ..Work::default()
    }
}

fn refused(n: usize, mapped: usize) -> bool {
    match exec::check_removals(&deleting(n), mapped, Options::default()) {
        Ok(()) => false,
        Err(Error::MassRemoval { deletes, managed }) => {
            assert_eq!((deletes, managed), (n, mapped));
            true
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn up_to_five_deletions_are_always_allowed() {
    assert!(!refused(5, 5));
    assert!(!refused(5, 6));
    assert!(!refused(1, 1));
    assert!(!refused(0, 0));
}

#[test]
fn more_than_five_deletions_are_refused_when_over_a_tenth_of_the_managed_playlists() {
    assert!(refused(6, 5), "more deletes than mapped playlists");
    assert!(refused(6, 59));
    assert!(!refused(6, 60), "exactly a tenth is allowed");
    assert!(!refused(6, 61));
    assert!(refused(7, 69));
    assert!(!refused(7, 70));
    assert!(refused(50, 100));
}

#[test]
fn allow_mass_removal_lifts_the_refusal() {
    let opts = Options {
        allow_mass_removal: true,
    };
    assert!(exec::check_removals(&deleting(50), 51, opts).is_ok());
    assert!(exec::check_removals(&deleting(50), 51, Options::default()).is_err());
}

// ---- Error::is_check_failure (the CLI's exit code 3)

#[test]
fn check_failures_are_recognised_including_through_a_failed_undo() {
    let io = || Error::Io {
        path: "x".into(),
        source: std::io::ErrorKind::Other.into(),
    };
    let pre = || Error::PreWriteCheck(vec!["p".into()]);
    let ver = || Error::Verification {
        problems: vec!["p".into()],
        stamp: "s".into(),
    };
    let inc = || Error::WriteIncomplete {
        cause: "c".into(),
        stamp: "s".into(),
    };
    assert!(pre().is_check_failure() && ver().is_check_failure() && inc().is_check_failure());
    let wrapped = Error::UndoFailed {
        cause: Box::new(pre()),
        undo: Box::new(io()),
    };
    assert!(wrapped.is_check_failure());
    let plain = Error::UndoFailed {
        cause: Box::new(io()),
        undo: Box::new(pre()),
    };
    assert!(!plain.is_check_failure(), "only the cause counts");
    assert!(!io().is_check_failure());
    assert!(
        !Error::MassRemoval {
            deletes: 6,
            managed: 6
        }
        .is_check_failure()
    );
    assert!(!Error::NoSuchBackup("s".into()).is_check_failure());
    assert!(!Error::NotWritable("x".into()).is_check_failure());
}
