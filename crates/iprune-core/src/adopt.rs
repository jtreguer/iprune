//! Adoption: matching the tracks already on an iPod to library files, so the
//! first sync updates the device instead of refilling it.
//!
//! The matching is pure: plain structs in, an [`Adoption`] out. Reading the
//! iPod and saving the result happen elsewhere.

use std::collections::{BTreeMap, HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;

/// Duration tolerance for a match on size.
const DURATION_MS: u32 = 1000;
/// Duration tolerance for the tags-only second pass.
const DURATION_TAGS_ONLY_MS: u32 = 2000;
/// A tags-only match this close in duration is the same recording even
/// without an album: a live take or a remix is never within 10 ms.
const DURATION_SAME_RECORDING_MS: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpodTrack {
    pub dbid: u64,
    pub size: u64,
    pub duration_ms: u32,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Number of regular playlists the track is in, repeats counted once.
    pub playlists: usize,
    pub playcount: u32,
    /// Content hash of the file on the iPod, when it was read. Only tracks
    /// the tags could not settle are read, see [`to_compare`].
    pub hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibTrack {
    pub id: i64,
    pub size: u64,
    pub duration_ms: Option<u32>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Content hash of the file, as in the index.
    pub hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// Same size and duration, but only one of title and artist matches, or
    /// neither does.
    WeakTags,
    /// Several library files match equally well (duplicates in the library),
    /// or the iPod file is identical to several library files.
    SeveralCandidates,
    /// Another iPod track matched the same library file (duplicates on the
    /// iPod).
    SharedWithAnotherIpodTrack,
    /// Same artist, title and album, duration within two seconds, different
    /// size: the file was probably retagged or re-encoded after the copy.
    TagsOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub dbid: u64,
    pub reason: Reason,
    /// Title and artist agreement of the best candidate, 0 to 2.
    pub score: u8,
    /// Library track ids, best first.
    pub candidates: Vec<i64>,
    /// The match `--accept-proposals` takes, if there is a clear one.
    pub default: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Adoption {
    /// `(dbid, library id)` matched without doubt.
    pub accepted: Vec<(u64, i64)>,
    /// The dbids in `accepted` matched because the iPod file is identical to
    /// the library file.
    pub identical: Vec<u64>,
    pub proposals: Vec<Proposal>,
    /// iPod tracks with no candidate. Left on the iPod, unmanaged.
    pub unmatched_ipod: Vec<u64>,
    /// Library tracks no iPod track matched or was proposed for.
    pub unmatched_library: Vec<i64>,
}

/// Lowercase, accents and punctuation removed, spaces collapsed, a leading
/// "the " dropped. Empty input gives an empty string.
pub fn normalize(s: Option<&str>) -> String {
    let s = s.unwrap_or("");
    // Only the Latin accents (U+0300 to U+036F) go: other marks, such as the
    // Japanese voicing marks, change the word.
    let folded: String = s
        .nfkd()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .collect::<String>()
        .nfc()
        .collect::<String>()
        .to_lowercase();
    let words: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let words = match words.split_first() {
        Some((&"the", rest)) if !rest.is_empty() => rest,
        _ => &words[..],
    };
    words.join(" ")
}

/// Equal and not empty: two missing titles are no evidence of a match.
fn same(a: &str, b: &str) -> bool {
    !a.is_empty() && a == b
}

struct Norm {
    title: String,
    artist: String,
    album: String,
}

impl Norm {
    fn of(title: Option<&str>, artist: Option<&str>, album: Option<&str>) -> Self {
        Norm {
            title: normalize(title),
            artist: normalize(artist),
            album: normalize(album),
        }
    }

    /// 2 when title and artist agree, 1 when one does, 0 otherwise. An
    /// artist missing on both sides agrees, provided the titles match: many
    /// untagged files reached the iPod with only a title taken from the file
    /// name. A title missing on both sides is no evidence.
    fn score(&self, other: &Norm) -> u8 {
        let title = same(&self.title, &other.title);
        let artist = same(&self.artist, &other.artist)
            || (title && self.artist.is_empty() && other.artist.is_empty());
        u8::from(title) + u8::from(artist)
    }
}

fn close(a: u32, b: Option<u32>, tolerance: u32) -> bool {
    b.is_some_and(|b| a.abs_diff(b) <= tolerance)
}

/// What the first pass decided for one iPod track.
enum First {
    Exact(i64),
    /// Reason, best score, candidates, default.
    Proposed(Reason, u8, Vec<i64>, Option<i64>),
    None,
}

/// The iPod tracks whose file is worth comparing with the library: those
/// not accepted, for which a library file has exactly the same size. Durations
/// are unreliable for some MP3s on both sides, and tags are often missing, but
/// a byte-identical file is the same track.
pub fn to_compare(a: &Adoption, ipod: &[IpodTrack], library: &[LibTrack]) -> Vec<u64> {
    let sizes: HashSet<u64> = library.iter().map(|l| l.size).collect();
    let accepted: HashSet<u64> = a.accepted.iter().map(|&(dbid, _)| dbid).collect();
    let mut out: Vec<u64> = ipod
        .iter()
        .filter(|t| !accepted.contains(&t.dbid) && sizes.contains(&t.size))
        .map(|t| t.dbid)
        .collect();
    out.sort_unstable();
    out
}

pub fn adopt(ipod: &[IpodTrack], library: &[LibTrack]) -> Adoption {
    let mut by_size: HashMap<u64, Vec<&LibTrack>> = HashMap::new();
    let mut by_hash: HashMap<[u8; 32], Vec<i64>> = HashMap::new();
    for t in library {
        by_size.entry(t.size).or_default().push(t);
        by_hash.entry(t.hash).or_default().push(t.id);
    }
    for ids in by_hash.values_mut() {
        ids.sort_unstable();
    }
    let lib_norm: HashMap<i64, Norm> = library
        .iter()
        .map(|t| {
            (
                t.id,
                Norm::of(t.title.as_deref(), t.artist.as_deref(), t.album.as_deref()),
            )
        })
        .collect();

    // Pass 1: identical content, then size, duration and tags.
    let mut first: BTreeMap<u64, First> = BTreeMap::new();
    let mut identical: HashSet<u64> = HashSet::new();
    for it in ipod {
        let norm = Norm::of(
            it.title.as_deref(),
            it.artist.as_deref(),
            it.album.as_deref(),
        );
        let same_bytes = it.hash.and_then(|h| by_hash.get(&h));
        if let Some(ids) = same_bytes {
            let decision = match ids[..] {
                [id] => {
                    identical.insert(it.dbid);
                    First::Exact(id)
                }
                // Identical duplicates in the library: any copy will do.
                _ => {
                    let score = ids
                        .iter()
                        .map(|id| norm.score(&lib_norm[id]))
                        .max()
                        .unwrap_or(0);
                    First::Proposed(Reason::SeveralCandidates, score, ids.clone(), Some(ids[0]))
                }
            };
            first.insert(it.dbid, decision);
            continue;
        }
        let mut scored: Vec<(u8, i64)> = by_size
            .get(&it.size)
            .into_iter()
            .flatten()
            .filter(|l| close(it.duration_ms, l.duration_ms, DURATION_MS))
            .map(|l| (norm.score(&lib_norm[&l.id]), l.id))
            .collect();
        // Best score first, then lowest id for a stable order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let decision = match scored.as_slice() {
            [] => First::None,
            [(best, id), rest @ ..] => {
                let tied = rest.iter().filter(|(s, _)| s == best).count();
                let all = scored.iter().map(|&(_, id)| id).collect();
                match (*best, tied) {
                    (2, 0) => First::Exact(*id),
                    // With title and artist both disagreeing, equal size and
                    // duration are not enough to take it by default.
                    (_, 0) => {
                        First::Proposed(Reason::WeakTags, *best, all, (*best > 0).then_some(*id))
                    }
                    // Duplicate files in the library: any of the tied ones
                    // will do, and only those.
                    _ => {
                        let copies = scored
                            .iter()
                            .take_while(|(s, _)| s == best)
                            .map(|&(_, id)| id)
                            .collect();
                        // Copies tied at score 0 share no tag evidence: no default.
                        First::Proposed(
                            Reason::SeveralCandidates,
                            *best,
                            copies,
                            (*best > 0).then_some(*id),
                        )
                    }
                }
            }
        };
        first.insert(it.dbid, decision);
    }

    // A library track can be claimed by one iPod track only. When several
    // claim it, all become proposals and the default goes to an identical
    // file, then to the one the user cares about most: in more playlists,
    // then played more, then the lowest dbid.
    let ipod_by_dbid: HashMap<u64, &IpodTrack> = ipod.iter().map(|t| (t.dbid, t)).collect();
    let mut claims: HashMap<i64, Vec<u64>> = HashMap::new();
    for (&dbid, d) in &first {
        if let First::Exact(id) = d {
            claims.entry(*id).or_default().push(dbid);
        }
    }
    let mut out = Adoption::default();
    let mut taken: HashSet<i64> = HashSet::new();
    for (id, mut dbids) in claims {
        if let [dbid] = dbids[..] {
            out.accepted.push((dbid, id));
            if identical.contains(&dbid) {
                out.identical.push(dbid);
            }
            taken.insert(id);
            continue;
        }
        // A track proven identical to the file comes first: another
        // claimant matched only on size, duration and tags.
        dbids.sort_by_key(|d| {
            let t = ipod_by_dbid[d];
            (
                !identical.contains(d),
                std::cmp::Reverse(t.playlists),
                std::cmp::Reverse(t.playcount),
                *d,
            )
        });
        for (i, dbid) in dbids.into_iter().enumerate() {
            let default = (i == 0).then_some(id);
            out.proposals.push(Proposal {
                dbid,
                reason: Reason::SharedWithAnotherIpodTrack,
                score: 2,
                candidates: vec![id],
                default,
            });
        }
    }
    let mut unmatched: Vec<u64> = Vec::new();
    for (dbid, d) in first {
        match d {
            First::Exact(_) => {}
            First::Proposed(reason, score, candidates, default) => out.proposals.push(Proposal {
                dbid,
                reason,
                score,
                candidates,
                default,
            }),
            First::None => unmatched.push(dbid),
        }
    }

    // Pass 2: unmatched iPod tracks, same artist, title and album, any size.
    let lib_by_id: HashMap<i64, &LibTrack> = library.iter().map(|l| (l.id, l)).collect();
    let mut by_tags: HashMap<(&str, &str, &str), Vec<&LibTrack>> = HashMap::new();
    for l in library {
        let n = &lib_norm[&l.id];
        if !n.title.is_empty() && !n.artist.is_empty() {
            by_tags
                .entry((&n.title, &n.artist, &n.album))
                .or_default()
                .push(l);
        }
    }
    for dbid in unmatched {
        let it = ipod_by_dbid[&dbid];
        let n = Norm::of(
            it.title.as_deref(),
            it.artist.as_deref(),
            it.album.as_deref(),
        );
        let mut candidates: Vec<i64> = by_tags
            .get(&(n.title.as_str(), n.artist.as_str(), n.album.as_str()))
            .into_iter()
            .flatten()
            .filter(|l| {
                !taken.contains(&l.id)
                    && close(it.duration_ms, l.duration_ms, DURATION_TAGS_ONLY_MS)
            })
            .map(|l| l.id)
            .collect();
        candidates.sort_unstable();
        if candidates.is_empty() {
            out.unmatched_ipod.push(dbid);
        } else {
            // Without an album, same artist and title could be another
            // recording (live, remix): no default, unless the durations
            // agree to within a few milliseconds.
            let same_recording = |id: i64| {
                !n.album.is_empty()
                    || close(
                        it.duration_ms,
                        lib_by_id[&id].duration_ms,
                        DURATION_SAME_RECORDING_MS,
                    )
            };
            let default = match candidates[..] {
                [id] if same_recording(id) => Some(id),
                _ => None,
            };
            out.proposals.push(Proposal {
                dbid,
                reason: Reason::TagsOnly,
                score: 2,
                candidates,
                default,
            });
        }
    }

    let mentioned: HashSet<i64> = out
        .accepted
        .iter()
        .map(|&(_, id)| id)
        .chain(
            out.proposals
                .iter()
                .flat_map(|p| p.candidates.iter().copied()),
        )
        .collect();
    out.unmatched_library = library
        .iter()
        .map(|l| l.id)
        .filter(|id| !mentioned.contains(id))
        .collect();
    out.accepted.sort_unstable();
    out.identical.sort_unstable();
    out.proposals.sort_by_key(|p| (p.reason, p.dbid));
    out.unmatched_ipod.sort_unstable();
    out.unmatched_library.sort_unstable();
    out
}

/// The final `(dbid, library id)` mapping: accepted matches plus, when
/// `accept_proposals` is set, each proposal's default. The mapping is one to
/// one: a default already taken is skipped, except that among duplicate
/// library files (equally good candidates) the next free one is used. Returns the mapping and the
/// proposals left unresolved, whose iPod tracks stay unmanaged.
pub fn resolve(a: &Adoption, accept_proposals: bool) -> (Vec<(u64, i64)>, Vec<&Proposal>) {
    let mut mapping = a.accepted.clone();
    let mut taken: HashSet<i64> = mapping.iter().map(|&(_, id)| id).collect();
    let mut left = Vec::new();
    // Strongest evidence first, so a weak default never takes a file from
    // an exact match.
    let strength = |r: Reason| match r {
        Reason::SharedWithAnotherIpodTrack => 0,
        Reason::SeveralCandidates => 1,
        Reason::WeakTags => 2,
        Reason::TagsOnly => 3,
    };
    let mut order: Vec<&Proposal> = a.proposals.iter().collect();
    order.sort_by_key(|p| (strength(p.reason), std::cmp::Reverse(p.score), p.dbid));
    for p in order {
        let pick = match (accept_proposals, p.default, p.reason) {
            (false, _, _) | (true, None, _) => None,
            (true, Some(_), Reason::SeveralCandidates) => {
                p.candidates.iter().copied().find(|id| !taken.contains(id))
            }
            (true, Some(id), _) => (!taken.contains(&id)).then_some(id),
        };
        match pick {
            Some(id) => {
                taken.insert(id);
                mapping.push((p.dbid, id));
            }
            None => left.push(p),
        }
    }
    mapping.sort_unstable();
    (mapping, left)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpodPlaylist {
    /// libgpod's persistent playlist id.
    pub id: u64,
    pub name: String,
    pub members: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibPlaylist {
    pub id: i64,
    pub name: String,
    /// Resolved track ids, `None` for unresolved entries.
    pub members: Vec<Option<i64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistMatch {
    pub ipod_id: u64,
    pub ipod_name: String,
    pub playlist_id: i64,
    pub library_name: String,
    /// False when the iPod playlist, mapped through the track mapping,
    /// differs from the file.
    pub same_contents: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistAdoption {
    pub matched: Vec<PlaylistMatch>,
    pub unmatched_ipod: Vec<String>,
    pub unmatched_library: Vec<String>,
}

fn loose(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
}

/// Characters a file name cannot hold on Linux or Windows. Playlists exported
/// to files got `_` in their place: "Dance/Trance" became "Dance_Trance".
const NOT_IN_FILE_NAMES: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// NFC, with the characters file names cannot hold replaced by `_`; case
/// kept. Two names equal under this differ only in how they were stored.
pub(crate) fn file_chars(name: &str) -> String {
    name.nfc()
        .collect::<String>()
        .replace(NOT_IN_FILE_NAMES, "_")
}

pub(crate) fn file_safe(name: &str) -> String {
    loose(name).replace(NOT_IN_FILE_NAMES, "_")
}

/// The ways two playlist names are compared, strictest first: exact, then
/// NFC case-insensitive, then with file-name characters replaced.
pub(crate) const NAME_PASSES: [fn(&str) -> String; 3] = [str::to_owned, loose, file_safe];

/// Matches regular iPod playlists to playlist files by exact name, then by
/// NFC case-insensitive name, then with the characters file names cannot hold
/// replaced by `_`. Each file matches at most one iPod playlist.
pub fn match_playlists(
    ipod: &[IpodPlaylist],
    library: &[LibPlaylist],
    mapping: &[(u64, i64)],
) -> PlaylistAdoption {
    let map: HashMap<u64, i64> = mapping.iter().copied().collect();
    let mut free: Vec<&LibPlaylist> = library.iter().collect();
    let mut out = PlaylistAdoption::default();
    let take =
        |ip: &IpodPlaylist, i: usize, free: &mut Vec<&LibPlaylist>, out: &mut PlaylistAdoption| {
            let lib = free.remove(i);
            let mapped: Vec<Option<i64>> = ip.members.iter().map(|d| map.get(d).copied()).collect();
            out.matched.push(PlaylistMatch {
                ipod_id: ip.id,
                ipod_name: ip.name.clone(),
                playlist_id: lib.id,
                library_name: lib.name.clone(),
                same_contents: mapped == lib.members,
            });
        };
    let mut pending: Vec<&IpodPlaylist> = ipod.iter().collect();
    for key in NAME_PASSES {
        let mut next = Vec::new();
        for ip in pending {
            let k = key(&ip.name);
            match free.iter().position(|l| key(&l.name) == k) {
                Some(i) => take(ip, i, &mut free, &mut out),
                None => next.push(ip),
            }
        }
        pending = next;
    }
    out.unmatched_ipod = pending.iter().map(|ip| ip.name.clone()).collect();
    out.unmatched_library = free.iter().map(|l| l.name.clone()).collect();
    out.matched.sort_by(|a, b| a.ipod_name.cmp(&b.ipod_name));
    out.unmatched_ipod.sort();
    out.unmatched_library.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
            hash: None,
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
            hash: [id as u8; 32],
        }
    }

    #[test]
    fn normalize_folds_case_accents_punctuation_and_the() {
        assert_eq!(normalize(Some("The Beatles")), "beatles");
        assert_eq!(normalize(Some("Björk — Jóga!")), "bjork joga");
        assert_eq!(normalize(Some("  It's  a Sin ")), "it s a sin");
        assert_eq!(normalize(Some("The")), "the");
        assert_eq!(normalize(None), "");
    }

    #[test]
    fn exact_weak_several_and_none() {
        let ipod = [
            ip(1, 100, 5000, "A", "X"),
            ip(2, 200, 5000, "B", "Y"),
            ip(3, 300, 5000, "C", "Z"),
            ip(4, 400, 5000, "D", "W"),
        ];
        let library = [
            lib(10, 100, 5400, "a", "x"),
            lib(20, 200, 5000, "B", "other"),
            lib(30, 300, 5000, "C", "Z"),
            lib(31, 300, 5000, "c", "z"),
            lib(40, 400, 7000, "D", "W"),
        ];
        let a = adopt(&ipod, &library);
        assert_eq!(a.accepted, vec![(1, 10)]);
        assert_eq!(a.proposals.len(), 3);
        let p = |d| a.proposals.iter().find(|p| p.dbid == d).unwrap();
        assert_eq!((p(2).reason, p(2).default), (Reason::WeakTags, Some(20)));
        assert_eq!(
            (p(3).reason, p(3).default, p(3).candidates.clone()),
            (Reason::SeveralCandidates, Some(30), vec![30, 31])
        );
        // Size matches, duration does not, tags do: second pass.
        assert_eq!((p(4).reason, p(4).default), (Reason::TagsOnly, Some(40)));
        assert!(a.unmatched_ipod.is_empty());
        assert!(a.unmatched_library.is_empty());
    }

    #[test]
    fn duplicates_on_the_ipod_become_proposals_with_one_default() {
        let mut a1 = ip(1, 100, 5000, "A", "X");
        let mut a2 = ip(2, 100, 5000, "A", "X");
        a1.playcount = 3;
        a2.playlists = 1;
        let a = adopt(&[a1, a2], &[lib(10, 100, 5000, "A", "X")]);
        assert!(a.accepted.is_empty());
        let d: Vec<_> = a
            .proposals
            .iter()
            .map(|p| (p.dbid, p.reason, p.default))
            .collect();
        assert_eq!(
            d,
            vec![
                (1, Reason::SharedWithAnotherIpodTrack, None),
                (2, Reason::SharedWithAnotherIpodTrack, Some(10))
            ]
        );
        let (mapping, left) = resolve(&a, true);
        assert_eq!(mapping, vec![(2, 10)]);
        assert_eq!(left.len(), 1);
        let (mapping, left) = resolve(&a, false);
        assert!(mapping.is_empty());
        assert_eq!(left.len(), 2);
    }

    #[test]
    fn empty_tags_are_no_evidence() {
        let mut i = ip(1, 100, 5000, "", "");
        i.title = None;
        let mut l = lib(10, 100, 5000, "", "");
        l.title = None;
        let a = adopt(&[i], &[l]);
        assert_eq!(a.proposals[0].reason, Reason::WeakTags);
    }

    #[test]
    fn unmatched_on_both_sides() {
        let a = adopt(
            &[ip(1, 100, 5000, "A", "X")],
            &[lib(10, 999, 5000, "B", "Y")],
        );
        assert_eq!((a.unmatched_ipod, a.unmatched_library), (vec![1], vec![10]));
    }

    #[test]
    fn resolve_never_maps_a_library_track_twice() {
        let a = Adoption {
            accepted: vec![(1, 10)],
            proposals: vec![Proposal {
                dbid: 2,
                reason: Reason::TagsOnly,
                score: 2,
                candidates: vec![10],
                default: Some(10),
            }],
            ..Adoption::default()
        };
        let (mapping, left) = resolve(&a, true);
        assert_eq!(mapping, vec![(1, 10)]);
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn duplicate_library_files_go_to_the_next_free_copy() {
        let ipod = [ip(1, 100, 5000, "A", "X"), ip(2, 100, 5000, "A", "X")];
        let library = [lib(10, 100, 5000, "A", "X"), lib(11, 100, 5000, "A", "X")];
        let a = adopt(&ipod, &library);
        assert!(
            a.proposals
                .iter()
                .all(|p| p.reason == Reason::SeveralCandidates)
        );
        let (mapping, left) = resolve(&a, true);
        assert_eq!(mapping, vec![(1, 10), (2, 11)]);
        assert!(left.is_empty());
    }

    #[test]
    fn playlists_match_exact_then_loose() {
        let ipod = vec![
            IpodPlaylist {
                id: 1,
                name: "80s".into(),
                members: vec![1, 2],
            },
            IpodPlaylist {
                id: 2,
                name: "jazz".into(),
                members: vec![],
            },
            IpodPlaylist {
                id: 3,
                name: "Gone".into(),
                members: vec![],
            },
        ];
        let library = vec![
            LibPlaylist {
                id: 1,
                name: "80s".into(),
                members: vec![Some(10), None],
            },
            LibPlaylist {
                id: 2,
                name: "Jazz".into(),
                members: vec![],
            },
            LibPlaylist {
                id: 3,
                name: "New".into(),
                members: vec![],
            },
        ];
        let m = match_playlists(&ipod, &library, &[(1, 10)]);
        assert_eq!(m.matched.len(), 2);
        assert!(
            m.matched[0].same_contents,
            "dbid 2 is unmapped and the entry unresolved"
        );
        assert_eq!(m.matched[1].library_name, "Jazz");
        assert_eq!(m.unmatched_ipod, vec!["Gone"]);
        assert_eq!(m.unmatched_library, vec!["New"]);
    }
}
