"""Tag reading and writing, per format.

Every test works on a copy of an ffmpeg-made file in a tmp dir. The audio
stream is compared with ffmpeg's md5 of the packets, so a write that touched
the audio fails here.
"""

import json
import struct
import subprocess

import pytest
from mutagen.id3 import APIC, COMM, PRIV, TCMP, TCON, TDRC, TIT2, TRCK, TSRC, TXXX
from mutagen.mp4 import MP4, MP4Cover, MP4FreeForm

from conftest import KINDS, audio_md5

ALL_FIELDS = {
    "title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low",
    "album_artist": "a-ha", "composer": "Pal Waaktaar", "genre": "Synth-Pop",
    "year": 1985, "track_no": 3, "track_total": 12, "disc_no": 1, "disc_total": 2,
    "compilation": True,
}
IDS = ("8c0c2908-54c7-4abb-872e-86f33ff90853", "0d56dbfe-894e-470a-9997-b2489d53b6a7")
ID3_KINDS = ["mp3", "wav", "aiff"]
MP4_KINDS = ["aac", "alac"]
FAKE_PNG = b"\x89PNG\r\n\x1a\nnot really a picture"
FAKE_JPG = b"\xff\xd8\xff\xe0fake jpeg"


def ensure_tags(audio):
    if audio.tags is None:
        audio.add_tags()


def seed(at, path, kind):
    """Give a file existing tags, with a picture, a comment, an unknown frame
    and the yt-dlp junk, written by mutagen directly."""
    audio = at.open_audio(path)
    if kind in MP4_KINDS:
        ensure_tags(audio)
        t = audio.tags
        t["©nam"] = ["Old title"]
        t["©ART"] = ["Old artist"]
        t["©alb"] = ["Old album"]
        t["trkn"] = [(3, 12)]
        t["disk"] = [(1, 2)]
        t["covr"] = [MP4Cover(FAKE_JPG, MP4Cover.FORMAT_JPEG)]
        t["©cmt"] = ["a comment of my own"]
        t["----:com.apple.iTunes:mine"] = [MP4FreeForm(b"keep me")]
        t["desc"] = ["yt description"]
        t["ldes"] = ["yt long description"]
        t["purl"] = ["https://www.youtube.com/watch?v=abc"]
        audio.save()
        return
    if audio.tags is None:
        ensure_tags(audio)
    t = audio.tags
    t.add(APIC(encoding=3, mime="image/png", type=3, desc="cover", data=FAKE_PNG))
    t.add(COMM(encoding=3, lang="eng", desc="mine", text="a comment of my own"))
    t.add(COMM(encoding=3, lang="eng", desc="", text="https://youtu.be/abc"))
    t.add(COMM(encoding=3, lang="eng", desc="yt2", text="from www.youtube.com"))
    t.add(TXXX(encoding=3, desc="mine", text="keep me"))
    t.add(TSRC(encoding=3, text="GBAAA1234567"))
    t.add(PRIV(owner="me@example.com", data=b"\x00\x01\x02"))
    for desc in ("description", "synopsis", "comment", "purl", "major_brand",
                 "minor_version", "compatible_brands", "Description"):
        t.add(TXXX(encoding=3, desc=desc, text="junk"))
    t.add(TIT2(encoding=3, text="Old title"))
    t.add(TRCK(encoding=3, text="3/12"))
    audio.save()


def reopen(at, path):
    return at.open_audio(path)


# -- round trip ----------------------------------------------------------------

@pytest.mark.parametrize("kind", KINDS)
def test_write_then_read_returns_what_was_written(at, make_audio, kind):
    path = make_audio(kind)
    before = audio_md5(path)
    at.write_tags(path, dict(ALL_FIELDS), IDS)
    assert at.read_tags(at.open_audio(path)) == ALL_FIELDS
    assert audio_md5(path) == before


@pytest.mark.parametrize("kind", KINDS)
def test_ffprobe_reads_the_written_tags(at, make_audio, kind):
    # A second reader, so a mutagen-only quirk cannot pass for a tag.
    path = make_audio(kind)
    at.write_tags(path, dict(ALL_FIELDS), IDS)
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format_tags", "-of", "json", str(path)],
        capture_output=True, text=True, check=True).stdout
    tags = {k.lower(): v for k, v in json.loads(out)["format"].get("tags", {}).items()}
    assert tags["title"] == "Take On Me"
    assert tags["artist"] == "a-ha"
    assert tags["album"] == "Hunting High and Low"
    assert tags["genre"] == "Synth-Pop"
    assert tags["date"].startswith("1985")
    assert tags["track"] == "3/12"
    assert tags["composer"] == "Pal Waaktaar"


