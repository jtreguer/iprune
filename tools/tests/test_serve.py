"""`autotag serve`, in process: Server.handle with stubbed lookups, and the
read-send loop with stdin and stdout replaced.

The framing of the real process is in test_serve_process.py.
"""

import copy
import errno
import io
import json
import pathlib
import sys
import types
import unicodedata

import pytest
from mutagen.id3 import ID3

from conftest import (KINDS, REC1, REC2, REC3, REL1, REL2, REL3, Snapshot, audio_md5,
                      digest, fixture_json, temp_files, tree)

FULL = {"title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low",
        "album_artist": "a-ha", "genre": "Synth-Pop", "year": 1985, "track_no": 3,
        "track_total": 12, "disc_no": 1, "disc_total": 1}
DEST = "a-ha/Hunting High and Low/03 Take On Me.mp3"


@pytest.fixture
def lib(make_library):
    return make_library()


@pytest.fixture
def srv(at, lib):
    return at.Server(lib, "")


def apply_req(f, tags=None, **kw):
    return {"op": "apply", "file": str(f), "tags": dict(FULL if tags is None else tags), **kw}


def lookup_file(make_audio, name="a-ha - Take On Me.mp3"):
    """A two-second file: far from every recorded length."""
    return make_audio("mp3", name=name)


@pytest.fixture
def take_on_me(long_mp3, tmp_path):
    """A 218 second file named like the recorded search: near recording 1."""
    f = tmp_path / "inbox" / "a-ha - Take On Me.mp3"
    f.parent.mkdir(exist_ok=True)
    f.write_bytes(long_mp3.read_bytes())
    return f


# -- lookup ----------------------------------------------------------------------------

def test_lookup_answer_for_the_recorded_search(at, net, srv, make_audio, take_on_me):
    f = take_on_me
    ans = srv.handle({"op": "lookup", "file": str(f)})
    assert ans["by"] == "name"
    assert ans["query"] == {"artist": "a-ha", "title": "Take On Me"}
    assert ans["duration_ms"] == pytest.approx(218000, abs=150)
    assert [c["recording"] for c in ans["candidates"]] == [REC1, REC3, REC2]
    assert [c["release"] for c in ans["candidates"]] == [REL1, REL3, REL2]
    assert [c["length_ms"] for c in ans["candidates"]] == [217906, None, 119773]
    assert net.kinds("name") == ['recording:"Take On Me" AND artist:"a-ha"']
    assert set(ans["candidates"][0]) == {"recording", "release", "label", "length_ms", "certain",
                                         "tags", "destination", "same_title"}


def test_only_the_first_candidate_has_its_genre_looked_up(at, net, srv, make_audio, take_on_me):
    f = take_on_me
    ans = srv.handle({"op": "lookup", "file": str(f)})
    first, *others = ans["candidates"]
    assert first["tags"]["genre"] == "Synth-Pop"
    assert [c["tags"]["genre"] for c in others] == [None, None]
    assert all("genre" in c["tags"] for c in others)  # present and null, not absent
    assert net.kinds("genre") == [("recording", REC1)]


def test_lookup_tags_of_the_first_candidate(at, net, srv, make_audio, take_on_me):
    ans = srv.handle({"op": "lookup", "file": str(take_on_me)})
    assert ans["candidates"][0]["tags"] == {
        "title": "Take On Me", "artist": "a-ha",
        "album": "Die ultimative Chartshow: Die beliebtesten Hits aller Zeiten",
        "album_artist": "Various Artists", "disc_no": 1, "disc_total": 2, "year": 2019,
        "track_no": 7, "track_total": 22, "genre": "Synth-Pop"}
    # Only candidate 1 has a first-release-date of its own; the others still get a year.
    assert [c["tags"]["year"] for c in ans["candidates"]] == [2019, 2001, 2012]


def test_lookup_destinations_follow_the_layout(at, net, srv, lib, make_audio, take_on_me):
    ans = srv.handle({"op": "lookup", "file": str(take_on_me)})
    d = [c["destination"] for c in ans["candidates"]]
    assert d[0] == "Compilations/Die ultimative Chartshow_ Die beliebtest/1-07 Take On Me.mp3"
    assert d[1] == "a\u2010ha/Homecoming_ Live at Vallhall/19 Take On Me.mp3"
    assert d[2] == "Compilations/Dancemania Sparkle CLASSICS/14 Take On Me.mp3"
    assert all(not (lib / x).exists() for x in d)   # nothing is created by a lookup


def test_lookup_reports_same_title_files_of_an_existing_artist(at, net, srv, lib, make_audio, take_on_me):
    old = lib / "Compilations" / "Old Hits" / "01 Take On Me.mp3"
    old.parent.mkdir(parents=True)
    old.write_bytes(b"x")
    ans = srv.handle({"op": "lookup", "file": str(take_on_me)})
    assert [c["same_title"] for c in ans["candidates"]] == [
        ["Compilations/Old Hits/01 Take On Me.mp3"], [], ["Compilations/Old Hits/01 Take On Me.mp3"]]


@pytest.mark.parametrize("state", ["absent", "empty"])
def test_lookup_destination_is_null_when_the_library_is_not_there(at, net, tmp_path, make_audio, state, take_on_me):
    root = tmp_path / "nolib"
    if state == "empty":
        root.mkdir()
    ans = at.Server(root, "").handle({"op": "lookup", "file": str(take_on_me)})
    assert len(ans["candidates"]) == 3
    assert [(c["destination"], c["same_title"]) for c in ans["candidates"]] == [(None, [])] * 3
    assert not (root.exists() and list(root.iterdir()))   # not created either


def test_lookup_certain_follows_the_ten_second_rule(at, net, srv, make_audio, long_mp3, tmp_path):
    short = srv.handle({"op": "lookup", "file": str(lookup_file(make_audio))})
    assert [c["certain"] for c in short["candidates"]] == [False, False, False]
    f = tmp_path / "inbox" / "a-ha - Take On Me.mp3"
    f.parent.mkdir(exist_ok=True)
    f.write_bytes(long_mp3.read_bytes())
    long = srv.handle({"op": "lookup", "file": str(f)})
    assert long["duration_ms"] == pytest.approx(218000, abs=150)
    assert [c["certain"] for c in long["candidates"]] == [True, False, False]
    assert long["candidates"][0]["recording"] == REC1


def test_lookup_query_from_the_name_then_the_file_tags(at, net, srv, make_audio):
    f = make_audio("mp3", name="track01.mp3")
    at.write_tags(f, {"title": "Take On Me", "artist": "a-ha"})
    ans = srv.handle({"op": "lookup", "file": str(f)})
    assert ans["query"] == {"artist": "a-ha", "title": "Take On Me"}


def test_lookup_with_artist_and_title_uses_them_and_skips_the_fingerprint(at, net, lib, make_audio, monkeypatch):
    monkeypatch.setattr(at, "fingerprint_ids", lambda *a: pytest.fail("fingerprint used"))
    f = make_audio("mp3", name="whatever.mp3")
    ans = at.Server(lib, "KEY").handle({"op": "lookup", "file": str(f),
                                        "artist": "Guts", "title": "Aimer sans amour"})
    assert ans["by"] == "name"
    assert ans["query"] == {"artist": "Guts", "title": "Aimer sans amour"}
    assert net.kinds("name") == ['recording:"Aimer sans amour" AND artist:"Guts"']


def test_lookup_with_only_a_title_skips_the_fingerprint_too(at, net, lib, make_audio, monkeypatch):
    monkeypatch.setattr(at, "fingerprint_ids", lambda *a: pytest.fail("fingerprint used"))
    f = make_audio("mp3", name="Band - Song.mp3")
    ans = at.Server(lib, "KEY").handle({"op": "lookup", "file": str(f), "title": "Other"})
    assert ans["query"] == {"artist": "Band", "title": "Other"}


