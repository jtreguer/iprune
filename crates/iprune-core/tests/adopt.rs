//! The adoption algorithm, tested through its public API only. Expected values
//! come from docs/technical.md section 10, not from reading the code.

use iprune_core::adopt::{
    Adoption, IpodPlaylist, IpodTrack, LibPlaylist, LibTrack, Proposal, Reason, adopt,
    match_playlists, normalize, resolve,
};

fn ip(dbid: u64, size: u64, ms: u32, title: &str, artist: &str) -> IpodTrack {
    IpodTrack {
        dbid,
        size,
        duration_ms: ms,
        title: Some(title.into()),
        artist: Some(artist.into()),
        album: Some("Al".into()),
        playlists: 0,
        playcount: 0,
    }
}

fn lib(id: i64, size: u64, ms: u32, title: &str, artist: &str) -> LibTrack {
    LibTrack {
        id,
        size,
        duration_ms: Some(ms),
        title: Some(title.into()),
        artist: Some(artist.into()),
        album: Some("Al".into()),
    }
}

fn proposal(a: &Adoption, dbid: u64) -> &Proposal {
    a.proposals
        .iter()
        .find(|p| p.dbid == dbid)
        .unwrap_or_else(|| panic!("no proposal for {dbid}: {a:?}"))
}

// ---- normalize

#[test]
fn normalize_table() {
    let cases: &[(Option<&str>, &str)] = &[
        (None, ""),
        (Some(""), ""),
        (Some("   "), ""),
        (Some("!!! ... ???"), ""),
        (Some("The"), "the"),
        (Some("the"), "the"),
        (Some("The The"), "the"),
        (Some("The Beatles"), "beatles"),
        (Some("THE   BEATLES"), "beatles"),
        (Some("  the beatles  "), "beatles"),
        // "the" must be a whole word to be dropped.
        (Some("Theatre of Pain"), "theatre of pain"),
        (Some("Them"), "them"),
        // Only a leading "the" is dropped.
        (Some("Fire and the Rain"), "fire and the rain"),
        (Some("ÉCOLE"), "ecole"),
        (Some("Beyoncé"), "beyonce"),
        // Fullwidth and ligature forms fold under NFKD.
        (Some("ＡＢＣ"), "abc"),
        (Some("ﬁn"), "fin"),
        (Some("Rock & Roll"), "rock roll"),
        (Some("Track 01"), "track 01"),
        (Some("日本語 タイトル"), "日本語 タイトル"),
    ];
    for (input, want) in cases {
        assert_eq!(normalize(*input), *want, "input {input:?}");
    }
}

#[test]
fn normalize_composed_and_decomposed_accents_agree() {
    let composed = "Jo\u{e3}o Gilberto \u{c5}ngstr\u{f6}m";
    let decomposed = "Joa\u{303}o Gilberto A\u{30a}ngstro\u{308}m";
    assert_ne!(composed, decomposed);
    assert_eq!(normalize(Some(composed)), normalize(Some(decomposed)));
    assert_eq!(normalize(Some(composed)), "joao gilberto angstrom");
}

// ---- pass 1: duration tolerance

#[test]
fn first_pass_duration_tolerance_is_1000_ms_inclusive() {
    for (lib_ms, accepted) in [
        (6000, true),
        (4000, true),
        (5000, true),
        (6001, false),
        (3999, false),
    ] {
        let a = adopt(
            &[ip(1, 100, 5000, "A", "X")],
            &[lib(10, 100, lib_ms, "A", "X")],
        );
        if accepted {
            assert_eq!(a.accepted, vec![(1, 10)], "library duration {lib_ms}");
            assert!(a.proposals.is_empty());
        } else {
            // Out of the first pass, but tags are equal and the gap is under
            // two seconds: the second pass proposes it.
            assert!(a.accepted.is_empty(), "library duration {lib_ms}");
            let p = proposal(&a, 1);
            assert_eq!(p.reason, Reason::TagsOnly, "library duration {lib_ms}");
            assert_eq!(p.default, Some(10));
        }
    }
}