@pytest.mark.parametrize("kind", KINDS)
def test_untouched_fields_stay_as_they_were(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"title": "T", "artist": "A", "year": 1999, "genre": "Rock"})
    at.write_tags(path, {"album": "Only the album"})
    assert at.read_tags(at.open_audio(path)) == {
        "title": "T", "artist": "A", "year": 1999, "genre": "Rock", "album": "Only the album"}


@pytest.mark.parametrize("kind", KINDS)
def test_none_in_new_leaves_the_field_alone(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"title": "T", "artist": "A", "year": 1999, "track_no": 2})
    at.write_tags(path, {"title": None, "year": None, "track_no": None, "album": "B"})
    assert at.read_tags(at.open_audio(path)) == {
        "title": "T", "artist": "A", "year": 1999, "track_no": 2, "album": "B"}


# -- what a write does not touch ------------------------------------------------

@pytest.mark.parametrize("kind", KINDS)
def test_other_frames_and_atoms_keep_their_bytes(at, make_audio, kind):
    path = make_audio(kind)
    seed(at, path, kind)
    before = audio_md5(path)
    at.write_tags(path, {"title": "New title", "year": 2001})
    read = at.read_tags(at.open_audio(path))
    assert read["title"] == "New title"
    assert read["year"] == 2001
    assert audio_md5(path) == before
    if kind in MP4_KINDS:
        assert read["artist"] == "Old artist"
        assert (read["track_no"], read["track_total"]) == (3, 12)
        t = reopen(at, path).tags
        assert bytes(t["covr"][0]) == FAKE_JPG
        assert list(t["©cmt"]) == ["a comment of my own"]
        assert bytes(t["----:com.apple.iTunes:mine"][0]) == b"keep me"
    else:
        t = reopen(at, path).tags
        assert t["APIC:cover"].data == FAKE_PNG
        assert str(t["COMM:mine:eng"]) == "a comment of my own"
        assert str(t["TXXX:mine"]) == "keep me"
        assert str(t["TSRC"]) == "GBAAA1234567"
        assert [f for k, f in t.items() if k.startswith("PRIV")][0].data == b"\x00\x01\x02"


@pytest.mark.parametrize("kind", ["aac", "alac"])
def test_a_picture_in_mp4_survives_an_untouched_atom_count(at, make_audio, kind):
    path = make_audio(kind)
    seed(at, path, kind)
    at.write_tags(path, {"title": "x"}, IDS)
    t = MP4(path).tags
    assert len(t["covr"]) == 1 and bytes(t["covr"][0]) == FAKE_JPG


# -- junk -----------------------------------------------------------------------

@pytest.mark.parametrize("kind", KINDS)
@pytest.mark.parametrize("new", [{"title": "x"}, {}])
def test_junk_is_removed_on_every_write_even_an_empty_one(at, make_audio, kind, new):
    path = make_audio(kind)
    seed(at, path, kind)
    assert at.junk_keys(at.open_audio(path).tags) != []  # the seed is junky
    at.write_tags(path, new)
    t = at.open_audio(path).tags
    assert at.junk_keys(t) == []
    if kind in MP4_KINDS:
        for atom in ("desc", "ldes", "purl"):
            assert atom not in t
        assert list(t["©cmt"]) == ["a comment of my own"]  # not a YouTube comment
    else:
        descs = {f.desc.lower() for k, f in t.items() if k.startswith("TXXX:")}
        assert descs == {"mine"}
        assert [k for k in t if k.startswith("COMM")] == ["COMM:mine:eng"]


@pytest.mark.parametrize("kind", MP4_KINDS)
def test_mp4_comment_with_a_youtube_url_is_removed(at, make_audio, kind):
    path = make_audio(kind)
    audio = at.open_audio(path)
    ensure_tags(audio)
    audio.tags["©cmt"] = ["https://www.youtube.com/watch?v=abc"]
    audio.save()
    at.write_tags(path, {"title": "x"})
    assert "©cmt" not in at.open_audio(path).tags


def test_junk_keys_of_no_tags_is_empty(at):
    assert at.junk_keys(None) == []


