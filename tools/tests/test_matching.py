"""Candidate ranking, the 10-second rule, proposed tags, fingerprints.

MusicBrainz and AcoustID answers come from tools/tests/fixtures (recorded or
shaped like the recorded one). Nothing reaches the network.
"""

import copy
import subprocess
import types

import pytest

from conftest import REC1, REC2, REC3, REL1, REL2, REL3, fixture_json


def recs():
    return copy.deepcopy(fixture_json("mb-search-take-on-me.json")["recordings"])


def release(**kw):
    """A small release in MusicBrainz's shape."""
    base = {"id": "rel", "title": "Album", "status": "Official", "count": 1,
            "date": "2000-01-01", "artist-credit": [{"name": "Band"}],
            "release-group": {"id": "rg", "primary-type": "Album"},
            "media": [{"position": 1, "track-count": 10, "track": [{"number": "4"}]}]}
    base.update(kw)
    return base


def recording(**kw):
    base = {"id": "rec", "title": "Song", "score": 100, "length": 200000,
            "artist-credit": [{"name": "Band", "artist": {"id": "artist-id"}}],
            "releases": [release()]}
    base.update(kw)
    return base


# -- release_rank -----------------------------------------------------------------

def test_official_beats_anything_else(at):
    assert at.release_rank(release(status="Official")) < at.release_rank(release(status="Bootleg"))
    assert at.release_rank(release(status="Official")) < at.release_rank(release(status=None))


def test_a_compilation_live_or_remix_loses_to_a_plain_release(at):
    plain = release()
    for bad in ("Compilation", "Live", "Remix", "DJ-mix", "Mixtape/Street", "Demo"):
        rg = {"primary-type": "Album", "secondary-types": [bad]}
        assert at.release_rank(plain) < at.release_rank(release(**{"release-group": rg})), bad


def test_a_secondary_type_that_is_not_bad_does_not_count(at):
    rg = {"primary-type": "Album", "secondary-types": ["Soundtrack"]}
    assert at.release_rank(release(**{"release-group": rg})) == at.release_rank(release())


@pytest.mark.parametrize("better, worse", [("Album", "EP"), ("EP", "Single"), ("Single", "Other"),
                                           ("Single", None)])
def test_release_types_in_order(at, better, worse):
    a = release(**{"release-group": {"primary-type": better}})
    b = release(**{"release-group": {"primary-type": worse}})
    assert at.release_rank(a) < at.release_rank(b)


def test_the_earlier_date_wins_and_no_date_is_last(at):
    assert at.release_rank(release(date="1985")) < at.release_rank(release(date="1999-01-01"))
    assert at.release_rank(release(date="2030")) < at.release_rank(release(date=None))
    assert at.release_rank(release(date="2030")) < at.release_rank(release(date=""))


def test_status_outranks_type_and_date(at):
    official_late_single = release(date="2020", **{"release-group": {"primary-type": "Single"}})
    bootleg_early_album = release(status="Bootleg", date="1980")
    assert at.release_rank(official_late_single) < at.release_rank(bootleg_early_album)


# -- candidates --------------------------------------------------------------------

def test_candidates_order_on_the_recorded_answer(at):
    # Recording 1 is 0.09 s from the file; the other two are far off, and of
    # those the live release (2001) comes before the DJ mix (2012): the
    # same ranking tuple, so the earlier date decides.
    got = at.candidates(recs(), 217.9, False)
    assert [c.rec["id"] for c in got] == [REC1, REC3, REC2]
    assert [c.rel["id"] for c in got] == [REL1, REL3, REL2]
    assert got[0].length_diff == pytest.approx(0.006, abs=0.001)
    assert got[1].length_diff == float("inf")
    assert got[2].length_diff == pytest.approx(98.1, abs=0.1)


def test_a_far_length_goes_after_a_near_one_whatever_the_release(at):
    near_bad = recording(id="near", length=200000, releases=[release(status="Bootleg", date="2020")])
    far_good = recording(id="far", length=260000, releases=[release(date="1970")])
    got = at.candidates([far_good, near_bad], 200.0, False)
    assert [c.rec["id"] for c in got] == ["near", "far"]


def test_within_the_near_group_the_better_release_wins_then_the_closer_length(at):
    a = recording(id="a", length=201000, releases=[release(date="1990")])
    b = recording(id="b", length=200500, releases=[release(date="1990")])
    c = recording(id="c", length=200000, releases=[release(date="2005")])
    got = at.candidates([c, a, b], 200.0, False)
    assert [x.rec["id"] for x in got] == ["b", "a", "c"]