#[test]
fn first_pass_needs_a_library_duration() {
    let mut l = lib(10, 100, 5000, "A", "X");
    l.duration_ms = None;
    let a = adopt(&[ip(1, 100, 5000, "A", "X")], &[l]);
    assert!(a.accepted.is_empty());
    assert!(a.proposals.is_empty());
    assert_eq!(a.unmatched_ipod, vec![1]);
    assert_eq!(a.unmatched_library, vec![10]);
}

#[test]
fn first_pass_requires_the_same_size() {
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X")],
        &[lib(10, 101, 5000, "A", "X")],
    );
    // Same tags and duration, other size: second pass, not an acceptance.
    assert!(a.accepted.is_empty());
    assert_eq!(proposal(&a, 1).reason, Reason::TagsOnly);
}

// ---- pass 2

#[test]
fn second_pass_duration_tolerance_is_2000_ms_inclusive() {
    for (lib_ms, proposed) in [(7000, true), (3000, true), (7001, false), (2999, false)] {
        let a = adopt(
            &[ip(1, 100, 5000, "A", "X")],
            &[lib(10, 999, lib_ms, "A", "X")],
        );
        assert!(a.accepted.is_empty());
        if proposed {
            let p = proposal(&a, 1);
            assert_eq!(
                (p.reason, p.default, p.candidates.clone()),
                (Reason::TagsOnly, Some(10), vec![10]),
                "library duration {lib_ms}"
            );
            assert!(a.unmatched_ipod.is_empty());
            assert!(a.unmatched_library.is_empty());
        } else {
            assert!(a.proposals.is_empty(), "library duration {lib_ms}");
            assert_eq!(a.unmatched_ipod, vec![1]);
            assert_eq!(a.unmatched_library, vec![10]);
        }
    }
}

#[test]
fn second_pass_needs_the_same_album() {
    let mut l = lib(10, 999, 5000, "A", "X");
    l.album = Some("Other".into());
    let a = adopt(&[ip(1, 100, 5000, "A", "X")], &[l]);
    assert!(a.proposals.is_empty());
    assert_eq!(a.unmatched_ipod, vec![1]);
}

#[test]
fn second_pass_ignores_library_files_without_title_or_artist() {
    // Two empty artists are no evidence in this pass, whatever the titles say.
    let mut i = ip(1, 100, 5000, "A", "");
    i.artist = None;
    let mut l = lib(10, 999, 5000, "A", "");
    l.artist = None;
    let a = adopt(&[i], &[l]);
    assert!(a.proposals.is_empty());
    assert_eq!(a.unmatched_ipod, vec![1]);
}

#[test]
fn second_pass_with_two_candidates_has_no_default() {
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X")],
        &[lib(12, 998, 5000, "A", "X"), lib(11, 999, 5000, "A", "X")],
    );
    let p = proposal(&a, 1);
    assert_eq!(p.reason, Reason::TagsOnly);
    assert_eq!(p.candidates, vec![11, 12]);
    assert_eq!(p.default, None);
    // With no default, accepting proposals still maps nothing.
    let (mapping, left) = resolve(&a, true);
    assert!(mapping.is_empty());
    assert_eq!(left.len(), 1);
}

#[test]
fn second_pass_never_offers_a_library_file_already_accepted() {
    // iPod 1 is an exact match for library 10. iPod 2 has the same tags but
    // another size: 10 is taken, so 2 has nothing left.
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X"), ip(2, 555, 5000, "A", "X")],
        &[lib(10, 100, 5000, "A", "X")],
    );
    assert_eq!(a.accepted, vec![(1, 10)]);
    assert!(a.proposals.is_empty());
    assert_eq!(a.unmatched_ipod, vec![2]);
}

// ---- scoring and ties

