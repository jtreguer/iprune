"""The interactive command line (`autotag FILE...`), run in process through
main() with argv, input() and the MusicBrainz stub replaced."""

import builtins
import shutil
import sys

import pytest

from conftest import KINDS, REC1, Snapshot, audio_md5, temp_files, tree

NOT_SUPPORTED = "not a supported file (MP3, M4A, WAV or AIFF)"
TAGS = {"title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low", "track_no": 3}


def run(at, monkeypatch, capsys, *argv, answers=()):
    monkeypatch.setattr(sys, "argv", ["autotag", *map(str, argv)])
    queue = list(answers)

    def fake_input(prompt=""):
        if not queue:
            raise EOFError
        print(prompt, end="")
        return queue.pop(0)

    monkeypatch.setattr(builtins, "input", fake_input)
    code = at.main()
    return code, capsys.readouterr().out


@pytest.fixture
def lib(make_library):
    return make_library()


@pytest.fixture
def tagged(at, make_audio):
    """A tagged file of the given kind in the inbox."""
    def make(kind="mp3", tags=TAGS):
        f = make_audio(kind)
        at.write_tags(f, dict(tags))
        return f
    return make


@pytest.fixture
def take_on_me(long_mp3, tmp_path):
    f = tmp_path / "inbox" / "a-ha - Take On Me.mp3"
    f.parent.mkdir(exist_ok=True)
    shutil.copy(long_mp3, f)
    return f


# -- --export-only ------------------------------------------------------------------------

@pytest.mark.parametrize("kind", KINDS)
def test_export_only_moves_the_file_with_the_tags_it_has(at, monkeypatch, capsys, lib, tagged, kind):
    f = tagged(kind)
    original = f.read_bytes()
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f)
    dest = lib / "a-ha" / "Hunting High and Low" / f"03 Take On Me{f.suffix}"
    assert code == 0
    assert f"moved to {dest}" in out
    assert dest.read_bytes() == original        # filed as it was, not rewritten
    assert not f.exists()
    assert temp_files(lib) == []


def test_export_only_with_copy_keeps_the_original(at, monkeypatch, capsys, lib, tagged):
    f = tagged("mp3")
    snap = Snapshot(f)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--copy", "--library", lib, f)
    dest = lib / "a-ha" / "Hunting High and Low" / "03 Take On Me.mp3"
    assert code == 0 and f"copied to {dest}" in out
    assert "copyd" not in out and "moved to" not in out
    assert dest.read_bytes() == f.read_bytes()
    snap.assert_unchanged()


def test_export_only_dry_run_changes_nothing_and_says_what_it_would_do(at, monkeypatch, capsys, lib, tagged):
    f = tagged("mp3")
    snap, before = Snapshot(f), tree(lib)
    code, out = run(at, monkeypatch, capsys, "--export-only", "-n", "--library", lib, f)
    assert code == 0
    assert f"would move to {lib / 'a-ha' / 'Hunting High and Low' / '03 Take On Me.mp3'}" in out
    snap.assert_unchanged()
    assert tree(lib) == before


def test_export_only_dry_run_with_copy_says_copy(at, monkeypatch, capsys, lib, tagged):
    f = tagged("mp3")
    code, out = run(at, monkeypatch, capsys, "--export-only", "-n", "--copy", "--library", lib, f)
    assert code == 0 and "would copy to" in out and "would move" not in out


def test_export_only_names_new_and_existing_folders(at, monkeypatch, capsys, lib, tagged):
    (lib / "A-HA").mkdir()
    code, out = run(at, monkeypatch, capsys, "--export-only", "-n", "--library", lib, tagged("mp3"))
    assert "artist folder A-HA\n" in out                  # reused, in its own spelling
    assert "album folder  Hunting High and Low (new)" in out


def test_export_only_leaves_an_existing_destination_alone(at, monkeypatch, capsys, lib, tagged):
    dest = lib / "a-ha" / "Hunting High and Low" / "03 Take On Me.mp3"
    dest.parent.mkdir(parents=True)
    dest.write_bytes(b"already here")
    f = tagged("mp3")
    snap = Snapshot(f)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f)
    assert code == 1 and "already in the library" in out
    assert dest.read_bytes() == b"already here"
    snap.assert_unchanged()
    assert temp_files(lib) == []