def test_the_ten_second_edge(at):
    on = recording(id="on", length=210000)
    off = recording(id="off", length=210001, releases=[release(date="1970")])
    got = at.candidates([off, on], 200.0, False)
    assert [c.rec["id"] for c in got] == ["on", "off"]  # 10.000 s is near, 10.001 s is far


def test_a_low_score_is_dropped_by_name_but_kept_by_fingerprint(at):
    r = [recording(id="low", score=69), recording(id="ok", score=70)]
    assert [c.rec["id"] for c in at.candidates(r, 200.0, False)] == ["ok"]
    assert sorted(c.rec["id"] for c in at.candidates(r, 200.0, True)) == ["low", "ok"]


def test_a_recording_with_no_usable_release_is_dropped(at):
    no_media = recording(id="nomedia", releases=[release(media=[])])
    no_media_key = recording(id="nokey", releases=[{k: v for k, v in release().items() if k != "media"}])
    none = recording(id="none", releases=[])
    missing = {k: v for k, v in recording(id="missing").items() if k != "releases"}
    assert at.candidates([no_media, no_media_key, none, missing], 200.0, True) == []


def test_the_best_release_of_a_recording_is_picked(at):
    rec = recording(releases=[release(id="live", **{"release-group": {"primary-type": "Album",
                                                                    "secondary-types": ["Live"]}}),
                              release(id="plain"),
                              release(id="boot", status="Bootleg")])
    assert at.candidates([rec], 200.0, True)[0].rel["id"] == "plain"


def test_a_release_without_media_is_ignored_in_favour_of_one_with(at):
    rec = recording(releases=[release(id="empty", media=[], date="1960"), release(id="full")])
    assert at.candidates([rec], 200.0, True)[0].rel["id"] == "full"


def test_a_recording_without_a_length_is_far(at):
    rec = {k: v for k, v in recording().items() if k != "length"}
    c = at.candidates([rec], 200.0, False)[0]
    assert c.length_diff == float("inf")
    assert not at.certain(c)


# -- certain -----------------------------------------------------------------------

def make_candidate(at, by_fp, diff):
    return at.Candidate(recording(), release(), by_fp, diff)


@pytest.mark.parametrize("by_fp, diff, expected", [
    (False, 0.0, True), (False, 10.0, True), (False, 10.001, False),
    (False, float("inf"), False), (True, 500.0, True), (True, float("inf"), True)])
def test_certain(at, by_fp, diff, expected):
    assert at.certain(make_candidate(at, by_fp, diff)) is expected


# -- proposed ----------------------------------------------------------------------

def one(at, rec=None, rel=None):
    rec = rec or recording()
    if rel is not None:
        rec["releases"] = [rel]
    return at.candidates([rec], 200.0, True)[0]


def test_proposed_from_the_recorded_first_candidate(at, net):
    c = at.candidates(recs(), 217.9, False)[0]
    got = at.proposed(c)
    assert got == {
        "title": "Take On Me", "artist": "a-ha",
        "album": "Die ultimative Chartshow: Die beliebtesten Hits aller Zeiten",
        "album_artist": "Various Artists", "disc_no": 1, "disc_total": 2, "year": 2019,
        "track_no": 7, "track_total": 22, "genre": "Synth-Pop"}


def test_track_number_only_when_the_number_is_digits(at):
    vinyl = release(media=[{"position": 1, "track-count": 5, "track": [{"number": "A1"}]}])
    got = at.proposed(one(at, rel=vinyl), genre=False)
    assert "track_no" not in got and "track_total" not in got
    no_track = release(media=[{"position": 1, "track-count": 5}])
    got = at.proposed(one(at, rel=no_track), genre=False)
    assert "track_no" not in got
    empty = release(media=[{"position": 1, "track-count": 5, "track": [{"number": ""}]}])
    assert "track_no" not in at.proposed(one(at, rel=empty), genre=False)


def test_track_number_and_total_come_from_the_medium(at):
    got = at.proposed(one(at), genre=False)
    assert (got["track_no"], got["track_total"]) == (4, 10)


def test_disc_total_is_the_releases_count_else_the_disc_position(at):
    two = release(count=3, media=[{"position": 2, "track-count": 9, "track": [{"number": "1"}]}])
    got = at.proposed(one(at, rel=two), genre=False)
    assert (got["disc_no"], got["disc_total"]) == (2, 3)
    no_count = {k: v for k, v in two.items() if k != "count"}
    got = at.proposed(one(at, rel=no_count), genre=False)
    assert (got["disc_no"], got["disc_total"]) == (2, 2)