#[test]
fn one_matching_field_is_weak_and_neither_is_weak_too() {
    let ipod = [
        ip(1, 100, 5000, "A", "X"),
        ip(2, 200, 5000, "B", "Y"),
        ip(3, 300, 5000, "C", "Z"),
    ];
    let library = [
        lib(10, 100, 5000, "A", "other"), // title only
        lib(20, 200, 5000, "other", "Y"), // artist only
        lib(30, 300, 5000, "nope", "nada"),
    ];
    let a = adopt(&ipod, &library);
    assert!(a.accepted.is_empty());
    // One field agreeing: weak, with a default. Nothing agreeing: weak, and
    // no default, so --accept-proposals never takes it.
    for (dbid, id, score, default) in [(1, 10, 1, Some(10)), (2, 20, 1, Some(20)), (3, 30, 0, None)]
    {
        let p = proposal(&a, dbid);
        assert_eq!(p.reason, Reason::WeakTags, "dbid {dbid}");
        assert_eq!(p.score, score, "dbid {dbid}");
        assert_eq!((p.default, p.candidates.clone()), (default, vec![id]));
    }
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(1, 10), (2, 20)]);
    assert_eq!(left.iter().map(|p| p.dbid).collect::<Vec<_>>(), vec![3]);
}

#[test]
fn a_clear_winner_among_same_size_candidates_is_accepted() {
    // 11 scores 2, 10 scores 1: exactly one top candidate.
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X")],
        &[
            lib(10, 100, 5000, "A", "other"),
            lib(11, 100, 5000, "A", "X"),
        ],
    );
    assert_eq!(a.accepted, vec![(1, 11)]);
    assert!(a.proposals.is_empty());
    // The runner-up is not reported as unmatched: nothing proposes it either,
    // so it is simply a library file the iPod does not have.
    assert_eq!(a.unmatched_library, vec![10]);
}

#[test]
fn tied_candidates_are_several_candidates_lowest_id_first() {
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X")],
        &[
            lib(30, 100, 5000, "a", "x"),
            lib(10, 100, 5000, "A", "X"),
            lib(20, 100, 5000, "A", "X"),
        ],
    );
    let p = proposal(&a, 1);
    assert_eq!(p.reason, Reason::SeveralCandidates);
    assert_eq!(p.candidates, vec![10, 20, 30]);
    assert_eq!(p.default, Some(10));
    assert!(a.accepted.is_empty());
}

#[test]
fn tied_at_a_low_score_is_still_several_candidates() {
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X")],
        &[
            lib(10, 100, 5000, "zzz", "yyy"),
            lib(11, 100, 5000, "www", "vvv"),
        ],
    );
    assert_eq!(proposal(&a, 1).reason, Reason::SeveralCandidates);
}

#[test]
fn empty_tags_on_both_sides_are_no_evidence() {
    let mut i = ip(1, 100, 5000, "", "");
    i.title = None;
    i.artist = None;
    i.album = None;
    let mut l = lib(10, 100, 5000, "", "");
    l.title = None;
    l.artist = None;
    l.album = None;
    let a = adopt(&[i], &[l]);
    assert!(a.accepted.is_empty());
    assert_eq!(proposal(&a, 1).reason, Reason::WeakTags);
}

#[test]
fn punctuation_only_titles_are_empty() {
    let a = adopt(
        &[ip(1, 100, 5000, "...", "")],
        &[lib(10, 100, 5000, "???", "")],
    );
    assert!(a.accepted.is_empty());
    assert_eq!(proposal(&a, 1).reason, Reason::WeakTags);
}

#[test]
fn artist_missing_on_both_sides_counts_when_titles_match() {
    let mut i = ip(1, 100, 5000, "Demo", "");
    i.artist = None;
    let mut l = lib(10, 100, 5000, "demo", "");
    l.artist = None;
    let a = adopt(&[i], &[l]);
    assert_eq!(a.accepted, vec![(1, 10)]);
}

#[test]
fn artist_missing_on_one_side_only_is_not_a_match() {
    let mut i = ip(1, 100, 5000, "Demo", "");
    i.artist = None;
    let l = lib(10, 100, 5000, "Demo", "Somebody");
    let a = adopt(&[i], &[l]);
    assert!(a.accepted.is_empty());
    assert_eq!(proposal(&a, 1).reason, Reason::WeakTags);
}

