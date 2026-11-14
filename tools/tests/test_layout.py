"""Where a song is filed: names, cuts, folders reused, same-title check."""

import unicodedata
from pathlib import Path

import pytest


def nfd(s):
    return unicodedata.normalize("NFD", s)


def nfc(s):
    return unicodedata.normalize("NFC", s)


# -- clean --------------------------------------------------------------------------

@pytest.mark.parametrize("name, limit, expected", [
    ("AC/DC", 40, "AC_DC"),
    ('a\\b:c*d?e"f<g>h|i', 40, "a_b_c_d_e_f_g_h_i"),
    ("  padded  ", 40, "padded"),
    (".hidden", 40, "_hidden"),
    ("ends with.", 40, "ends with_"),
    ("", 40, "_"),
    ("   ", 40, "_"),
    ("x" * 41, 40, "x" * 40),
    ("x" * 39 + " y", 40, "x" * 39),       # the cut leaves a space: stripped
    ("é" * 50, 36, "é" * 36),
])
def test_clean(at, name, limit, expected):
    assert at.clean(name, limit) == expected


def test_clean_cuts_after_normalising_so_nfd_input_is_not_cut_short(at):
    decomposed = nfd("é" * 40)          # 80 code points
    got = at.clean(decomposed, 40)
    assert got == "é" * 40
    assert unicodedata.is_normalized("NFC", got)


def test_clean_cut_name_that_ends_in_a_dot_is_fixed(at):
    assert at.clean("a" * 39 + "." + "b" * 5, 40) == "a" * 39 + "_"


# -- file_name ----------------------------------------------------------------------

@pytest.mark.parametrize("tags, expected", [
    ({"title": "Song", "track_no": 3}, "03 Song.mp3"),
    ({"title": "Song", "track_no": 12}, "12 Song.mp3"),
    ({"title": "Song"}, "Song.mp3"),
    ({"title": "Song", "track_no": None}, "Song.mp3"),
    ({"title": "Song", "track_no": 3, "disc_no": 2, "disc_total": 2}, "2-03 Song.mp3"),
    ({"title": "Song", "track_no": 3, "disc_no": 1, "disc_total": 1}, "03 Song.mp3"),
    ({"title": "Song", "track_no": 3, "disc_no": 0, "disc_total": 2}, "03 Song.mp3"),
    ({"title": "Song", "track_no": 3, "disc_total": 2}, "03 Song.mp3"),
    ({"title": "Song", "track_no": 3, "disc_no": 2}, "03 Song.mp3"),
    ({"title": "Song", "track_no": 105, "disc_no": 1, "disc_total": 3}, "1-105 Song.mp3"),
    ({"title": "AC/DC: Live?", "track_no": 1}, "01 AC_DC_ Live_.mp3"),
])
def test_file_name(at, tags, expected):
    assert at.file_name(tags, ".mp3") == expected


def test_file_name_is_cut_at_36_characters_prefix_included(at):
    got = at.file_name({"title": "T" * 60, "track_no": 7}, ".m4a")
    assert got == "07 " + "T" * 33 + ".m4a"
    assert len(got) == 36 + len(".m4a")


def test_file_name_with_a_disc_prefix_is_cut_at_36_too(at):
    got = at.file_name({"title": "T" * 60, "track_no": 7, "disc_no": 2, "disc_total": 2}, ".wav")
    assert got == "2-07 " + "T" * 31 + ".wav"


def test_file_name_cut_does_not_leave_a_trailing_space(at):
    title = "a" * 32 + " bbbb"      # "01 " + 32 a's = 35 chars, then a space at 36
    got = at.file_name({"title": title, "track_no": 1}, ".mp3")
    assert got == "01 " + "a" * 32 + ".mp3"


# -- find_or_new ---------------------------------------------------------------------

def test_a_missing_folder_is_new_and_cut_at_40(at, tmp_path):
    got, exists = at.find_or_new(tmp_path, "N" * 55)
    assert (got, exists) == (tmp_path / ("N" * 40), False)
    assert not got.exists()  # find_or_new creates nothing


def test_a_missing_parent_is_new(at, tmp_path):
    got, exists = at.find_or_new(tmp_path / "absent", "X")
    assert (got, exists) == (tmp_path / "absent" / "X", False)