@pytest.mark.parametrize("date, group_date, year", [
    ("1985-05-02", None, 1985), ("1985", "1999", 1985), (None, "1990-04", 1990),
    ("", "1991", 1991), (None, None, None)])
def test_year_from_the_release_date_then_the_group(at, date, group_date, year):
    rg = {"id": "rg", "primary-type": "Album"}
    if group_date:
        rg["first-release-date"] = group_date
    rel = release(date=date, **{"release-group": rg})
    got = at.proposed(one(at, rel=rel), genre=False)
    assert got.get("year") == year
    if year is None:
        assert "year" not in got


def test_album_artist_falls_back_to_the_recording_artist(at):
    rel = release(**{"artist-credit": []})
    assert at.proposed(one(at, rel=rel), genre=False)["album_artist"] == "Band"


def test_artist_credit_joins_names_with_their_join_phrases(at):
    rec = recording(**{"artist-credit": [{"name": "A", "joinphrase": " feat. "}, {"name": "B"}]})
    assert at.proposed(one(at, rec), genre=False)["artist"] == "A feat. B"


def test_genre_false_makes_no_request_and_has_no_genre_key(at, net):
    got = at.proposed(one(at), genre=False)
    assert "genre" not in got
    assert net.calls == []


def test_genre_comes_from_the_recording_first(at, net):
    net.genres = {("recording", "rec"): {"genres": [{"name": "pop", "count": 1},
                                                    {"name": "synth-pop", "count": 4}]}}
    assert at.proposed(one(at))["genre"] == "Synth-Pop"
    assert net.kinds("genre") == [("recording", "rec")]


def test_genre_falls_back_to_the_release_group_then_the_artist(at, net):
    net.genres = {("release-group", "rg"): {"genres": [{"name": "rock", "count": 3}]},
                  ("artist", "artist-id"): {"genres": [{"name": "jazz", "count": 3}]}}
    assert at.proposed(one(at))["genre"] == "Rock"
    assert net.kinds("genre") == [("recording", "rec"), ("release-group", "rg")]

    net.calls.clear()
    net.genres = {("artist", "artist-id"): {"genres": [{"name": "jazz", "count": 3}]}}
    assert at.proposed(one(at))["genre"] == "Jazz"
    assert net.kinds("genre") == [("recording", "rec"), ("release-group", "rg"),
                                  ("artist", "artist-id")]


def test_no_genre_anywhere_leaves_the_key_out(at, net):
    got = at.proposed(one(at))
    assert "genre" not in got
    assert len(net.kinds("genre")) == 3


def test_the_release_group_is_skipped_when_it_has_no_id(at, net):
    rel = release(**{"release-group": {"primary-type": "Album"}})
    at.proposed(one(at, rel=rel))
    assert net.kinds("genre") == [("recording", "rec"), ("artist", "artist-id")]


def test_a_found_genre_stops_the_requests(at, net):
    net.genres = {("recording", "rec"): {"genres": [{"name": "pop", "count": 1}]}}
    at.proposed(one(at))
    assert net.kinds("genre") == [("recording", "rec")]


@pytest.mark.parametrize("raw, name", [
    ("synth-pop", "Synth-Pop"), ("hip hop", "Hip Hop"), ("r&b", "R&B"), ("uk garage", "UK Garage"),
    ("edm", "EDM"), ("drum and bass", "Drum And Bass"), ("idm", "IDM")])
def test_genre_names_are_capitalised(at, raw, name):
    assert at.genre_name(raw) == name


def test_top_genre_takes_the_highest_count_and_empty_is_empty(at, net):
    net.genres = {("recording", "r"): {"genres": [{"name": "a", "count": 1}, {"name": "b", "count": 9},
                                                  {"name": "c", "count": 9}]}}
    assert at.top_genre("recording", "r") == "B"
    assert at.top_genre("recording", "nothing") == ""


@pytest.mark.parametrize("text, year", [
    ("1985", 1985), ("1985-05-02", 1985), ("1985-05-02T07:00:00Z", 1985), (" 2001", 2001),
    ("", None), (None, None), ("abc", None), ("85", None)])
def test_year_of(at, text, year):
    assert at.year_of(text) == year


# -- guess and search ---------------------------------------------------------------

def test_guess_from_a_file_name_drops_youtube_brackets(at, tmp_path):
    p = tmp_path / "a-ha - Take On Me (Official Video).mp3"
    assert at.guess(p, {}) == ("a-ha", "Take On Me")
    p = tmp_path / "Band - Song [HD] (Lyrics).mp3"
    assert at.guess(p, {}) == ("Band", "Song")
    p = tmp_path / "Band - Song (Live in Paris).mp3"
    assert at.guess(p, {}) == ("Band", "Song (Live in Paris)")