#[test]
fn artist_missing_on_both_sides_without_matching_titles_scores_nothing() {
    let mut i = ip(1, 100, 5000, "One", "");
    i.artist = None;
    let mut l = lib(10, 100, 5000, "Two", "");
    l.artist = None;
    let a = adopt(&[i], &[l]);
    assert!(a.accepted.is_empty());
    assert_eq!(proposal(&a, 1).reason, Reason::WeakTags);
}

#[test]
fn tags_match_across_case_accents_and_the() {
    let a = adopt(
        &[ip(1, 100, 5000, "Jóga", "The Björk")],
        &[lib(10, 100, 5000, "JOGA", "bjork")],
    );
    assert_eq!(a.accepted, vec![(1, 10)]);
}

// ---- several iPod tracks claiming one library file

#[test]
fn three_ipod_tracks_claiming_one_file_all_become_proposals() {
    let mut t1 = ip(1, 100, 5000, "A", "X");
    let mut t2 = ip(2, 100, 5000, "A", "X");
    let mut t3 = ip(3, 100, 5000, "A", "X");
    t1.playlists = 1;
    t1.playcount = 50;
    t2.playlists = 2;
    t2.playcount = 0;
    t3.playlists = 0;
    t3.playcount = 999;
    let a = adopt(&[t1, t2, t3], &[lib(10, 100, 5000, "A", "X")]);
    assert!(a.accepted.is_empty());
    assert_eq!(a.proposals.len(), 3);
    for p in &a.proposals {
        assert_eq!(p.reason, Reason::SharedWithAnotherIpodTrack);
        assert_eq!(p.candidates, vec![10]);
    }
    // Most playlists wins over a higher play count.
    let defaults: Vec<(u64, Option<i64>)> =
        a.proposals.iter().map(|p| (p.dbid, p.default)).collect();
    assert_eq!(defaults, vec![(1, None), (2, Some(10)), (3, None)]);
    assert!(a.unmatched_library.is_empty());
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(2, 10)]);
    assert_eq!(left.len(), 2);
}

#[test]
fn default_rule_is_playlists_then_playcount_then_lowest_dbid() {
    // Give the claimers in an order that does not favour the right answer.
    let mk = |dbid, playlists, playcount| {
        let mut t = ip(dbid, 100, 5000, "A", "X");
        t.playlists = playlists;
        t.playcount = playcount;
        t
    };
    let ipod = [mk(9, 2, 3), mk(1, 1, 99), mk(8, 2, 3), mk(7, 2, 2)];
    let a = adopt(&ipod, &[lib(10, 100, 5000, "A", "X")]);
    let winners: Vec<u64> = a
        .proposals
        .iter()
        .filter(|p| p.default.is_some())
        .map(|p| p.dbid)
        .collect();
    // 8 and 9 tie on playlists (2) and playcount (3): the lower dbid wins.
    assert_eq!(winners, vec![8]);
    assert_eq!(a.proposals.len(), 4);
}

#[test]
fn claimers_of_different_files_are_independent() {
    let a = adopt(
        &[
            ip(1, 100, 5000, "A", "X"),
            ip(2, 100, 5000, "A", "X"),
            ip(3, 200, 5000, "B", "Y"),
        ],
        &[lib(10, 100, 5000, "A", "X"), lib(20, 200, 5000, "B", "Y")],
    );
    assert_eq!(a.accepted, vec![(3, 20)]);
    assert_eq!(a.proposals.len(), 2);
}

// ---- resolve

#[test]
fn resolve_without_accept_keeps_only_accepted() {
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X"), ip(2, 200, 5000, "B", "Y")],
        &[
            lib(10, 100, 5000, "A", "X"),
            lib(20, 200, 5000, "B", "other"),
        ],
    );
    let (mapping, left) = resolve(&a, false);
    assert_eq!(mapping, vec![(1, 10)]);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].dbid, 2);
}