def test_export_only_with_no_title_is_refused(at, monkeypatch, capsys, lib, make_audio):
    f = make_audio("mp3")
    snap, before = Snapshot(f), tree(lib)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f)
    assert code == 1 and "no title" in out
    snap.assert_unchanged()
    assert tree(lib) == before


def test_export_only_refuses_a_library_that_is_not_there(at, monkeypatch, capsys, tmp_path, tagged):
    f = tagged("mp3")
    snap = Snapshot(f)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", tmp_path / "gone", f)
    assert code == 1 and "missing or empty" in out
    snap.assert_unchanged()
    assert not (tmp_path / "gone").exists()


def test_a_same_title_song_asks_first(at, monkeypatch, capsys, lib, tagged):
    old = lib / "a-ha" / "Other" / "01 Take On Me.mp3"
    old.parent.mkdir(parents=True)
    old.write_bytes(b"old")
    f = tagged("mp3")
    snap = Snapshot(f)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f, answers=["n"])
    assert code == 1 and "same title already filed: a-ha/Other/01 Take On Me.mp3" in out
    assert "not exported" in out
    snap.assert_unchanged()
    assert not (lib / "a-ha" / "Hunting High and Low").exists()

    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f, answers=["y"])
    assert code == 0 and not f.exists()
    assert (lib / "a-ha" / "Hunting High and Low" / "03 Take On Me.mp3").is_file()
    assert old.read_bytes() == b"old"


def test_a_same_title_song_is_not_filed_with_yes(at, monkeypatch, capsys, lib, tagged):
    old = lib / "a-ha" / "Other" / "01 Take On Me.mp3"
    old.parent.mkdir(parents=True)
    old.write_bytes(b"old")
    f = tagged("mp3")
    snap, before = Snapshot(f), tree(lib)
    code, out = run(at, monkeypatch, capsys, "--export-only", "-y", "--library", lib, f)
    assert code == 1 and "not exported" in out
    snap.assert_unchanged()
    assert tree(lib) == before


def test_export_only_and_no_export_exclude_each_other(at, monkeypatch, capsys, lib, tagged):
    f = tagged("mp3")
    with pytest.raises(SystemExit) as e:
        run(at, monkeypatch, capsys, "--export-only", "--no-export", "--library", lib, f)
    assert e.value.code == 2


# -- files it does not take -----------------------------------------------------------------

@pytest.mark.parametrize("name", ["notes.txt", "song.flac", "song.ogg", "cover.jpg", "noext"])
def test_a_file_that_is_not_supported_is_named_and_skipped(at, monkeypatch, capsys, lib, tmp_path, name):
    f = tmp_path / name
    f.write_bytes(b"data")
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, f)
    assert code == 1
    assert out.splitlines()[0] == name
    assert f"  {NOT_SUPPORTED}" in out.splitlines()
    assert f.read_bytes() == b"data"


def test_a_missing_file_gets_the_same_message(at, monkeypatch, capsys, lib, tmp_path):
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, tmp_path / "gone.mp3")
    assert code == 1 and NOT_SUPPORTED in out


def test_the_other_files_are_still_done_and_the_status_is_1(at, monkeypatch, capsys, lib, tagged, tmp_path):
    bad = tmp_path / "a.flac"
    bad.write_bytes(b"x")
    f = tagged("mp3")
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, bad, f)
    assert code == 1 and NOT_SUPPORTED in out
    assert (lib / "a-ha" / "Hunting High and Low" / "03 Take On Me.mp3").is_file()


def test_an_unreadable_audio_file_is_reported_and_the_next_one_goes_on(at, monkeypatch, capsys, lib, tagged, tmp_path):
    broken = tmp_path / "broken.mp3"
    broken.write_bytes(b"not audio" * 30)
    code, out = run(at, monkeypatch, capsys, "--export-only", "--library", lib, broken, tagged("mp3"))
    assert code == 1 and "cannot read broken.mp3" in out
    assert broken.exists()
    assert (lib / "a-ha").is_dir()