def test_guess_falls_back_to_the_tags_when_the_name_has_no_dash(at, tmp_path):
    assert at.guess(tmp_path / "track01.mp3", {"title": "T", "artist": "A"}) == ("A", "T")
    assert at.guess(tmp_path / "track01.mp3", {}) == ("", "track01")


def test_guess_prefers_what_it_is_given(at, tmp_path):
    p = tmp_path / "X - Y.mp3"
    assert at.guess(p, {}, artist="A", title="T") == ("A", "T")
    assert at.guess(p, {}, artist="A") == ("A", "Y")
    assert at.guess(p, {}, title="T") == ("X", "T")


def test_search_by_name_quotes_and_escapes(at, net):
    at.search('The "Band"', 'Back\\slash "x"')
    q = net.kinds("name")[0]
    assert q == r'recording:"Back\\slash \"x\"" AND artist:"The \"Band\""'


def test_search_without_an_artist_has_no_artist_clause(at, net):
    at.search("", "Song")
    assert net.kinds("name") == ['recording:"Song"']


def test_search_by_ids_uses_a_rid_query(at, net):
    at.search("ignored", "ignored", ["a", "b"])
    assert net.kinds("rid") == ["rid:(a OR b)"]
    assert net.kinds("name") == []


def test_mb_asks_for_json_and_is_paced(at, monkeypatch):
    seen = []
    monkeypatch.setattr(at, "get_json", lambda url, **k: seen.append(url) or {"recordings": []})
    sleeps = []
    monkeypatch.setattr(at.time, "sleep", sleeps.append)
    monkeypatch.setattr(at, "MB_INTERVAL", 5)
    monkeypatch.setattr(at, "_last_mb", 0.0)
    at.mb("recording", query="x")
    at.mb("recording", query="y")
    assert "fmt=json" in seen[0] and seen[0].startswith(at.MB + "/recording?")
    assert len(sleeps) == 1 and 0 < sleeps[0] <= 5  # the second call waits, the first does not


# -- fingerprints -------------------------------------------------------------------

def fake_fpcalc(monkeypatch, at, returncode=0, stdout='{"duration": 218.4, "fingerprint": "FP"}',
                stderr="", which="/usr/bin/fpcalc"):
    monkeypatch.setattr(at.shutil, "which", lambda name: which)
    ran = []

    def run(cmd, **kw):
        ran.append(cmd)
        return types.SimpleNamespace(returncode=returncode, stdout=stdout, stderr=stderr)

    monkeypatch.setattr(at.subprocess, "run", run)
    return ran


def test_fingerprint_ids_best_score_first_deduplicated(at, net, monkeypatch, tmp_path):
    ran = fake_fpcalc(monkeypatch, at)
    ids = at.fingerprint_ids(tmp_path / "x.mp3", "KEY")
    # 0.98 -> REC1, REC2; 0.80 -> REC3, REC1 (already there); 0.45 is dropped.
    assert ids == [REC1, REC2, REC3]
    assert ran == [["fpcalc", "-json", str(tmp_path / "x.mp3")]]
    body = net.kinds("acoustid")[0]
    assert body["client"] == ["KEY"] and body["fingerprint"] == ["FP"]
    assert body["duration"] == ["218"] and body["meta"] == ["recordingids"]


def test_fingerprint_ids_stops_at_ten(at, net, monkeypatch, tmp_path):
    fake_fpcalc(monkeypatch, at)
    net.acoustid = {"status": "ok", "results": [
        {"score": 0.9, "recordings": [{"id": f"r{i}"} for i in range(15)]}]}
    assert at.fingerprint_ids(tmp_path / "x.mp3", "K") == [f"r{i}" for i in range(10)]


def test_fingerprint_ids_with_only_weak_matches_is_empty(at, net, monkeypatch, tmp_path):
    fake_fpcalc(monkeypatch, at)
    net.acoustid = {"status": "ok", "results": [{"score": 0.49, "recordings": [{"id": "r"}]},
                                                 {"score": 0.5, "recordings": [{"id": "ok"}]}]}
    assert at.fingerprint_ids(tmp_path / "x.mp3", "K") == ["ok"]


def test_an_acoustid_error_is_a_failure(at, net, monkeypatch, tmp_path):
    fake_fpcalc(monkeypatch, at)
    net.acoustid = fixture_json("acoustid-error.json")
    with pytest.raises(at.Fail) as e:
        at.fingerprint_ids(tmp_path / "x.mp3", "K")
    assert "invalid API key" in str(e.value)