#[test]
fn resolve_gives_a_contested_default_to_one_proposal_only() {
    let a = Adoption {
        proposals: vec![
            Proposal {
                dbid: 1,
                reason: Reason::WeakTags,
                score: 1,
                candidates: vec![10],
                default: Some(10),
            },
            Proposal {
                dbid: 2,
                reason: Reason::WeakTags,
                score: 1,
                candidates: vec![10],
                default: Some(10),
            },
        ],
        ..Adoption::default()
    };
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(1, 10)]);
    assert_eq!(left.iter().map(|p| p.dbid).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn resolve_leaves_several_candidates_unmapped_when_every_copy_is_taken() {
    let a = Adoption {
        accepted: vec![(1, 10), (2, 11)],
        proposals: vec![Proposal {
            dbid: 3,
            reason: Reason::SeveralCandidates,
            score: 2,
            candidates: vec![10, 11],
            default: Some(10),
        }],
        ..Adoption::default()
    };
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(1, 10), (2, 11)]);
    assert_eq!(left.len(), 1);
}

#[test]
fn resolve_never_picks_a_non_default_copy_for_other_reasons() {
    let a = Adoption {
        accepted: vec![(1, 10)],
        proposals: vec![Proposal {
            dbid: 3,
            reason: Reason::WeakTags,
            score: 1,
            candidates: vec![10, 11],
            default: Some(10),
        }],
        ..Adoption::default()
    };
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(1, 10)]);
    assert_eq!(left.len(), 1);
}

/// Small deterministic generator, so the property test needs no dependency.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn random_case(rng: &mut Lcg) -> (Vec<IpodTrack>, Vec<LibTrack>) {
    let words = ["a", "b", "c", "The a", "", "b!"];
    let pick = |rng: &mut Lcg| {
        let w = words[rng.next(words.len() as u64) as usize];
        (!w.is_empty()).then(|| w.to_owned())
    };
    let ipod = (0..rng.next(12))
        .map(|d| IpodTrack {
            dbid: d + 1,
            size: 100 + rng.next(3),
            duration_ms: 5000 + rng.next(4) as u32 * 700,
            title: pick(rng),
            artist: pick(rng),
            album: pick(rng),
            playlists: rng.next(3) as usize,
            playcount: rng.next(3) as u32,
        })
        .collect();
    let library = (0..rng.next(12))
        .map(|i| LibTrack {
            id: 10 + i as i64,
            size: 100 + rng.next(3),
            duration_ms: (rng.next(5) > 0).then(|| 5000 + rng.next(4) as u32 * 700),
            title: pick(rng),
            artist: pick(rng),
            album: pick(rng),
        })
        .collect();
    (ipod, library)
}