def test_lookup_by_fingerprint(at, net, lib, make_audio, monkeypatch):
    monkeypatch.setattr(at, "fingerprint_ids", lambda path, key: [REC1, REC2])
    f = make_audio("mp3", name="whatever.mp3")
    ans = at.Server(lib, "KEY").handle({"op": "lookup", "file": str(f)})
    assert ans["by"] == "fingerprint"
    assert net.kinds("rid") == [f"rid:({REC1} OR {REC2})"]
    assert net.kinds("name") == []
    assert [c["recording"] for c in ans["candidates"]] == [REC1]
    assert ans["candidates"][0]["certain"] is True       # a fingerprint match needs no length
    assert ans["query"] == {"artist": "", "title": "whatever"}


def test_lookup_falls_back_to_the_name_when_the_fingerprint_gives_no_ids(at, net, lib, take_on_me, monkeypatch):
    monkeypatch.setattr(at, "fingerprint_ids", lambda path, key: [])
    ans = at.Server(lib, "KEY").handle({"op": "lookup", "file": str(take_on_me)})
    assert ans["by"] == "name"
    assert len(net.kinds("name")) == 1 and net.kinds("rid") == []
    assert len(ans["candidates"]) == 3


# Regression: `by` stayed "fingerprint" when the rid search found no recording and the
# candidates came from the name search (iprune reads `by` to decide Ready).
def test_lookup_by_is_name_when_the_fingerprint_ids_find_no_recording(at, net, lib, take_on_me, monkeypatch):
    monkeypatch.setattr(at, "fingerprint_ids", lambda path, key: [REC1])
    net.search_rid = fixture_json("mb-search-none.json")
    ans = at.Server(lib, "KEY").handle({"op": "lookup", "file": str(take_on_me)})
    assert len(net.kinds("name")) == 1 and len(ans["candidates"]) == 3   # the fallback did happen
    assert ans["by"] == "name"
    assert [c["certain"] for c in ans["candidates"]] == [True, False, False]


def test_a_short_file_orders_far_candidates_by_release_rank_then_date(at, net, srv, make_audio):
    # 2 s against 218 s: every length is far, so only the release decides; the
    # earlier dates come first (Live 2001, DJ-mix 2012, Compilation 2019).
    ans = srv.handle({"op": "lookup", "file": str(lookup_file(make_audio))})
    assert [c["recording"] for c in ans["candidates"]] == [REC3, REC2, REC1]


def test_no_key_never_fingerprints(at, net, srv, make_audio, monkeypatch, take_on_me):
    monkeypatch.setattr(at, "fingerprint_ids", lambda *a: pytest.fail("fingerprint used"))
    assert srv.handle({"op": "lookup", "file": str(take_on_me)})["by"] == "name"


def test_lookup_with_no_match(at, net, srv, make_audio, take_on_me):
    net.search_name = fixture_json("mb-search-none.json")
    ans = srv.handle({"op": "lookup", "file": str(take_on_me)})
    assert ans["candidates"] == [] and ans["by"] == "name"
    assert net.kinds("genre") == []


def test_lookup_choices_limits_the_candidates(at, net, srv, make_audio, take_on_me):
    f = take_on_me
    ans = srv.handle({"op": "lookup", "file": str(f), "choices": 2})
    assert len(ans["candidates"]) == 2
    ans = srv.handle({"op": "lookup", "file": str(f), "choices": 1})
    assert [c["tags"]["genre"] for c in ans["candidates"]] == ["Synth-Pop"]


@pytest.mark.parametrize("choices", [0, 26, -1, True, "3", 2.5, None])
def test_lookup_refuses_bad_choices_before_any_request(at, net, srv, make_audio, choices, take_on_me):
    with pytest.raises(at.Fail) as e:
        srv.handle({"op": "lookup", "file": str(take_on_me), "choices": choices})
    assert "choices" in str(e.value)
    assert net.calls == []


@pytest.mark.parametrize("req, fragment", [
    ({"op": "lookup"}, "file must be"),
    ({"op": "lookup", "file": 5}, "file must be"),
    ({"op": "lookup", "file": ""}, "file must be"),
    ({"op": "lookup", "file": "relative/x.mp3"}, "absolute"),
    ({"op": "lookup", "file": "/nonexistent/x.mp3"}, "no such file"),
])
def test_lookup_refuses_bad_files(at, net, srv, req, fragment):
    with pytest.raises(at.Fail) as e:
        srv.handle(req)
    assert fragment in str(e.value)
    assert net.calls == []


def test_lookup_refuses_an_unsupported_file(at, net, srv, tmp_path):
    f = tmp_path / "a.flac"
    f.write_bytes(b"x")
    with pytest.raises(at.Fail) as e:
        srv.handle({"op": "lookup", "file": str(f)})
    assert "not a supported file" in str(e.value)


@pytest.mark.parametrize("field", ["artist", "title"])
@pytest.mark.parametrize("value", ["", "   ", 5, []])
def test_lookup_refuses_a_blank_or_non_string_search(at, net, srv, make_audio, field, value, take_on_me):
    with pytest.raises(at.Fail):
        srv.handle({"op": "lookup", "file": str(take_on_me), field: value})
    assert net.calls == []


def test_lookup_never_writes_the_file(at, net, srv, make_audio, take_on_me):
    f = take_on_me
    snap = Snapshot(f)
    srv.handle({"op": "lookup", "file": str(f)})
    snap.assert_unchanged()


# -- tags -----------------------------------------------------------------------------

def test_tags_for_a_candidate_seen_in_a_lookup_makes_no_search(at, net, srv, make_audio, take_on_me):
    srv.handle({"op": "lookup", "file": str(take_on_me)})
    net.calls.clear()
    ans = srv.handle({"op": "tags", "recording": REC1, "release": REL1})
    assert net.kinds("rid") == [] and net.kinds("name") == []
    assert ans["tags"]["genre"] == "Synth-Pop"
    assert ans["tags"]["album_artist"] == "Various Artists"
    assert list(ans) == ["tags"]


def test_tags_for_a_later_candidate_looks_its_genre_up(at, net, srv, make_audio, take_on_me):
    srv.handle({"op": "lookup", "file": str(take_on_me)})
    net.calls.clear()
    net.genres[("release-group", "778b9081-fb9a-3fa0-9169-cfae8b6927ce")] = {
        "genres": [{"name": "live rock", "count": 2}]}
    ans = srv.handle({"op": "tags", "recording": REC3, "release": REL3})
    assert ans["tags"]["genre"] == "Live Rock"
    assert ans["tags"]["track_no"] == 19
    assert net.kinds("genre") == [("recording", REC3), ("release-group", "778b9081-fb9a-3fa0-9169-cfae8b6927ce")]


def test_tags_not_seen_before_goes_through_a_rid_search_once(at, net, srv):
    ans = srv.handle({"op": "tags", "recording": REC1, "release": REL1})
    assert net.kinds("rid") == [f"rid:({REC1})"]
    assert ans["tags"]["title"] == "Take On Me" and ans["tags"]["genre"] == "Synth-Pop"
    assert ans["tags"]["track_no"] == 7
    srv.handle({"op": "tags", "recording": REC1, "release": REL1})
    assert len(net.kinds("rid")) == 1               # now cached