@pytest.mark.parametrize("existing, wanted", [
    ("a-ha", "A-HA"), ("A-HA", "a-ha"), ("Beyoncé", "BEYONCÉ"),
    (nfd("Beyoncé"), "Beyoncé"), ("Beyoncé", nfd("Beyoncé")), (nfd("Sigur Rós"), "sigur rós"),
])
def test_an_existing_folder_is_reused_across_case_and_unicode_form(at, tmp_path, existing, wanted):
    (tmp_path / existing).mkdir()
    got, exists = at.find_or_new(tmp_path, wanted)
    assert exists is True
    assert got.name == existing          # the name on disk, not the requested one
    assert len(list(tmp_path.iterdir())) == 1


def test_a_long_name_is_matched_through_its_cut_form(at, tmp_path):
    (tmp_path / ("L" * 40)).mkdir()
    got, exists = at.find_or_new(tmp_path, "L" * 70)
    assert exists and got.name == "L" * 40


def test_a_file_with_the_name_is_not_a_folder_to_reuse(at, tmp_path):
    (tmp_path / "Band").write_bytes(b"x")
    got, exists = at.find_or_new(tmp_path, "Band")
    assert (got, exists) == (tmp_path / "Band", False)


def test_a_different_name_is_not_reused(at, tmp_path):
    (tmp_path / "Band Two").mkdir()
    assert at.find_or_new(tmp_path, "Band")[1] is False


# -- same_song -----------------------------------------------------------------------

def touch(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"x")
    return path


@pytest.mark.parametrize("name", [
    "05 Take On Me.mp3", "5 Take On Me.mp3", "1-05 Take On Me.mp3", "05. Take On Me.mp3",
    "05 - Take On Me.mp3", "Take On Me.mp3", "05 take on me.m4a", "05 TAKE ON ME.wav",
    "05 Take On Me.aiff", "05 Take On Me.aif",
])
def test_same_song_matches_the_title_without_its_number(at, tmp_path, name):
    f = touch(tmp_path / "Album" / name)
    assert at.same_song(tmp_path, "Take On Me") == [f]


def test_same_song_matches_across_unicode_form(at, tmp_path):
    f = touch(tmp_path / "A" / ("01 " + nfd("Café") + ".mp3"))
    assert at.same_song(tmp_path, "Café") == [f]
    assert at.same_song(tmp_path, nfd("CAFÉ")) == [f]


def test_same_song_does_not_match_other_titles_or_longer_ones(at, tmp_path):
    touch(tmp_path / "A" / "01 Take On Me (Live).mp3")
    touch(tmp_path / "A" / "02 Take On.mp3")
    touch(tmp_path / "A" / "03 Other.mp3")
    assert at.same_song(tmp_path, "Take On Me") == []


def test_same_song_ignores_non_audio_files_and_folders(at, tmp_path):
    touch(tmp_path / "A" / "01 Take On Me.txt")
    touch(tmp_path / "A" / "01 Take On Me.jpg")
    (tmp_path / "A" / "01 Take On Me.mp3").mkdir()
    assert at.same_song(tmp_path, "Take On Me") == []


def test_same_song_looks_in_every_album_under_the_folder(at, tmp_path):
    a = touch(tmp_path / "A" / "One" / "01 Song.mp3")
    b = touch(tmp_path / "A" / "Two" / "09 Song.m4a")
    assert sorted(at.same_song(tmp_path / "A", "Song")) == [a, b]


def test_a_title_cut_at_36_characters_matches_the_full_title(at, tmp_path):
    full = "A Very Long Song Title That Goes On And On Forever"
    stored = at.file_name({"title": full, "track_no": 1}, ".mp3")
    f = touch(tmp_path / "A" / stored)
    assert len(Path(stored).stem) in (35, 36)  # 35 when the cut ends on a space
    assert at.same_song(tmp_path, full) == [f]


def test_a_short_name_is_not_a_cut_title(at, tmp_path):
    # Starts the title but is short: only 36-character names count as cut.
    touch(tmp_path / "A" / "01 A Very Long Song.mp3")
    assert at.same_song(tmp_path, "A Very Long Song Title That Goes On And On Forever") == []


def test_a_cut_name_too_short_after_its_number_does_not_match(at, tmp_path):
    # 35+ characters in the stem but under 10 once the number is gone.
    touch(tmp_path / "A" / ("1-05 - " + "x" * 28 + ".mp3"))
    touch(tmp_path / "A" / ("0" * 33 + " ab.mp3"))
    assert at.same_song(tmp_path, "abcdefghijkl") == []


# -- place ---------------------------------------------------------------------------

def test_place_lays_out_artist_album_number_title(at, make_library):
    lib = make_library()
    w = at.place(lib, {"title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low",
                       "track_no": 3}, ".mp3")
    assert w.dest == lib / "a-ha" / "Hunting High and Low" / "03 Take On Me.mp3"
    assert (w.artist_exists, w.album_exists, w.dups) == (False, False, [])
    assert w.root == lib and w.artist_dir == lib / "a-ha"
    assert not w.dest.parent.exists()   # place creates nothing