#[test]
fn random_inputs_keep_the_invariants() {
    let mut rng = Lcg(42);
    for case in 0..500 {
        let (ipod, library) = random_case(&mut rng);
        let a = adopt(&ipod, &library);

        // Every iPod track is in exactly one bucket.
        let mut seen: Vec<u64> = a.accepted.iter().map(|&(d, _)| d).collect();
        seen.extend(a.proposals.iter().map(|p| p.dbid));
        seen.extend(&a.unmatched_ipod);
        seen.sort_unstable();
        let mut all: Vec<u64> = ipod.iter().map(|t| t.dbid).collect();
        all.sort_unstable();
        assert_eq!(seen, all, "case {case}: {ipod:?} {library:?}");

        // Accepted library files are unique.
        let mut ids: Vec<i64> = a.accepted.iter().map(|&(_, id)| id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), a.accepted.len(), "case {case}");

        // A proposal's default is one of its candidates.
        for p in &a.proposals {
            assert!(!p.candidates.is_empty(), "case {case}");
            if let Some(d) = p.default {
                assert!(p.candidates.contains(&d), "case {case}");
            }
        }

        // Unmatched library files are mentioned nowhere else.
        for id in &a.unmatched_library {
            assert!(!a.accepted.iter().any(|&(_, i)| i == *id), "case {case}");
            assert!(
                !a.proposals.iter().any(|p| p.candidates.contains(id)),
                "case {case}"
            );
        }

        // The resolved mapping is one to one, with or without proposals.
        for accept in [false, true] {
            let (mapping, left) = resolve(&a, accept);
            let mut dbids: Vec<u64> = mapping.iter().map(|&(d, _)| d).collect();
            let mut libs: Vec<i64> = mapping.iter().map(|&(_, l)| l).collect();
            dbids.sort_unstable();
            libs.sort_unstable();
            let (nd, nl) = (dbids.len(), libs.len());
            dbids.dedup();
            libs.dedup();
            assert_eq!((dbids.len(), libs.len()), (nd, nl), "case {case}");
            assert_eq!(
                mapping.len() + left.len() + a.unmatched_ipod.len(),
                ipod.len(),
                "case {case}"
            );
            assert!(accept || mapping.len() == a.accepted.len(), "case {case}");
        }

        // The input order does not change the result.
        let mut ipod_r = ipod.clone();
        let mut lib_r = library.clone();
        ipod_r.reverse();
        lib_r.reverse();
        assert_eq!(adopt(&ipod_r, &lib_r), a, "case {case}");
    }
}

// ---- playlists

fn ipl(name: &str, members: &[u64]) -> IpodPlaylist {
    IpodPlaylist {
        name: name.into(),
        members: members.to_vec(),
    }
}

fn lpl(id: i64, name: &str, members: &[Option<i64>]) -> LibPlaylist {
    LibPlaylist {
        id,
        name: name.into(),
        members: members.to_vec(),
    }
}

#[test]
fn an_exact_name_beats_a_loose_one_whatever_the_order() {
    // "jazz" comes first and would take "Jazz" in a single loose pass.
    let m = match_playlists(
        &[ipl("jazz", &[]), ipl("Jazz", &[])],
        &[lpl(1, "Jazz", &[])],
        &[],
    );
    assert_eq!(m.matched.len(), 1);
    assert_eq!(m.matched[0].ipod_name, "Jazz");
    assert_eq!(m.matched[0].playlist_id, 1);
    assert_eq!(m.unmatched_ipod, vec!["jazz"]);
    assert!(m.unmatched_library.is_empty());
}

#[test]
fn a_file_matches_one_ipod_playlist_only() {
    let m = match_playlists(&[ipl("A", &[]), ipl("A", &[])], &[lpl(1, "A", &[])], &[]);
    assert_eq!(m.matched.len(), 1);
    assert_eq!(m.unmatched_ipod, vec!["A"]);
}

#[test]
fn loose_match_ignores_case_and_unicode_form() {
    let nfd = "Cafe\u{301} Mix";
    let m = match_playlists(&[ipl(nfd, &[])], &[lpl(7, "CAF\u{c9} mix", &[])], &[]);
    // NFC then lowercase: "café mix" on both sides.
    assert_eq!(m.matched.len(), 1, "{m:?}");
    assert_eq!(m.matched[0].playlist_id, 7);
    assert_eq!(m.matched[0].ipod_name, nfd);
    assert_eq!(m.matched[0].library_name, "CAF\u{c9} mix");
}

#[test]
fn playlists_with_other_names_are_unmatched_on_both_sides_sorted() {
    let m = match_playlists(
        &[ipl("zeta", &[]), ipl("alpha", &[])],
        &[lpl(1, "omega", &[]), lpl(2, "beta", &[])],
        &[],
    );
    assert!(m.matched.is_empty());
    assert_eq!(m.unmatched_ipod, vec!["alpha", "zeta"]);
    assert_eq!(m.unmatched_library, vec!["beta", "omega"]);
}