def test_tags_uses_the_requested_release_not_the_best_one(at, net, srv):
    rid = copy.deepcopy(net.search_rid)
    rel = copy.deepcopy(rid["recordings"][0]["releases"][0])
    rel.update(id="other-release", title="Other Album", date="2020-02-02")
    rid["recordings"][0]["releases"].append(rel)
    net.search_rid = rid
    ans = srv.handle({"op": "tags", "recording": REC1, "release": "other-release"})
    assert ans["tags"]["album"] == "Other Album" and ans["tags"]["year"] == 2020


@pytest.mark.parametrize("rec, rel", [(REC1, "no-such-release"), (REC2, REL2), ("nope", REL1)])
def test_tags_for_an_unknown_pair_is_an_error(at, net, srv, rec, rel):
    with pytest.raises(at.Fail) as e:
        srv.handle({"op": "tags", "recording": rec, "release": rel})
    assert "not found" in str(e.value)
    assert net.kinds("genre") == []


@pytest.mark.parametrize("req", [
    {"op": "tags"}, {"op": "tags", "recording": REC1}, {"op": "tags", "release": REL1},
    {"op": "tags", "recording": "", "release": REL1}, {"op": "tags", "recording": 1, "release": REL1}])
def test_tags_needs_both_ids(at, net, srv, req):
    with pytest.raises(at.Fail):
        srv.handle(req)
    assert net.calls == []


# -- dispatch ----------------------------------------------------------------------------

@pytest.mark.parametrize("req", [{}, {"op": None}, {"op": "frobnicate"}, {"op": ["lookup"]}, {"op": "LOOKUP"}])
def test_unknown_op(at, srv, req):
    with pytest.raises(at.Fail) as e:
        srv.handle(req)
    assert "unknown op" in str(e.value)


# -- check_tags --------------------------------------------------------------------------

def test_check_tags_keeps_good_values_and_drops_none_and_blank(at):
    got = at.check_tags({"title": "  T  ", "artist": "", "album": "   ", "genre": None,
                         "year": 1985, "track_no": 3, "compilation": False})
    assert got == {"title": "T", "year": 1985, "track_no": 3, "compilation": False}


def test_check_tags_accepts_every_field(at):
    assert at.check_tags(dict(FULL, composer="C", compilation=True)) == dict(FULL, composer="C", compilation=True)


@pytest.mark.parametrize("bad, fragment", [
    ({"rating": 5}, "unknown tag field"),
    ({"Title": "x"}, "unknown tag field"),
    ({"title": 5}, "title must be a string"),
    ({"artist": ["a"]}, "artist must be a string"),
    ({"genre": True}, "genre must be a string"),
    ({"year": "1985"}, "year must be a whole number"),
    ({"year": 1985.0}, "year must be a whole number"),
    ({"year": True}, "year must be a whole number"),
    ({"track_no": False}, "track_no must be a whole number"),
    ({"disc_total": "2"}, "disc_total must be a whole number"),
    ({"year": 999}, "year must be from 1000 to 9999"),
    ({"year": 10000}, "year must be from 1000 to 9999"),
    ({"track_no": 0}, "track_no must be from 1 to 999"),
    ({"track_total": 1000}, "track_total must be from 1 to 999"),
    ({"disc_no": -1}, "disc_no must be from 1 to 999"),
    ({"disc_total": 0}, "disc_total must be from 1 to 999"),
    ({"compilation": "yes"}, "compilation must be true or false"),
    ({"compilation": 1}, "compilation must be true or false"),
])
def test_check_tags_refuses(at, bad, fragment):
    with pytest.raises(at.Fail) as e:
        at.check_tags(bad)
    assert fragment in str(e.value)


@pytest.mark.parametrize("good", [{"year": 1000}, {"year": 9999}, {"track_no": 1}, {"track_no": 999},
                                  {"disc_no": 1}, {"disc_total": 999}])
def test_check_tags_accepts_the_range_edges(at, good):
    assert at.check_tags(good) == good


@pytest.mark.parametrize("bad", [None, [], "x", 5, [("title", "x")]])
def test_check_tags_needs_an_object(at, bad):
    with pytest.raises(at.Fail) as e:
        at.check_tags(bad)
    assert "JSON object" in str(e.value)


def test_check_tags_empty_object_is_fine(at):
    assert at.check_tags({}) == {}


# -- apply: what it answers ------------------------------------------------------------

def test_dry_run_writes_nothing_and_says_where(at, srv, lib, make_audio):
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    ans = srv.handle(apply_req(f, dry_run=True))
    assert ans == {"destination": DEST, "same_title": [], "filed": False}
    assert tree(lib) == before             # no folder made either
    snap.assert_unchanged()
    assert temp_files(lib) == [] and temp_files(f.parent) == []


def test_dry_run_with_tags_that_have_no_title_is_an_error(at, srv, lib, make_audio):
    with pytest.raises(at.Fail) as e:
        srv.handle(apply_req(make_audio("mp3"), {"artist": "A"}, dry_run=True))
    assert "title" in str(e.value)


@pytest.mark.parametrize("kind", KINDS)
def test_apply_files_a_tagged_copy_and_removes_the_source(at, srv, lib, make_audio, kind):
    f = make_audio(kind)
    audio_before = audio_md5(f)
    ids = {"recording": REC1, "release": REL1}
    ans = srv.handle(apply_req(f, **ids))
    suffix = f.suffix
    dest = lib / "a-ha" / "Hunting High and Low" / f"03 Take On Me{suffix}"
    assert ans == {"destination": str(dest.relative_to(lib)), "same_title": [], "filed": True}
    assert not f.exists()
    assert at.read_tags(at.open_audio(dest)) == dict(FULL)
    assert audio_md5(dest) == audio_before
    assert temp_files(lib) == []
    assert [p.name for p in dest.parent.iterdir()] == [dest.name]
    t = at.open_audio(dest).tags
    if kind in ("aac", "alac"):
        assert bytes(t["----:com.apple.iTunes:MusicBrainz Track Id"][0]) == REC1.encode()
    else:
        assert t["UFID:http://musicbrainz.org"].data == REC1.encode()
        assert t.version == (2, 3, 0)


def test_apply_without_ids_writes_none(at, srv, lib, make_audio):
    srv.handle(apply_req(make_audio("mp3")))
    t = at.open_audio(lib / DEST).tags
    assert not [k for k in t if k.startswith("UFID") or "MusicBrainz" in k]


def test_apply_keeps_tags_the_request_does_not_give_and_builds_the_name_from_them(at, srv, lib, make_audio):
    f = make_audio("mp3")
    at.write_tags(f, {"track_no": 5, "composer": "Me", "artist": "Old artist"})
    ans = srv.handle(apply_req(f, {"title": "Song", "album": "Rec", "artist": "New artist"}))
    assert ans["destination"] == "New artist/Rec/05 Song.mp3"
    got = at.read_tags(at.open_audio(lib / ans["destination"]))
    assert got == {"title": "Song", "album": "Rec", "artist": "New artist", "track_no": 5, "composer": "Me"}


def test_apply_files_a_compilation_under_compilations(at, srv, lib, make_audio):
    f = make_audio("mp3")
    ans = srv.handle(apply_req(f, dict(FULL, album_artist="Various Artists", compilation=True)))
    assert ans["destination"].startswith("Compilations/Hunting High and Low/")
    assert at.read_tags(at.open_audio(lib / ans["destination"]))["compilation"] is True


def test_apply_reuses_a_folder_spelled_differently(at, srv, lib, make_audio):
    existing = lib / unicodedata.normalize("NFD", "Beyoncé") / "LEMONADE"
    existing.mkdir(parents=True)
    f = make_audio("mp3")
    ans = srv.handle(apply_req(f, {"title": "Sorry", "artist": "BEYONCÉ", "album": "Lemonade"}))
    assert (lib / ans["destination"]).parent == existing
    assert sorted(p.name for p in lib.iterdir()) == sorted(
        ["Someone Else", unicodedata.normalize("NFD", "Beyoncé")])