def test_junk_names_drops_the_frame_name(at):
    assert at.junk_names(["TXXX:description", "COMM::eng", "desc"]) == ["description", ":eng", "desc"]


# -- numbers --------------------------------------------------------------------

@pytest.mark.parametrize("kind", KINDS)
def test_track_number_alone_keeps_the_files_total(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"track_no": 3, "track_total": 12, "disc_no": 1, "disc_total": 2})
    at.write_tags(path, {"track_no": 5})
    got = at.read_tags(at.open_audio(path))
    assert (got["track_no"], got["track_total"], got["disc_no"], got["disc_total"]) == (5, 12, 1, 2)


@pytest.mark.parametrize("kind", KINDS)
def test_total_alone_keeps_the_files_number(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"track_no": 3, "track_total": 12, "disc_no": 1, "disc_total": 2})
    at.write_tags(path, {"track_total": 20, "disc_total": 3})
    got = at.read_tags(at.open_audio(path))
    assert (got["track_no"], got["track_total"], got["disc_no"], got["disc_total"]) == (3, 20, 1, 3)


@pytest.mark.parametrize("kind", KINDS)
def test_number_without_a_total_on_a_bare_file_has_no_total(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"track_no": 4})
    got = at.read_tags(at.open_audio(path))
    assert got["track_no"] == 4
    assert "track_total" not in got
    assert "disc_no" not in got


@pytest.mark.parametrize("kind", KINDS)
def test_a_total_alone_on_a_bare_file_has_no_number(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"disc_total": 2})
    got = at.read_tags(at.open_audio(path))
    assert got["disc_total"] == 2
    assert "disc_no" not in got


@pytest.mark.parametrize("new, current, expected", [
    ({}, {"track_no": 1}, None),
    ({"track_no": None, "track_total": None}, {"track_no": 1}, None),
    ({"track_no": 5}, {"track_no": 3, "track_total": 12}, (5, 12)),
    ({"track_total": 20}, {"track_no": 3, "track_total": 12}, (3, 20)),
    ({"track_no": 5, "track_total": 6}, {"track_no": 3, "track_total": 12}, (5, 6)),
    ({"track_no": 5}, {}, (5, 0)),
    ({"track_total": 9}, {}, (0, 9)),
    ({"track_no": 5}, {"track_no": None, "track_total": None}, (5, 0)),
])
def test_pair_to_write(at, new, current, expected):
    assert at.pair_to_write(new, current, "track_no", "track_total") == expected


# -- compilation ------------------------------------------------------------------

@pytest.mark.parametrize("kind", KINDS)
def test_compilation_false_removes_the_flag(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"compilation": True, "title": "x"})
    assert at.read_tags(at.open_audio(path))["compilation"] is True
    at.write_tags(path, {"compilation": False})
    audio = at.open_audio(path)
    assert "compilation" not in at.read_tags(audio)
    assert ("cpil" not in audio.tags) if kind in MP4_KINDS else ("TCMP" not in audio.tags)
    assert at.read_tags(audio)["title"] == "x"


@pytest.mark.parametrize("kind", KINDS)
def test_compilation_absent_or_none_keeps_the_flag(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"compilation": True})
    at.write_tags(path, {"title": "x"})
    at.write_tags(path, {"compilation": None})
    assert at.read_tags(at.open_audio(path))["compilation"] is True


def test_a_tcmp_of_zero_is_not_a_compilation(at, make_audio):
    path = make_audio("mp3")
    audio = at.open_audio(path)
    ensure_tags(audio)
    audio.tags.add(TCMP(encoding=3, text="0"))
    audio.save()
    assert "compilation" not in at.read_tags(at.open_audio(path))


# -- MusicBrainz ids ---------------------------------------------------------------

@pytest.mark.parametrize("kind", ID3_KINDS)
def test_id3_gets_a_ufid_and_an_album_id(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"title": "x"}, IDS)
    t = at.open_audio(path).tags
    assert t["UFID:http://musicbrainz.org"].data == IDS[0].encode()
    assert t["UFID:http://musicbrainz.org"].owner == "http://musicbrainz.org"
    assert list(t["TXXX:MusicBrainz Album Id"].text) == [IDS[1]]