#[test]
fn same_contents_follows_order_repeats_and_length() {
    let map = [(1, 10), (2, 20)];
    let check = |ipod: &[u64], lib: &[Option<i64>]| {
        match_playlists(&[ipl("P", ipod)], &[lpl(1, "P", lib)], &map).matched[0].same_contents
    };
    assert!(check(&[1, 2], &[Some(10), Some(20)]));
    assert!(check(&[], &[]));
    // Repeats are kept on both sides.
    assert!(check(&[1, 2, 1], &[Some(10), Some(20), Some(10)]));
    assert!(!check(&[1, 2], &[Some(10), Some(20), Some(10)]));
    assert!(!check(&[1, 2, 1], &[Some(10), Some(20)]));
    // A repeat in a row is a different playlist from a single entry.
    assert!(check(&[1, 1], &[Some(10), Some(10)]));
    assert!(!check(&[1, 1], &[Some(10)]));
    assert!(!check(&[1], &[Some(10), Some(10)]));
    // Order matters.
    assert!(!check(&[2, 1], &[Some(10), Some(20)]));
    // A different track.
    assert!(!check(&[1, 2], &[Some(10), Some(21)]));
    // Extra member on either side.
    assert!(!check(&[1], &[Some(10), Some(20)]));
    assert!(!check(&[1, 2], &[Some(10)]));
}

#[test]
fn same_contents_treats_unmapped_dbids_and_unresolved_entries_alike() {
    let map = [(1, 10)];
    let check = |ipod: &[u64], lib: &[Option<i64>]| {
        match_playlists(&[ipl("P", ipod)], &[lpl(1, "P", lib)], &map).matched[0].same_contents
    };
    // dbid 99 is not adopted; the file has an entry it could not resolve.
    assert!(check(&[1, 99], &[Some(10), None]));
    // An unmapped dbid cannot equal a resolved library track.
    assert!(!check(&[1, 99], &[Some(10), Some(20)]));
    // A mapped dbid cannot equal an unresolved entry.
    assert!(!check(&[1, 1], &[Some(10), None]));
    // Positions still count.
    assert!(!check(&[99, 1], &[Some(10), None]));
}

// ---- review fixes

#[test]
fn several_candidates_lists_only_the_tied_best() {
    // Three copies on the iPod, two in the library, and an unrelated file with
    // the same size and duration that must never be picked as a "copy".
    let ipod = [
        ip(1, 100, 5000, "A", "X"),
        ip(2, 100, 5000, "A", "X"),
        ip(3, 100, 5000, "A", "X"),
    ];
    let library = [
        lib(10, 100, 5000, "A", "X"),
        lib(11, 100, 5000, "A", "X"),
        lib(12, 100, 5000, "B", "Y"),
    ];
    let a = adopt(&ipod, &library);
    for p in &a.proposals {
        assert_eq!(p.reason, Reason::SeveralCandidates);
        assert_eq!(p.candidates, vec![10, 11], "dbid {}", p.dbid);
        assert_eq!(p.score, 2);
    }
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(1, 10), (2, 11)]);
    assert_eq!(left.iter().map(|p| p.dbid).collect::<Vec<_>>(), vec![3]);
    assert!(mapping.iter().all(|&(_, l)| l != 12));
    assert_eq!(a.unmatched_library, vec![12]);
}

#[test]
fn resolve_gives_a_file_to_the_stronger_proposal_first() {
    // 5 and 6 match L exactly (shared); 1 has L as a one-field weak default.
    // The weak one has the lowest dbid and used to go first.
    let mut t6 = ip(6, 100, 5000, "A", "X");
    t6.playlists = 1;
    let ipod = [
        ip(1, 100, 5000, "A", "other"),
        ip(5, 100, 5000, "A", "X"),
        t6,
    ];
    let a = adopt(&ipod, &[lib(10, 100, 5000, "A", "X")]);
    // 1 is title-only (score 1) for the same file.
    assert_eq!(proposal(&a, 1).reason, Reason::WeakTags);
    assert_eq!(proposal(&a, 1).default, Some(10));
    let (mapping, left) = resolve(&a, true);
    assert_eq!(mapping, vec![(6, 10)]);
    let mut l: Vec<u64> = left.iter().map(|p| p.dbid).collect();
    l.sort_unstable();
    assert_eq!(l, vec![1, 5]);
}