def test_apply_a_second_time_with_the_file_gone_is_an_error(at, srv, lib, make_audio):
    f = make_audio("mp3")
    srv.handle(apply_req(f))
    with pytest.raises(at.Fail) as e:
        srv.handle(apply_req(f))
    assert "no such file" in str(e.value)
    assert (lib / DEST).is_file()


# -- apply: refusals change nothing --------------------------------------------------------

def assert_untouched(lib, snap, before_tree, f):
    snap.assert_unchanged()
    assert tree(lib) == before_tree
    assert temp_files(lib) == [] and temp_files(f.parent) == []


@pytest.mark.parametrize("extra", [{}, {"allow_duplicate": True}, {"dry_run": True}])
def test_refused_when_the_destination_exists_whatever_the_flags(at, srv, lib, make_audio, extra):
    dest = lib / DEST
    dest.parent.mkdir(parents=True)
    dest.write_bytes(b"the file that was there")
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    ans = srv.handle(apply_req(f, **extra))
    assert ans["refused"].startswith("already in the library")
    assert ans["destination"] == DEST
    assert "filed" not in ans
    assert dest.read_bytes() == b"the file that was there"
    assert_untouched(lib, snap, before, f)


def test_refused_when_the_same_title_is_filed_under_the_artist(at, srv, lib, make_audio):
    old = lib / "a-ha" / "Other Album" / "01 Take On Me.mp3"
    old.parent.mkdir(parents=True)
    old.write_bytes(b"old")
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    ans = srv.handle(apply_req(f))
    assert ans["refused"].startswith("same title already filed")
    assert ans["same_title"] == ["a-ha/Other Album/01 Take On Me.mp3"]
    assert ans["destination"] == DEST
    assert old.read_bytes() == b"old"
    assert_untouched(lib, snap, before, f)


def test_dry_run_reports_same_title_without_refusing(at, srv, lib, make_audio):
    old = lib / "a-ha" / "Other Album" / "01 Take On Me.mp3"
    old.parent.mkdir(parents=True)
    old.write_bytes(b"old")
    f = make_audio("mp3")
    before = tree(lib)
    ans = srv.handle(apply_req(f, dry_run=True))
    assert ans == {"destination": DEST, "same_title": ["a-ha/Other Album/01 Take On Me.mp3"],
                   "filed": False}
    assert tree(lib) == before


def test_allow_duplicate_files_it_next_to_the_other_one(at, srv, lib, make_audio):
    old = lib / "a-ha" / "Other Album" / "01 Take On Me.mp3"
    old.parent.mkdir(parents=True)
    old.write_bytes(b"old")
    f = make_audio("mp3")
    ans = srv.handle(apply_req(f, allow_duplicate=True))
    assert ans["filed"] is True and ans["same_title"] == ["a-ha/Other Album/01 Take On Me.mp3"]
    assert (lib / DEST).is_file() and old.read_bytes() == b"old" and not f.exists()


@pytest.mark.parametrize("req_extra, fragment", [
    ({"recording": REC1}, "go together"),
    ({"release": REL1}, "go together"),
    ({"recording": "", "release": REL1}, "non-empty"),
    ({"recording": 5, "release": REL1}, "non-empty"),
    ({"dry_run": "yes"}, "dry_run"),
    ({"allow_duplicate": 1}, "allow_duplicate"),
    ({"dry_run": None}, "dry_run"),
])
def test_bad_apply_requests_are_errors_and_change_nothing(at, srv, lib, make_audio, req_extra, fragment):
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    with pytest.raises(at.Fail) as e:
        srv.handle(apply_req(f, **req_extra))
    assert fragment in str(e.value)
    assert_untouched(lib, snap, before, f)


@pytest.mark.parametrize("tags, fragment", [
    ({"rating": 5}, "unknown tag field"),
    ({"title": 5}, "must be a string"),
    ({"title": "T", "year": True}, "whole number"),
    ({"title": "T", "year": 99999}, "from 1000"),
    ({"title": "T", "track_no": 0}, "from 1"),
    ({"title": "T", "compilation": "no"}, "true or false"),
    (None, "JSON object"),
])
def test_invalid_tags_are_refused_and_change_nothing(at, srv, lib, make_audio, tags, fragment):
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    req = apply_req(f)
    req["tags"] = tags
    with pytest.raises(at.Fail) as e:
        srv.handle(req)
    assert fragment in str(e.value)
    assert_untouched(lib, snap, before, f)


def test_a_missing_tags_key_is_refused(at, srv, make_audio):
    with pytest.raises(at.Fail):
        srv.handle({"op": "apply", "file": str(make_audio("mp3"))})


def test_empty_strings_are_dropped_so_a_titleless_request_is_refused(at, srv, lib, make_audio):
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    with pytest.raises(at.Fail) as e:
        srv.handle(apply_req(f, {"title": "   ", "artist": "A"}))
    assert "title" in str(e.value)
    assert_untouched(lib, snap, before, f)


def test_a_title_already_in_the_file_is_enough(at, srv, lib, make_audio):
    f = make_audio("mp3")
    at.write_tags(f, {"title": "From the file"})
    ans = srv.handle(apply_req(f, {"artist": "A", "title": ""}))
    assert ans["destination"] == "A/Unknown Album/From the file.mp3"


@pytest.mark.parametrize("state", ["absent", "empty"])
def test_apply_refuses_when_the_library_is_not_there(at, tmp_path, make_audio, state):
    root = tmp_path / "gone"
    if state == "empty":
        root.mkdir()
    f = make_audio("mp3")
    snap = Snapshot(f)
    with pytest.raises(at.Fail) as e:
        at.Server(root, "").handle(apply_req(f))
    assert "missing or empty" in str(e.value)
    snap.assert_unchanged()
    assert not root.exists() or list(root.iterdir()) == []


@pytest.mark.parametrize("name", ["a.flac", "a.txt"])
def test_apply_refuses_a_file_that_is_not_supported(at, srv, lib, tmp_path, name):
    f = tmp_path / name
    f.write_bytes(b"data")
    before = tree(lib)
    with pytest.raises(at.Fail) as e:
        srv.handle(apply_req(f))
    assert "not a supported file" in str(e.value)
    assert tree(lib) == before and f.read_bytes() == b"data"


def test_apply_refuses_an_unreadable_audio_file(at, srv, lib, tmp_path):
    f = tmp_path / "broken.mp3"
    f.write_bytes(b"not audio at all" * 10)
    before = tree(lib)
    with pytest.raises(at.Fail):
        srv.handle(apply_req(f))
    assert tree(lib) == before and f.exists()


# -- apply: safety around the write --------------------------------------------------------