@pytest.mark.parametrize("kind", MP4_KINDS)
def test_mp4_gets_track_and_album_id_atoms(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"title": "x"}, IDS)
    t = at.open_audio(path).tags
    assert bytes(t["----:com.apple.iTunes:MusicBrainz Track Id"][0]) == IDS[0].encode()
    assert bytes(t["----:com.apple.iTunes:MusicBrainz Album Id"][0]) == IDS[1].encode()


@pytest.mark.parametrize("kind", KINDS)
def test_no_ids_means_no_ids_written_and_old_ones_kept(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"title": "x"})
    t = at.open_audio(path).tags
    assert not [k for k in t.keys() if "MusicBrainz" in k or k.startswith("UFID")]
    at.write_tags(path, {"title": "x"}, IDS)
    at.write_tags(path, {"title": "y"})
    t = at.open_audio(path).tags
    assert [k for k in t.keys() if "MusicBrainz" in k or k.startswith("UFID")]


# -- format specifics ----------------------------------------------------------------

@pytest.mark.parametrize("kind", ID3_KINDS)
def test_id3_is_saved_as_v2_3(at, make_audio, kind):
    path = make_audio(kind)
    at.write_tags(path, {"title": "x"})  # ffmpeg's MP3 starts as v2.4
    assert at.open_audio(path).tags.version == (2, 3, 0)
    at.write_tags(path, {"title": "y"})
    assert at.open_audio(path).tags.version == (2, 3, 0)
    if kind == "mp3":
        assert path.read_bytes()[:4] == b"ID3\x03"


def test_the_mp3_template_really_starts_as_v2_4(at, make_audio):
    # Guards the test above: without this the v2.3 check could pass vacuously.
    assert make_audio("mp3").read_bytes()[:4] == b"ID3\x04"


def test_wav_keeps_its_riff_info_and_gets_an_id3_chunk(at, make_audio):
    path = make_audio("wav")
    assert b"LIST" in path.read_bytes()
    at.write_tags(path, {"title": "x"})
    data = path.read_bytes()
    assert b"id3 " in data
    assert b"LIST" in data
    assert data[:4] == b"RIFF"


def test_aiff_gets_an_id3_chunk(at, make_audio):
    path = make_audio("aiff")
    at.write_tags(path, {"title": "x"})
    data = path.read_bytes()
    assert data[:4] == b"FORM" and b"ID3 " in data


def inject_gnre(path, value):
    """Add a numeric `gnre` atom to an m4a by hand (mutagen cannot write one).
    ffmpeg puts `moov` last, so growing it moves no audio offsets."""
    def boxes(data, start, end):
        while start < end:
            size, name = struct.unpack(">I4s", data[start:start + 8])
            yield start, size, name
            start += size

    def find(data, start, end, want):
        return next((o, z) for o, z, n in boxes(data, start, end) if n == want)

    data = bytearray(path.read_bytes())
    moov = find(data, 0, len(data), b"moov")
    udta = find(data, moov[0] + 8, moov[0] + moov[1], b"udta")
    meta = find(data, udta[0] + 8, udta[0] + udta[1], b"meta")
    ilst = find(data, meta[0] + 12, meta[0] + meta[1], b"ilst")
    atom = struct.pack(">I4sI4sIIH", 26, b"gnre", 18, b"data", 0, 0, value)
    end = ilst[0] + ilst[1]
    data[end:end] = atom
    for offset in (moov[0], udta[0], meta[0], ilst[0]):
        size = struct.unpack(">I", data[offset:offset + 4])[0]
        data[offset:offset + 4] = struct.pack(">I", size + len(atom))
    path.write_bytes(data)


@pytest.mark.parametrize("kind", MP4_KINDS)
@pytest.mark.parametrize("gnre, name", [(14, "Pop"), (18, "Rock"), (1, "Blues")])
def test_numeric_mp4_genre_reads_as_its_name(at, make_audio, kind, gnre, name):
    path = make_audio(kind)
    inject_gnre(path, gnre)
    assert at.read_tags(at.open_audio(path))["genre"] == name


@pytest.mark.parametrize("gnre", [999, 60000])
def test_an_unknown_numeric_mp4_genre_reads_as_none(at, make_audio, gnre):
    path = make_audio("aac")
    inject_gnre(path, gnre)
    assert "genre" not in at.read_tags(at.open_audio(path))