#[test]
fn resolve_prefers_the_higher_score_within_a_reason() {
    let a = Adoption {
        proposals: vec![
            Proposal {
                dbid: 1,
                reason: Reason::WeakTags,
                score: 1,
                candidates: vec![10],
                default: Some(10),
            },
            Proposal {
                dbid: 2,
                reason: Reason::WeakTags,
                score: 2,
                candidates: vec![10],
                default: Some(10),
            },
        ],
        ..Adoption::default()
    };
    let (mapping, _) = resolve(&a, true);
    assert_eq!(mapping, vec![(2, 10)]);
}

#[test]
fn tags_only_scores_two_and_needs_an_album_for_a_default() {
    let mut i = ip(1, 100, 5000, "A", "X");
    let mut l = lib(10, 999, 5000, "A", "X");
    let a = adopt(&[i.clone()], &[l.clone()]);
    assert_eq!(
        (proposal(&a, 1).score, proposal(&a, 1).default),
        (2, Some(10))
    );
    // No album on either side: could be a live version or a remix.
    i.album = None;
    l.album = None;
    let a = adopt(&[i], &[l]);
    let p = proposal(&a, 1);
    assert_eq!(
        (p.reason, p.default, p.candidates.clone()),
        (Reason::TagsOnly, None, vec![10])
    );
    let (mapping, left) = resolve(&a, true);
    assert!(mapping.is_empty());
    assert_eq!(left.len(), 1);
}

#[test]
fn shared_proposals_score_two() {
    let a = adopt(
        &[ip(1, 100, 5000, "A", "X"), ip(2, 100, 5000, "A", "X")],
        &[lib(10, 100, 5000, "A", "X")],
    );
    assert!(a.proposals.iter().all(|p| p.score == 2));
}

#[test]
fn normalize_keeps_japanese_voicing_marks_but_folds_latin_accents() {
    assert_ne!(normalize(Some("ガ")), normalize(Some("カ")));
    assert_ne!(normalize(Some("ド")), normalize(Some("ト")));
    assert_ne!(normalize(Some("パ")), normalize(Some("ハ")));
    // Composed and decomposed kana agree.
    assert_eq!(normalize(Some("ガ")), normalize(Some("カ\u{3099}")));
    assert_eq!(normalize(Some("ガ")), "ガ");
    assert_eq!(normalize(Some("Éé e\u{301}")), "ee e");
    assert_eq!(normalize(Some("Ångström")), "angstrom");
}

#[test]
fn tied_at_score_zero_has_no_default_but_score_one_and_two_do() {
    // (ipod title/artist, library title/artist for both copies, score, defaulted)
    let cases = [
        (("A", "X"), ("zzz", "yyy"), 0, false),
        (("A", "X"), ("A", "yyy"), 1, true),
        (("A", "X"), ("A", "X"), 2, true),
    ];
    for ((it, ia), (lt, la), score, defaulted) in cases {
        let ipod = [ip(1, 100, 5000, it, ia), ip(2, 100, 5000, it, ia)];
        let library = [lib(10, 100, 5000, lt, la), lib(11, 100, 5000, lt, la)];
        let a = adopt(&ipod, &library);
        assert!(a.accepted.is_empty(), "score {score}");
        for p in &a.proposals {
            assert_eq!(p.reason, Reason::SeveralCandidates, "score {score}");
            assert_eq!(p.score, score);
            assert_eq!(p.candidates, vec![10, 11]);
            assert_eq!(p.default.is_some(), defaulted, "score {score}");
        }
        let (mapping, left) = resolve(&a, true);
        if defaulted {
            // Each iPod track takes the next free copy.
            assert_eq!(mapping, vec![(1, 10), (2, 11)], "score {score}");
            assert!(left.is_empty());
        } else {
            assert!(mapping.is_empty(), "score {score}");
            assert_eq!(left.len(), 2);
        }
    }
}