# -- tagging from MusicBrainz ------------------------------------------------------------------

def test_yes_no_export_tags_the_file_in_place(at, net, monkeypatch, capsys, lib, take_on_me):
    audio = audio_md5(take_on_me)
    before = tree(lib)
    code, out = run(at, monkeypatch, capsys, "-y", "--no-export", "--library", lib, take_on_me)
    assert code == 0 and "written" in out
    assert take_on_me.is_file()
    assert at.read_tags(at.open_audio(take_on_me)) == {
        "title": "Take On Me", "artist": "a-ha",
        "album": "Die ultimative Chartshow: Die beliebtesten Hits aller Zeiten",
        "album_artist": "Various Artists", "genre": "Synth-Pop", "year": 2019,
        "track_no": 7, "track_total": 22, "disc_no": 1, "disc_total": 2}
    assert at.open_audio(take_on_me).tags["UFID:http://musicbrainz.org"].data == REC1.encode()
    assert audio_md5(take_on_me) == audio
    assert tree(lib) == before
    assert temp_files(take_on_me.parent) == []


def test_yes_files_the_tagged_song_and_says_moved(at, net, monkeypatch, capsys, lib, take_on_me):
    code, out = run(at, monkeypatch, capsys, "-y", "--library", lib, take_on_me)
    dest = lib / "Compilations" / "Die ultimative Chartshow_ Die beliebtest" / "1-07 Take On Me.mp3"
    assert code == 0 and f"moved to {dest}" in out
    assert dest.is_file() and not take_on_me.exists()
    assert at.read_tags(at.open_audio(dest))["genre"] == "Synth-Pop"
    assert temp_files(lib) == []


def test_yes_with_copy_says_copied_and_keeps_the_file(at, net, monkeypatch, capsys, lib, take_on_me):
    code, out = run(at, monkeypatch, capsys, "-y", "--copy", "--library", lib, take_on_me)
    dest = lib / "Compilations" / "Die ultimative Chartshow_ Die beliebtest" / "1-07 Take On Me.mp3"
    assert code == 0 and f"copied to {dest}" in out
    assert "copyd" not in out
    assert dest.is_file() and take_on_me.is_file()
    assert at.read_tags(at.open_audio(dest)) == at.read_tags(at.open_audio(take_on_me))


def test_dry_run_writes_and_files_nothing(at, net, monkeypatch, capsys, lib, take_on_me):
    snap, before = Snapshot(take_on_me), tree(lib)
    code, out = run(at, monkeypatch, capsys, "-n", "-y", "--library", lib, take_on_me)
    assert code == 0 and "would move to" in out and "written" not in out
    snap.assert_unchanged()
    assert tree(lib) == before


def test_yes_skips_a_first_match_whose_length_differs(at, net, monkeypatch, capsys, lib, make_audio):
    f = make_audio("mp3", name="a-ha - Take On Me.mp3")      # 2 s against 218 s
    snap, before = Snapshot(f), tree(lib)
    code, out = run(at, monkeypatch, capsys, "-y", "--library", lib, f)
    assert code == 1 and "not certain" in out
    snap.assert_unchanged()
    assert tree(lib) == before


def test_yes_with_no_match_skips_the_file(at, net, monkeypatch, capsys, lib, take_on_me):
    from conftest import fixture_json
    net.search_name = fixture_json("mb-search-none.json")
    snap, before = Snapshot(take_on_me), tree(lib)
    code, out = run(at, monkeypatch, capsys, "-y", "--library", lib, take_on_me)
    assert code == 1 and "no match" in out
    snap.assert_unchanged()
    assert tree(lib) == before