def test_the_source_is_never_written_only_a_hidden_copy_in_the_album_folder(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    seen = []
    real = at.write_tags

    def spy(path, new, ids=None):
        seen.append(path)
        assert path != f
        assert path.parent == lib / "a-ha" / "Hunting High and Low"
        assert path.name.startswith(".autotag-")
        return real(path, new, ids)

    monkeypatch.setattr(at, "write_tags", spy)
    srv.handle(apply_req(f))
    assert len(seen) == 1


def test_a_failure_inside_write_tags_leaves_the_source_and_no_temp_file(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    snap = Snapshot(f)
    files_before = [p for p in tree(lib) if p[1] != -1]
    real = at.write_tags

    def tag_then_fail(path, new, ids=None):
        real(path, new, ids)           # the copy really was tagged...
        raise RuntimeError("disk on fire")

    monkeypatch.setattr(at, "write_tags", tag_then_fail)
    with pytest.raises(RuntimeError, match="disk on fire"):
        srv.handle(apply_req(f))
    snap.assert_unchanged()
    assert at.read_tags(at.open_audio(f)) == {}  # nothing of the request reached the source
    assert temp_files(lib) == [] and temp_files(f.parent) == []
    assert not (lib / DEST).exists()
    assert [p for p in tree(lib) if p[1] != -1] == files_before


def test_a_failure_while_copying_leaves_the_source_and_no_temp_file(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    snap = Snapshot(f)

    def broken_copy(src, dst, **kw):
        pathlib.Path(dst).write_bytes(b"half")
        raise OSError(errno.ENOSPC, "No space left on device")

    monkeypatch.setattr(at.shutil, "copy2", broken_copy)
    with pytest.raises(OSError):
        srv.handle(apply_req(f))
    snap.assert_unchanged()
    assert temp_files(lib) == [] and not (lib / DEST).exists()


def test_a_failure_in_the_fsync_leaves_the_source_and_no_temp_file(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    snap = Snapshot(f)
    monkeypatch.setattr(at, "fsync_file", lambda p: (_ for _ in ()).throw(OSError(errno.EIO, "io")))
    with pytest.raises(OSError):
        srv.handle(apply_req(f))
    snap.assert_unchanged()
    assert temp_files(lib) == [] and not (lib / DEST).exists()


def record_events(at, monkeypatch, f, dest):
    """Log fsyncs, the link and the source's deletion, in order."""
    events = []
    real = {"fsync_dir": at.fsync_dir, "fsync_file": at.fsync_file, "link_into": at.link_into,
            "unlink": pathlib.Path.unlink}

    def fsync_dir(path):
        events.append(("fsync_dir", pathlib.Path(path), dest.is_file(), f.exists()))
        return real["fsync_dir"](path)

    def fsync_file(path):
        # The copy must already carry the tags when it is synced.
        events.append(("fsync_file", pathlib.Path(path).name.startswith(".autotag-"),
                       at.read_tags(at.open_audio(pathlib.Path(path))).get("title")))
        return real["fsync_file"](path)

    def link_into(tmp, d):
        events.append(("link", dest.exists()))
        return real["link_into"](tmp, d)

    def unlink(self, *a, **k):
        if self == f:
            events.append(("unlink_source", dest.is_file(), f.exists()))
        return real["unlink"](self, *a, **k)

    monkeypatch.setattr(at, "fsync_dir", fsync_dir)
    monkeypatch.setattr(at, "fsync_file", fsync_file)
    monkeypatch.setattr(at, "link_into", link_into)
    monkeypatch.setattr(pathlib.Path, "unlink", unlink)
    return events


def test_the_source_is_deleted_only_after_everything_is_on_disk(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    dest = lib / DEST
    events = record_events(at, monkeypatch, f, dest)
    srv.handle(apply_req(f))
    # The temp file is synced (tagged) before the link; then the album folder, and the
    # parent of each folder that was created (the artist's folder in the library, the
    # album's in the artist folder); only then does the source go.
    assert events[0] == ("fsync_file", True, "Take On Me")
    assert events[1] == ("link", False)
    synced = [e for e in events if e[0] == "fsync_dir"]
    assert [e[1] for e in synced] == [dest.parent, lib, dest.parent.parent]
    assert all(e[2] and e[3] for e in synced)        # dest there, source still there
    assert events[-1] == ("unlink_source", True, True)
    assert events.index(events[-1]) > max(events.index(e) for e in synced)
    assert not f.exists()


def test_only_the_new_folders_parents_are_synced(at, srv, lib, make_audio, monkeypatch):
    (lib / "a-ha" / "Other Album").mkdir(parents=True)
    f = make_audio("mp3")
    dest = lib / DEST
    events = record_events(at, monkeypatch, f, dest)
    srv.handle(apply_req(f))
    assert [e[1] for e in events if e[0] == "fsync_dir"] == [dest.parent, lib / "a-ha"]
    assert events[-1][0] == "unlink_source"


def test_the_temp_file_is_synced_before_the_retag_replaces_the_file(at, net, monkeypatch, capsys, lib, tmp_path, long_mp3):
    f = tmp_path / "inbox" / "a-ha - Take On Me.mp3"
    f.parent.mkdir(exist_ok=True)
    f.write_bytes(long_mp3.read_bytes())
    events = []
    real_fsync, real_replace = at.fsync_file, at.os.replace
    monkeypatch.setattr(at, "fsync_file", lambda p: events.append("fsync") or real_fsync(p))
    monkeypatch.setattr(at.os, "replace", lambda a, b: events.append("replace") or real_replace(a, b))
    at.retag(f, {"title": "x"})
    assert events == ["fsync", "replace"]


def test_a_taken_link_is_a_refusal_and_cleans_up(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    snap = Snapshot(f)

    def lost_race(tmp, dest):
        raise at.Taken(f"already in the library: {dest}")

    monkeypatch.setattr(at, "link_into", lost_race)
    ans = srv.handle(apply_req(f))
    assert ans["refused"].startswith("already in the library")
    assert ans["destination"] == DEST and "filed" not in ans
    snap.assert_unchanged()
    assert temp_files(lib) == [] and not (lib / DEST).exists()
    assert not (lib / "a-ha").exists()


def test_any_other_failure_from_the_link_is_an_error_not_a_refusal(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    snap = Snapshot(f)

    def odd(tmp, dest):
        raise at.Fail("something else")

    monkeypatch.setattr(at, "link_into", odd)
    with pytest.raises(at.Fail) as e:
        srv.handle(apply_req(f))
    assert not isinstance(e.value, at.Taken)
    snap.assert_unchanged()
    assert temp_files(lib) == [] and not (lib / "a-ha").exists()


def test_a_failure_on_the_tagged_copy_is_an_error_not_a_refusal(at, srv, lib, make_audio, monkeypatch, capsys):
    f = make_audio("mp3")
    snap = Snapshot(f)
    monkeypatch.setattr(at, "write_tags", lambda *a, **k: (_ for _ in ()).throw(at.Fail("cannot read the copy")))
    _, answers, _ = run_serve(at, monkeypatch, capsys, lib, [apply_req(f)])
    assert list(answers[0]) == ["error"] and "cannot read the copy" in answers[0]["error"]
    snap.assert_unchanged()
    assert temp_files(lib) == [] and not (lib / "a-ha").exists()


def test_a_dest_created_after_the_check_is_never_replaced(at, srv, lib, make_audio, monkeypatch):
    # Another process files the same name between the exists() check and the link.
    f = make_audio("mp3")
    dest = lib / DEST
    real_copy = at.shutil.copy2

    def copy_then_rival(src, dst, **kw):
        real_copy(src, dst, **kw)
        dest.write_bytes(b"rival")

    monkeypatch.setattr(at.shutil, "copy2", copy_then_rival)
    ans = srv.handle(apply_req(f))
    assert ans["refused"].startswith("already in the library")
    assert dest.read_bytes() == b"rival"
    assert f.exists() and temp_files(lib) == []


# -- link_into -----------------------------------------------------------------------------

def test_link_into_gives_the_temp_file_its_name(at, tmp_path):
    tmp, dest = tmp_path / ".autotag-x.mp3", tmp_path / "song.mp3"
    tmp.write_bytes(b"data")
    at.link_into(tmp, dest)
    assert dest.read_bytes() == b"data" and not tmp.exists()
    assert dest.stat().st_nlink == 1


def test_link_into_never_replaces_an_existing_file(at, tmp_path):
    tmp, dest = tmp_path / ".autotag-x.mp3", tmp_path / "song.mp3"
    tmp.write_bytes(b"new")
    dest.write_bytes(b"old")
    with pytest.raises(at.Fail) as e:
        at.link_into(tmp, dest)
    assert "already in the library" in str(e.value) and str(dest) in str(e.value)
    assert dest.read_bytes() == b"old"
    assert tmp.read_bytes() == b"new"        # the caller cleans the temp file up


def test_link_into_when_link_says_file_exists(at, tmp_path, monkeypatch):
    tmp, dest = tmp_path / ".autotag-x.mp3", tmp_path / "song.mp3"
    tmp.write_bytes(b"new")
    monkeypatch.setattr(at.os, "link", lambda a, b: (_ for _ in ()).throw(FileExistsError()))
    with pytest.raises(at.Fail):
        at.link_into(tmp, dest)
    assert not dest.exists() and tmp.exists()   # no fallback rename after FileExistsError


@pytest.mark.parametrize("err", [OSError(errno.EPERM, "no links here"), OSError(errno.EXDEV, "x"),
                                 PermissionError(errno.EACCES, "x"), OSError(errno.EOPNOTSUPP, "x")])
def test_link_into_falls_back_to_rename_when_there_are_no_hard_links(at, tmp_path, monkeypatch, err):
    tmp, dest = tmp_path / ".autotag-x.mp3", tmp_path / "song.mp3"
    tmp.write_bytes(b"data")
    monkeypatch.setattr(at.os, "link", lambda a, b: (_ for _ in ()).throw(err))
    at.link_into(tmp, dest)
    assert dest.read_bytes() == b"data" and not tmp.exists()


def test_link_into_fallback_still_refuses_an_existing_destination(at, tmp_path, monkeypatch):
    tmp, dest = tmp_path / ".autotag-x.mp3", tmp_path / "song.mp3"
    tmp.write_bytes(b"new")
    dest.write_bytes(b"old")
    monkeypatch.setattr(at.os, "link", lambda a, b: (_ for _ in ()).throw(OSError(errno.EPERM, "x")))
    with pytest.raises(at.Fail) as e:
        at.link_into(tmp, dest)
    assert "already in the library" in str(e.value)
    assert dest.read_bytes() == b"old" and tmp.read_bytes() == b"new"


def test_apply_works_through_the_no_hard_link_fallback(at, srv, lib, make_audio, monkeypatch):
    monkeypatch.setattr(at.os, "link", lambda a, b: (_ for _ in ()).throw(OSError(errno.EPERM, "x")))
    f = make_audio("mp3")
    ans = srv.handle(apply_req(f))
    assert ans["filed"] is True and (lib / DEST).is_file() and not f.exists()
    assert temp_files(lib) == []
    assert at.read_tags(at.open_audio(lib / DEST))["title"] == "Take On Me"


# -- serve(): the loop ---------------------------------------------------------------------

def run_serve(at, monkeypatch, capsys, lib, lines, key=""):
    monkeypatch.setattr(sys, "stdout", sys.stdout)   # serve() swaps it; restore afterwards
    monkeypatch.setattr(sys, "stdin", io.TextIOWrapper(io.BytesIO(
        "".join(l if isinstance(l, str) else json.dumps(l) + "\n" for l in lines).encode())))
    monkeypatch.setattr(at, "acoustid_key", lambda: key)
    assert at.serve(lib) == 0
    cap = capsys.readouterr()
    out = [json.loads(l) for l in cap.out.splitlines()]   # every stdout line must be JSON
    return out[0], out[1:], cap.err


def test_serve_says_hello_first_and_ends_at_eof(at, monkeypatch, capsys, lib):
    monkeypatch.setattr(at.shutil, "which", lambda n: "/usr/bin/fpcalc")
    hello, answers, err = run_serve(at, monkeypatch, capsys, lib, [])
    assert hello == {"protocol": 1, "formats": ["mp3", "m4a", "wav", "aiff"],
                     "acoustid": False, "fpcalc": True}
    assert answers == []


def test_serve_hello_reports_the_key_and_fpcalc(at, monkeypatch, capsys, lib):
    monkeypatch.setattr(at.shutil, "which", lambda n: None)
    hello, _, _ = run_serve(at, monkeypatch, capsys, lib, [], key="K")
    assert hello["acoustid"] is True and hello["fpcalc"] is False


def test_serve_echoes_the_id_on_answers_and_errors_only_when_given(at, monkeypatch, capsys, lib):
    _, answers, _ = run_serve(at, monkeypatch, capsys, lib, [
        {"op": "nope", "id": 7}, {"op": "nope"}, {"op": "nope", "id": "abc"}, {"op": "nope", "id": None}])
    assert [a.get("id", "absent") for a in answers] == [7, "absent", "abc", None]
    assert all("error" in a for a in answers)


def test_serve_survives_bad_lines_and_keeps_answering(at, net, monkeypatch, capsys, lib, make_audio):
    f = make_audio("mp3")
    _, answers, _ = run_serve(at, monkeypatch, capsys, lib, [
        "this is not json\n", "[1, 2]\n", "\n", "   \n", '"text"\n', "null\n",
        {"op": "nope", "id": 1}, apply_req(f, dry_run=True) | {"id": 2}])
    assert len(answers) == 6          # blank lines get no answer
    assert all("error" in a for a in answers[:5])
    assert "not JSON" in answers[0]["error"]
    assert all("JSON object" in answers[i]["error"] for i in (1, 2, 3))   # [1, 2], "text", null
    assert "unknown op" in answers[4]["error"] and answers[4]["id"] == 1
    assert answers[5] == {"destination": DEST, "same_title": [], "filed": False, "id": 2}


def test_serve_answers_a_failed_request_with_an_error_and_goes_on(at, monkeypatch, capsys, lib, make_audio):
    f = make_audio("mp3")
    snap = Snapshot(f)
    _, answers, _ = run_serve(at, monkeypatch, capsys, lib, [
        {"op": "apply", "file": str(f), "tags": {"rating": 1}, "id": 1},
        {"op": "apply", "file": str(f), "tags": FULL, "dry_run": True, "id": 2}])
    assert "unknown tag field" in answers[0]["error"] and answers[0]["id"] == 1
    assert answers[1]["filed"] is False
    snap.assert_unchanged()


def test_serve_turns_an_unexpected_exception_into_an_error_and_goes_on(at, monkeypatch, capsys, lib, make_audio):
    f = make_audio("mp3")
    snap = Snapshot(f)
    real = at.place
    boom = {"on": True}

    def flaky(*a, **k):
        if boom["on"]:
            boom["on"] = False
            raise ZeroDivisionError("kaboom")
        return real(*a, **k)

    monkeypatch.setattr(at, "place", flaky)
    _, answers, err = run_serve(at, monkeypatch, capsys, lib, [
        apply_req(f, dry_run=True) | {"id": 1}, apply_req(f, dry_run=True) | {"id": 2}])
    assert answers[0]["error"].startswith("internal error") and "kaboom" in answers[0]["error"]
    assert answers[0]["id"] == 1
    assert answers[1] == {"destination": DEST, "same_title": [], "filed": False, "id": 2}
    assert "ZeroDivisionError" in err and "Traceback" in err    # the trace is on stderr
    snap.assert_unchanged()


def test_serve_prints_stray_output_to_stderr_not_stdout(at, net, monkeypatch, capsys, lib, make_audio, take_on_me):
    # With a key and no fpcalc, fingerprint_ids prints a notice; stdout must stay JSON only.
    monkeypatch.setattr(at.shutil, "which", lambda n: None)
    f = take_on_me
    _, answers, err = run_serve(at, monkeypatch, capsys, lib,
                                [{"op": "lookup", "file": str(f), "id": 1}], key="KEY")
    assert answers[0]["by"] == "name" and answers[0]["id"] == 1
    assert "fpcalc not found" in err


def test_serve_handles_unicode_paths(at, monkeypatch, capsys, lib, make_audio):
    name = unicodedata.normalize("NFD", "Beyoncé – Déjà vu.mp3")
    f = make_audio("mp3", name=name)
    _, answers, _ = run_serve(at, monkeypatch, capsys, lib, [
        apply_req(f, {"title": "Déjà vu", "artist": "Beyoncé", "album": "B’Day"}, dry_run=True)])
    assert answers[0]["destination"] == "Beyoncé/B’Day/Déjà vu.mp3"


def test_serve_files_a_song_end_to_end(at, monkeypatch, capsys, lib, make_audio):
    f = make_audio("aac")
    _, answers, _ = run_serve(at, monkeypatch, capsys, lib, [apply_req(f)])
    dest = lib / "a-ha" / "Hunting High and Low" / "03 Take On Me.m4a"
    assert answers == [{"destination": str(dest.relative_to(lib)), "same_title": [], "filed": True}]
    assert dest.is_file() and not f.exists()


# -- a failed apply leaves no new folders -------------------------------------------------

def inject(monkeypatch, at, where):
    """Make `where` ("write_tags" or "copy2") fail after the folders exist."""
    def boom(*a, **k):
        raise RuntimeError("disk on fire")
    if where == "write_tags":
        monkeypatch.setattr(at, "write_tags", boom)
    else:
        monkeypatch.setattr(at.shutil, "copy2", boom)


@pytest.mark.parametrize("where", ["write_tags", "copy2"])
def test_a_failure_removes_the_new_artist_and_album_folders(at, srv, lib, make_audio, monkeypatch, where):
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    inject(monkeypatch, at, where)
    with pytest.raises(RuntimeError):
        srv.handle(apply_req(f))
    assert not (lib / "a-ha").exists()
    assert tree(lib) == before
    snap.assert_unchanged()


def test_a_failure_keeps_an_existing_artist_folder_when_only_the_album_was_new(at, srv, lib, make_audio, monkeypatch):
    other = lib / "a-ha" / "Other Album" / "01 Another Song.mp3"
    other.parent.mkdir(parents=True)
    other.write_bytes(b"x")
    f = make_audio("mp3")
    before = tree(lib)
    inject(monkeypatch, at, "write_tags")
    with pytest.raises(RuntimeError):
        srv.handle(apply_req(f))
    assert (lib / "a-ha").is_dir() and not (lib / "a-ha" / "Hunting High and Low").exists()
    assert tree(lib) == before


def test_a_failure_keeps_an_existing_album_folder_and_its_files(at, srv, lib, make_audio, monkeypatch):
    other = lib / "a-ha" / "Hunting High and Low" / "01 Another Song.mp3"
    other.parent.mkdir(parents=True)
    other.write_bytes(b"x")
    before = tree(lib)
    inject(monkeypatch, at, "copy2")
    with pytest.raises(RuntimeError):
        srv.handle(apply_req(make_audio("mp3")))
    assert tree(lib) == before


def test_a_folder_that_is_no_longer_empty_is_left_alone(at, srv, lib, make_audio, monkeypatch):
    # Something else files into the new album folder while the copy is being made.
    album = lib / "a-ha" / "Hunting High and Low"
    f = make_audio("mp3")

    def meanwhile(*a, **k):
        (album / "01 Filed Meanwhile.mp3").write_bytes(b"theirs")
        raise RuntimeError("disk on fire")

    monkeypatch.setattr(at, "write_tags", meanwhile)
    with pytest.raises(RuntimeError):
        srv.handle(apply_req(f))
    assert (album / "01 Filed Meanwhile.mp3").read_bytes() == b"theirs"
    assert temp_files(lib) == []


def test_a_failure_making_the_folders_leaves_nothing_behind(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    before = tree(lib)
    monkeypatch.setattr(at.tempfile, "mkstemp", lambda **k: (_ for _ in ()).throw(OSError(errno.ENOSPC, "full")))
    with pytest.raises(OSError):
        srv.handle(apply_req(f))
    assert tree(lib) == before


# -- a destination spelled another way is the same file ----------------------------------

def nfd(x):
    return unicodedata.normalize("NFD", x)


@pytest.mark.parametrize("existing", ["03 TAKE ON ME.mp3", "03 take on me.MP3".replace(".MP3", ".mp3")])
def test_a_file_in_another_case_is_the_destination_and_is_taken(at, srv, lib, make_audio, existing):
    album = lib / "a-ha" / "Hunting High and Low"
    album.mkdir(parents=True)
    (album / existing).write_bytes(b"already here")
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    for extra in ({"allow_duplicate": True}, {"dry_run": True}, {}):
        ans = srv.handle(apply_req(f, **extra))
        assert ans["refused"].startswith("already in the library")
        assert ans["destination"] == f"a-ha/Hunting High and Low/{existing}"
        assert "filed" not in ans
    assert tree(lib) == before and (album / existing).read_bytes() == b"already here"
    snap.assert_unchanged()


def test_a_file_in_another_unicode_form_is_taken(at, srv, lib, make_audio):
    album = lib / "Beyoncé" / "B’Day"
    album.mkdir(parents=True)
    old = album / (nfd("Déjà vu") + ".mp3")
    old.write_bytes(b"already here")
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    ans = srv.handle(apply_req(f, {"title": "Déjà vu", "artist": "Beyoncé", "album": "B’Day"},
                               allow_duplicate=True))
    assert ans["refused"].startswith("already in the library")
    assert ans["destination"] == str(old.relative_to(lib))       # the name on disk, NFD
    assert [p.name for p in album.iterdir()] == [old.name]       # still one file
    assert tree(lib) == before
    snap.assert_unchanged()


def test_the_cli_says_already_in_the_library_for_a_differently_spelled_file(at, monkeypatch, capsys, lib, make_audio):
    from test_cli import run
    album = lib / "a-ha" / "Hunting High and Low"
    album.mkdir(parents=True)
    (album / "03 take on me.mp3").write_bytes(b"already here")
    f = make_audio("mp3")
    at.write_tags(f, {"title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low", "track_no": 3})
    snap = Snapshot(f)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f)
    assert code == 1 and "already in the library" in out
    assert [p.name for p in album.iterdir()] == ["03 take on me.mp3"]
    snap.assert_unchanged()


# -- read-only sources -------------------------------------------------------------------

def test_a_read_only_source_is_filed_and_the_copy_is_writable(at, srv, lib, make_audio):
    f = make_audio("mp3")
    f.chmod(0o444)
    ans = srv.handle(apply_req(f))
    dest = lib / DEST
    assert ans["filed"] is True and not f.exists()
    assert at.read_tags(at.open_audio(dest))["title"] == "Take On Me"
    assert dest.stat().st_mode & 0o200            # the owner can write the filed file


def test_a_refused_read_only_source_is_unchanged_mode_included(at, srv, lib, make_audio):
    dest = lib / DEST
    dest.parent.mkdir(parents=True)
    dest.write_bytes(b"there")
    f = make_audio("mp3")
    f.chmod(0o444)
    snap = Snapshot(f)
    assert "refused" in srv.handle(apply_req(f))
    snap.assert_unchanged()
    assert f.stat().st_mode & 0o777 == 0o444


def test_the_cli_retags_a_read_only_file(at, net, monkeypatch, capsys, lib, tmp_path, long_mp3):
    from test_cli import run
    f = tmp_path / "inbox" / "a-ha - Take On Me.mp3"
    f.parent.mkdir(exist_ok=True)
    f.write_bytes(long_mp3.read_bytes())
    f.chmod(0o444)
    code, out = run(at, monkeypatch, capsys, "-y", "--no-export", "--library", lib, f)
    assert code == 0
    assert at.read_tags(at.open_audio(f))["title"] == "Take On Me"
    assert temp_files(f.parent) == []


# -- order of matches --------------------------------------------------------------------

def test_same_title_files_are_sorted(at, srv, lib, make_audio):
    for album in ("Zeta", "Mu", "Alpha"):                 # created in reverse order
        touch_song(lib / "a-ha" / album / f"0{len(album)} Take On Me.mp3")
    expected = ["a-ha/Alpha/05 Take On Me.mp3", "a-ha/Mu/02 Take On Me.mp3", "a-ha/Zeta/04 Take On Me.mp3"]
    assert [str(p.relative_to(lib)) for p in at.same_song(lib / "a-ha", "Take On Me")] == expected
    ans = srv.handle(apply_req(make_audio("mp3"), dry_run=True))
    assert ans["same_title"] == expected


@pytest.mark.parametrize("order", [("Band", "BAND"), ("BAND", "Band")])
def test_two_folders_that_fold_alike_give_the_sorted_first_whatever_the_creation_order(at, tmp_path, order):
    for name in order:
        (tmp_path / name).mkdir()
    got, exists = at.find_or_new(tmp_path, "band")
    assert exists and got.name == "BAND"          # "BAND" sorts before "Band"


def touch_song(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"x")


def test_a_title_that_starts_with_a_number_is_found_with_or_without_a_track(at, srv, lib, make_audio):
    plain = lib / "Nena" / "99 Luftballons" / "99 Luftballons.mp3"
    numbered = lib / "Nena" / "Other" / "01 99 Luftballons.mp3"
    touch_song(plain)
    touch_song(numbered)
    assert at.same_song(lib / "Nena", "99 Luftballons") == sorted([plain, numbered])
    f = make_audio("mp3")
    ans = srv.handle(apply_req(f, {"title": "99 Luftballons", "artist": "Nena", "album": "New", "track_no": 2}))
    assert ans["refused"].startswith("same title already filed")
    assert ans["same_title"] == sorted(str(p.relative_to(lib)) for p in (plain, numbered))


# -- link_into and a dangling symlink ------------------------------------------------------

@pytest.mark.parametrize("no_hard_links", [False, True])
def test_a_dangling_symlink_at_the_destination_counts_as_taken(at, tmp_path, monkeypatch, no_hard_links):
    tmp, dest = tmp_path / ".autotag-x.mp3", tmp_path / "song.mp3"
    tmp.write_bytes(b"new")
    dest.symlink_to(tmp_path / "nowhere")
    if no_hard_links:
        monkeypatch.setattr(at.os, "link", lambda a, b: (_ for _ in ()).throw(OSError(errno.EPERM, "x")))
    with pytest.raises(at.Taken):
        at.link_into(tmp, dest)
    assert dest.is_symlink() and not dest.exists()      # not replaced, not followed
    assert tmp.read_bytes() == b"new"


def test_taken_is_a_fail(at):
    assert issubclass(at.Taken, at.Fail)


# -- the source cannot be deleted after the link ----------------------------------------

def test_a_source_that_cannot_be_deleted_leaves_the_file_filed_and_the_next_apply_refused(
        at, lib, make_audio, monkeypatch, capsys):
    f = make_audio("mp3")
    real = pathlib.Path.unlink

    def unlink(self, *a, **k):
        if self == f:
            raise PermissionError(errno.EACCES, "cannot delete")
        return real(self, *a, **k)

    monkeypatch.setattr(pathlib.Path, "unlink", unlink)
    _, answers, err = run_serve(at, monkeypatch, capsys, lib, [apply_req(f), apply_req(f)])
    assert list(answers[0]) == ["error"] and "internal error" in answers[0]["error"]
    assert (lib / DEST).is_file() and f.exists()
    assert at.read_tags(at.open_audio(lib / DEST))["title"] == "Take On Me"
    assert answers[1]["refused"].startswith("already in the library")
    assert temp_files(lib) == []


@pytest.mark.parametrize("order", [("03 TAKE ON ME.mp3", "03 take on me.mp3"), ("03 take on me.mp3", "03 TAKE ON ME.mp3")])
def test_with_two_spellings_on_disk_the_sorted_first_is_the_destination(at, srv, lib, make_audio, order):
    album = lib / "a-ha" / "Hunting High and Low"
    album.mkdir(parents=True)
    for name in order:
        (album / name).write_bytes(b"x")
    ans = srv.handle(apply_req(make_audio("mp3"), dry_run=True))
    assert ans["destination"] == "a-ha/Hunting High and Low/03 TAKE ON ME.mp3"


# -- retag keeps the file's mode; a dangling symlink is taken everywhere --------------------

@pytest.mark.parametrize("mode", [0o444, 0o644, 0o600])
def test_the_cli_retag_keeps_the_files_mode(at, net, monkeypatch, capsys, lib, tmp_path, long_mp3, mode):
    from test_cli import run
    f = tmp_path / "inbox" / "a-ha - Take On Me.mp3"
    f.parent.mkdir(exist_ok=True)
    f.write_bytes(long_mp3.read_bytes())
    f.chmod(mode)
    code, out = run(at, monkeypatch, capsys, "-y", "--no-export", "--library", lib, f)
    assert code == 0 and at.read_tags(at.open_audio(f))["title"] == "Take On Me"
    assert f.stat().st_mode & 0o777 == mode
    assert temp_files(f.parent) == []


@pytest.mark.parametrize("extra", [{}, {"dry_run": True}, {"allow_duplicate": True}])
def test_a_dangling_symlink_at_the_destination_is_refused_even_in_a_dry_run(at, srv, lib, make_audio, extra):
    dest = lib / DEST
    dest.parent.mkdir(parents=True)
    dest.symlink_to(lib / "nowhere")
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    ans = srv.handle(apply_req(f, **extra))
    assert ans["refused"].startswith("already in the library")
    assert ans["destination"] == DEST and "filed" not in ans
    assert dest.is_symlink() and not dest.exists()
    assert tree(lib) == before
    snap.assert_unchanged()


@pytest.mark.parametrize("flags", [[], ["-n"]])
def test_the_cli_reports_a_dangling_symlink_as_already_in_the_library(at, monkeypatch, capsys, lib, make_audio, flags):
    from test_cli import run
    dest = lib / DEST
    dest.parent.mkdir(parents=True)
    dest.symlink_to(lib / "nowhere")
    f = make_audio("mp3")
    at.write_tags(f, {"title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low", "track_no": 3})
    snap = Snapshot(f)
    code, out = run(at, monkeypatch, capsys, "--export-only", *flags, "--library", lib, f)
    assert code == 1 and "already in the library" in out and "would" not in out
    assert dest.is_symlink() and not dest.exists()
    assert [p.name for p in dest.parent.iterdir()] == [dest.name]
    snap.assert_unchanged()


def test_the_race_refusal_has_the_same_text_as_the_early_one_and_no_absolute_path(at, srv, lib, make_audio, monkeypatch):
    f = make_audio("mp3")
    dest = lib / DEST
    real_copy = at.shutil.copy2

    def copy_then_rival(src, dst, **kw):
        real_copy(src, dst, **kw)
        dest.write_bytes(b"rival")

    monkeypatch.setattr(at.shutil, "copy2", copy_then_rival)
    race = srv.handle(apply_req(f))
    assert race["refused"] == f"already in the library: {DEST}"
    assert str(lib) not in race["refused"] and str(f.parent) not in race["refused"]
    early = srv.handle(apply_req(make_audio("mp3")))        # the destination now exists
    assert early["refused"] == race["refused"]