def test_numeric_mp4_genre_is_replaced_by_a_written_genre(at, make_audio):
    path = make_audio("aac")
    inject_gnre(path, 18)
    at.write_tags(path, {"genre": "Folk"})
    audio = at.open_audio(path)
    assert at.read_tags(audio)["genre"] == "Folk"
    assert audio.tags.get("gnre") is None and list(audio.tags["©gen"]) == ["Folk"]


def test_numeric_mp4_genre_kept_when_no_genre_is_written(at, make_audio):
    path = make_audio("aac")
    inject_gnre(path, 18)
    at.write_tags(path, {"title": "x"})
    assert at.read_tags(at.open_audio(path))["genre"] == "Rock"


@pytest.mark.parametrize("kind", ID3_KINDS)
def test_id3_tcon_with_a_number_reads_as_its_name(at, make_audio, kind):
    path = make_audio(kind)
    audio = at.open_audio(path)
    if audio.tags is None:
        ensure_tags(audio)
    audio.tags.add(TCON(encoding=0, text="(13)"))
    audio.save(v2_version=3)         # v2.4 would turn the number into a name on save
    assert b"(13)" in path.read_bytes()
    assert at.read_tags(at.open_audio(path))["genre"] == "Pop"


@pytest.mark.parametrize("kind", ID3_KINDS)
def test_id3_tcon_with_a_number_and_a_refinement(at, make_audio, kind):
    path = make_audio(kind)
    audio = at.open_audio(path)
    if audio.tags is None:
        ensure_tags(audio)
    audio.tags.add(TCON(encoding=0, text="(17)Hard Rock"))
    audio.save(v2_version=3)
    assert b"(17)Hard Rock" in path.read_bytes()
    assert at.read_tags(at.open_audio(path))["genre"] == "Rock/Hard Rock"


@pytest.mark.parametrize("kind", KINDS)
def test_year_reads_from_a_full_date(at, make_audio, kind):
    path = make_audio(kind)
    audio = at.open_audio(path)
    ensure_tags(audio)
    if kind in MP4_KINDS:
        audio.tags["©day"] = ["1985-06-02T07:00:00Z"]
    else:
        audio.tags.add(TDRC(encoding=3, text="1985-06-02"))
    audio.save()
    assert at.read_tags(at.open_audio(path))["year"] == 1985


@pytest.mark.parametrize("kind", ["mp3", "aac", "wav", "aiff"])
def test_a_file_with_no_tags_reads_as_empty(at, make_audio, kind):
    path = make_audio(kind)
    audio = at.open_audio(path)
    if audio.tags is not None:  # ffmpeg adds an encoder tag; strip it
        audio.delete()
        audio = at.open_audio(path)
    assert at.read_tags(audio) == {}


def test_tags_are_written_to_a_file_that_had_none(at, make_audio):
    path = make_audio("wav")
    audio = at.open_audio(path)
    audio.delete() if audio.tags is not None else None
    at.write_tags(path, {"title": "x"})
    assert at.read_tags(at.open_audio(path)) == {"title": "x"}


# -- opening -------------------------------------------------------------------------

@pytest.mark.parametrize("name", ["a.flac", "a.ogg", "a.txt", "a"])
def test_open_audio_refuses_unsupported_files(at, tmp_path, name):
    p = tmp_path / name
    p.write_bytes(b"data")
    with pytest.raises(at.Fail) as e:
        at.open_audio(p)
    assert str(e.value) == "not a supported file (MP3, M4A, WAV or AIFF)"


@pytest.mark.parametrize("suffix", [".mp3", ".m4a", ".wav", ".aiff", ".aif"])
def test_open_audio_names_a_file_it_cannot_read(at, tmp_path, suffix):
    p = tmp_path / f"broken{suffix}"
    p.write_bytes(b"this is not audio" * 20)
    with pytest.raises(at.Fail) as e:
        at.open_audio(p)
    assert "cannot read" in str(e.value) and p.name in str(e.value)


def test_extensions_are_case_insensitive(at, make_audio, tmp_path):
    path = make_audio("mp3", name="LOUD.MP3")
    assert at.read_tags(at.open_audio(path)) is not None


def test_a_failed_write_to_an_unreadable_file_changes_nothing(at, tmp_path):
    p = tmp_path / "broken.mp3"
    p.write_bytes(b"this is not audio" * 20)
    before = p.read_bytes()
    with pytest.raises(at.Fail):
        at.write_tags(p, {"title": "x"})
    assert p.read_bytes() == before