def test_artist_and_title_replace_the_guess_and_skip_the_fingerprint(at, net, monkeypatch, capsys, lib, make_audio):
    monkeypatch.setenv("ACOUSTID_KEY", "KEY")
    monkeypatch.setattr(at, "fingerprint_ids", lambda *a: pytest.fail("fingerprint used"))
    f = make_audio("mp3", name="whatever.mp3")
    code, out = run(at, monkeypatch, capsys, "-n", "--no-export", "--artist", "Guts",
                    "--title", "Aimer sans amour", "--library", lib, f, answers=["1"])
    assert net.kinds("name") == ['recording:"Aimer sans amour" AND artist:"Guts"']
    assert net.kinds("acoustid") == []
    assert "searching for: Guts" in out


def test_without_a_key_it_says_so_and_matches_by_name(at, net, monkeypatch, capsys, lib, take_on_me):
    code, out = run(at, monkeypatch, capsys, "-n", "-y", "--no-export", "--library", lib, take_on_me)
    assert "no AcoustID key" in out
    assert net.kinds("name") and net.kinds("rid") == []


def test_choosing_a_candidate_by_number_then_confirming(at, net, monkeypatch, capsys, lib, take_on_me):
    code, out = run(at, monkeypatch, capsys, "--no-export", "--library", lib, take_on_me,
                    answers=["2", "y"])
    assert code == 0
    got = at.read_tags(at.open_audio(take_on_me))
    assert got["track_no"] == 19 and got["album"].startswith("Homecoming")   # the second candidate


def test_declining_the_write_changes_nothing(at, net, monkeypatch, capsys, lib, take_on_me):
    snap = Snapshot(take_on_me)
    code, out = run(at, monkeypatch, capsys, "--no-export", "--library", lib, take_on_me,
                    answers=["1", "n"])
    assert code == 1 and "not written" in out
    snap.assert_unchanged()


@pytest.mark.parametrize("answers", [["q"], []])
def test_skipping_or_closed_input_changes_nothing(at, net, monkeypatch, capsys, lib, take_on_me, answers):
    snap, before = Snapshot(take_on_me), tree(lib)
    code, out = run(at, monkeypatch, capsys, "--library", lib, take_on_me, answers=answers)
    assert code == 1
    snap.assert_unchanged()
    assert tree(lib) == before


def test_typed_tags_for_a_track_musicbrainz_does_not_know(at, net, monkeypatch, capsys, lib, make_audio):
    from conftest import fixture_json
    net.search_name = fixture_json("mb-search-none.json")
    f = make_audio("mp3", name="Mashup (2019).mp3")
    typed = ["y", "My Mashup", "DJ Me", "Bootlegs", "DJ Me", "2/9", "2019", "Electronic", "y"]
    code, out = run(at, monkeypatch, capsys, "--no-export", "--library", lib, f, answers=typed)
    assert code == 0
    assert at.read_tags(at.open_audio(f)) == {
        "title": "My Mashup", "artist": "DJ Me", "album": "Bootlegs", "album_artist": "DJ Me",
        "track_no": 2, "track_total": 9, "year": 2019, "genre": "Electronic"}
    assert not [k for k in at.open_audio(f).tags if k.startswith("UFID")]   # no ids for typed tags


def test_a_failure_while_tagging_leaves_the_original_and_no_copy(at, net, monkeypatch, capsys, lib, take_on_me):
    snap = Snapshot(take_on_me)
    real = at.write_tags

    def tag_then_fail(path, new, ids=None):
        real(path, new, ids)
        raise RuntimeError("disk on fire")

    monkeypatch.setattr(at, "write_tags", tag_then_fail)
    with pytest.raises(RuntimeError):
        run(at, monkeypatch, capsys, "-y", "--no-export", "--library", lib, take_on_me)
    snap.assert_unchanged()
    assert temp_files(take_on_me.parent) == []


def test_tags_already_matching_are_not_rewritten(at, net, monkeypatch, capsys, lib, take_on_me):
    run(at, monkeypatch, capsys, "-y", "--no-export", "--library", lib, take_on_me)
    snap = Snapshot(take_on_me)
    code, out = run(at, monkeypatch, capsys, "-y", "--no-export", "--library", lib, take_on_me)
    assert code == 0 and "tags already match" in out
    snap.assert_unchanged()