def test_no_fpcalc_means_no_ids_and_no_request(at, net, monkeypatch, tmp_path):
    fake_fpcalc(monkeypatch, at, which=None)
    assert at.fingerprint_ids(tmp_path / "x.mp3", "K") == []
    assert net.calls == []


def test_a_failing_fpcalc_means_no_ids_and_no_request(at, net, monkeypatch, tmp_path):
    fake_fpcalc(monkeypatch, at, returncode=2, stderr="boom")
    assert at.fingerprint_ids(tmp_path / "x.mp3", "K") == []
    assert net.calls == []


def test_acoustid_key_from_the_environment_then_the_file(at, monkeypatch, tmp_path):
    assert at.acoustid_key() == ""
    (tmp_path / "home" / ".config" / "autotag").mkdir(parents=True)
    (tmp_path / "home" / ".config" / "autotag" / "acoustid.key").write_text("FROMFILE\n")
    assert at.acoustid_key() == "FROMFILE"
    monkeypatch.setenv("ACOUSTID_KEY", " FROMENV ")
    assert at.acoustid_key() == "FROMENV"


# -- get_json (the one real network function, with urlopen replaced) ----------------

def test_get_json_turns_http_and_url_errors_into_fail(monkeypatch):
    import io
    import urllib.error

    import conftest

    mod = conftest.load_module()  # a copy: the shared one has get_json stubbed out
    monkeypatch.setattr(mod.time, "sleep", lambda s: None)

    def http(code):
        def raiser(*a, **k):
            raise urllib.error.HTTPError("u", code, "x", {}, io.BytesIO(b""))
        return raiser

    monkeypatch.setattr(mod.urllib.request, "urlopen", http(404))
    with pytest.raises(mod.Fail) as e:
        mod.get_json("https://example.invalid/x?secret=1")
    assert "HTTP 404" in str(e.value) and "secret" not in str(e.value)

    calls = []
    monkeypatch.setattr(mod.urllib.request, "urlopen",
                        lambda *a, **k: calls.append(1) or http(503)())
    with pytest.raises(mod.Fail):
        mod.get_json("https://example.invalid/x", retries=3)
    assert len(calls) == 3  # 503 is retried

    def refused(*a, **k):
        raise urllib.error.URLError("refused")

    monkeypatch.setattr(mod.urllib.request, "urlopen", refused)
    with pytest.raises(mod.Fail):
        mod.get_json("https://example.invalid/x")


# -- failures while reading an answer, and a bad fpcalc ----------------------------------

class FakeResponse:
    def __init__(self, body=b"", error=None):
        self.body, self.error = body, error

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False

    def read(self, *a):
        if self.error:
            raise self.error
        return self.body


@pytest.mark.parametrize("response, fragment", [
    (FakeResponse(b"<html><body>502 Bad Gateway</body></html>"), "not JSON"),
    (FakeResponse(b""), "not JSON"),
    (FakeResponse(b'{"a": "\xff\xfe"}'), "not JSON"),            # not UTF-8
    (FakeResponse(error=TimeoutError("read timed out")), "timed out"),
    (FakeResponse(error=ConnectionResetError("reset by peer")), "reset"),
])
def test_get_json_failures_while_reading_are_fails(monkeypatch, response, fragment):
    import conftest

    mod = conftest.load_module()
    monkeypatch.setattr(mod.urllib.request, "urlopen", lambda *a, **k: response)
    with pytest.raises(mod.Fail) as e:
        mod.get_json("https://example.invalid/x?secret=1")
    assert fragment in str(e.value) and "secret" not in str(e.value)


def test_get_json_still_returns_good_json(monkeypatch):
    import conftest

    mod = conftest.load_module()
    monkeypatch.setattr(mod.urllib.request, "urlopen", lambda *a, **k: FakeResponse(b'{"a": 1}'))
    assert mod.get_json("https://example.invalid/x") == {"a": 1}


@pytest.mark.parametrize("stdout", [
    "not json", "", "[1]", "{}", '{"duration": 200}', '{"fingerprint": "FP"}',
    '{"duration": "long", "fingerprint": "FP"}', '{"duration": null, "fingerprint": "FP"}'])
def test_a_bad_fpcalc_answer_gives_no_ids_and_no_request(at, net, monkeypatch, tmp_path, capsys, stdout):
    fake_fpcalc(monkeypatch, at, stdout=stdout)
    assert at.fingerprint_ids(tmp_path / "x.mp3", "K") == []
    assert net.calls == []
    assert "no fingerprint" in capsys.readouterr().out