def test_album_artist_wins_over_artist(at, make_library):
    w = at.place(make_library(), {"title": "T", "artist": "Feat", "album_artist": "Main",
                                  "album": "A"}, ".mp3")
    assert w.artist_dir.name == "Main"


@pytest.mark.parametrize("name", ["Various Artists", "various artists", "VARIOUS ARTISTS ", ])
def test_various_artists_go_to_compilations(at, make_library, name):
    w = at.place(make_library(), {"title": "T", "album_artist": name, "album": "Hits"}, ".mp3")
    assert w.dest.parent.parent.name == "Compilations"
    assert w.dest.parent.name == "Hits"


def test_an_existing_compilations_folder_is_reused_in_any_case(at, make_library):
    lib = make_library()
    (lib / "COMPILATIONS" / "Hits").mkdir(parents=True)
    w = at.place(lib, {"title": "T", "album_artist": "Various Artists", "album": "hits"}, ".mp3")
    assert w.dest.parent == lib / "COMPILATIONS" / "Hits"
    assert (w.artist_exists, w.album_exists) == (True, True)


def test_missing_artist_and_album_get_the_unknown_names(at, make_library):
    w = at.place(make_library(), {"title": "T"}, ".mp3")
    assert w.dest.parent.parent.name == "Unknown Artist"
    assert w.dest.parent.name == "Unknown Album"


def test_artist_and_album_folders_are_cut_at_40(at, make_library):
    w = at.place(make_library(), {"title": "T", "artist": "A" * 60, "album": "B" * 60}, ".mp3")
    assert w.artist_dir.name == "A" * 40 and w.album_dir.name == "B" * 40


def test_folders_are_reused_across_case_and_unicode_form(at, make_library):
    lib = make_library()
    (lib / nfd("Beyoncé") / nfd("Lemonade")).mkdir(parents=True)
    w = at.place(lib, {"title": "T", "artist": "BEYONCÉ", "album": "lemonade", "track_no": 1}, ".m4a")
    assert w.dest.parent == lib / nfd("Beyoncé") / nfd("Lemonade")
    assert (w.artist_exists, w.album_exists) == (True, True)
    assert w.dest.name == "01 T.m4a"


def test_a_new_name_is_made_in_nfc(at, make_library):
    w = at.place(make_library(), {"title": nfd("Été"), "artist": nfd("Mylène"), "album": "A"}, ".mp3")
    assert w.artist_dir.name == nfc("Mylène") and w.dest.name == nfc("Été") + ".mp3"
    assert unicodedata.is_normalized("NFC", str(w.dest.relative_to(w.root)))


def test_dups_are_found_only_under_an_existing_artist(at, make_library):
    lib = make_library()
    other = lib / "Band" / "Old Album" / "01 Song.mp3"
    touch(other)
    w = at.place(lib, {"title": "Song", "artist": "band", "album": "New Album"}, ".mp3")
    assert w.dups == [other]
    w = at.place(lib, {"title": "Song", "artist": "Nobody", "album": "New Album"}, ".mp3")
    assert w.dups == []
    # The same title under another artist is not a duplicate.
    touch(lib / "Someone Else" / "Some Album" / "01 Song.mp3")
    assert at.place(lib, {"title": "Song", "artist": "Nobody"}, ".mp3").dups == []


def test_a_missing_library_is_refused(at, tmp_path):
    with pytest.raises(at.Fail) as e:
        at.place(tmp_path / "absent", {"title": "T"}, ".mp3")
    assert "missing or empty" in str(e.value)


def test_an_empty_library_is_refused(at, tmp_path):
    (tmp_path / "empty").mkdir()
    with pytest.raises(at.Fail) as e:
        at.place(tmp_path / "empty", {"title": "T"}, ".mp3")
    assert "missing or empty" in str(e.value)
    assert list((tmp_path / "empty").iterdir()) == []


def test_a_library_that_is_a_file_is_refused(at, tmp_path):
    (tmp_path / "f").write_bytes(b"x")
    with pytest.raises(at.Fail):
        at.place(tmp_path / "f", {"title": "T"}, ".mp3")


@pytest.mark.parametrize("tags", [{}, {"title": ""}, {"title": None}, {"artist": "A"}])
def test_no_title_is_refused(at, make_library, tags):
    with pytest.raises(at.Fail) as e:
        at.place(make_library(), tags, ".mp3")
    assert "title" in str(e.value)
