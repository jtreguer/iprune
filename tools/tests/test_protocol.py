"""The contract between autotag and iprune: tools/tests/protocol/*.jsonl.

Each line of a .jsonl file is {"request": {...}, "answer": {...}}; the lines
of a file are played in order against one Server, so a later line sees what
an earlier one did (a filed song makes the next one a duplicate). hello.json
holds the first line serve writes. In paths, $INBOX and $LIBRARY stand for
the test's absolute inbox and library folders. duration_ms and length_ms may
differ a little with the encoder, so they are compared within a tolerance;
everything else must match exactly.

iprune's tests parse these same files (M9 step 2), so a change to the
protocol on either side fails the other's tests.

Each file is played with the stubbed MusicBrainz answers named in SCENARIOS.
Every $INBOX file a request names is made from a 218 second MP3 first (except
names starting with "absent", which must not exist).
"""

import io
import json
import shutil
import sys

import pytest

from conftest import PROTOCOL_DIR, REC1, fixture_json

TOLERANCE_MS = 150
APPROX = {"duration_ms", "length_ms"}

RG3 = "778b9081-fb9a-3fa0-9169-cfae8b6927ce"   # release group of recording 3

# file -> how it is played. Keys, all optional:
#   search_name: fixture answering the name search     key: AcoustID key of the server
#   fp_ids: what fingerprint_ids returns               library: "absent" (not mounted)
#   genres: {(kind, id): answer}, replacing the stub's genres (none by default: empty = no genre)
#   extra_files: files created in the library, relative paths
SCENARIOS = {
    "lookup.jsonl": {},
    "lookup-no-match.jsonl": {"search_name": "mb-search-none.json"},
    "lookup-fingerprint.jsonl": {"key": "KEY", "fp_ids": [REC1]},
    "lookup-no-library.jsonl": {"library": "absent"},
    "lookup-no-genre.jsonl": {"genres": {}},
    "tags.jsonl": {},
    "tags-after-lookup.jsonl": {"genres": {("release-group", RG3): {"genres": [{"name": "live rock", "count": 2}]}}},
    "apply.jsonl": {},
    "apply-same-title-two.jsonl": {"extra_files": ["a-ha/Zeta/01 Take On Me.mp3", "a-ha/Alpha/05 Take On Me.mp3"]},
    "error.jsonl": {},
}


def substitute(value, inbox, library):
    if isinstance(value, str):
        return value.replace("$INBOX", str(inbox)).replace("$LIBRARY", str(library))
    if isinstance(value, list):
        return [substitute(v, inbox, library) for v in value]
    if isinstance(value, dict):
        return {k: substitute(v, inbox, library) for k, v in value.items()}
    return value


def is_number(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def mismatches(expected, actual, where="answer"):
    """Differences between an expected answer and a real one, as strings."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return [f"{where}: expected an object, got {actual!r}"]
        out = []
        for key in sorted(set(expected) | set(actual)):
            if key not in actual:
                out.append(f"{where}.{key}: missing")
            elif key not in expected:
                out.append(f"{where}.{key}: not in the contract (value {actual[key]!r})")
            elif key in APPROX and is_number(expected[key]) and is_number(actual[key]):
                if abs(actual[key] - expected[key]) > TOLERANCE_MS:
                    out.append(f"{where}.{key}: {actual[key]!r} is not within {TOLERANCE_MS} of {expected[key]!r}")
            else:
                out += mismatches(expected[key], actual[key], f"{where}.{key}")
        return out
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(expected) != len(actual):
            return [f"{where}: expected {expected!r}, got {actual!r}"]
        return [m for i, (e, a) in enumerate(zip(expected, actual))
                for m in mismatches(e, a, f"{where}[{i}]")]
    if type(expected) is not type(actual) or expected != actual:    # True is not 1 here
        return [f"{where}: expected {expected!r}, got {actual!r}"]
    return []


def read_lines(name):
    lines = []
    for n, line in enumerate((PROTOCOL_DIR / name).read_text(encoding="utf-8").splitlines(), 1):
        if line.strip():
            obj = json.loads(line)
            assert set(obj) == {"request", "answer"}, f"{name}:{n}"
            lines.append(obj)
    return lines


def ensure_inbox_file(request, inbox, long_mp3):
    target = request.get("file", "")
    if target.startswith(str(inbox)) and not target.startswith(str(inbox / "absent")):
        if not (inbox / target[len(str(inbox)) + 1:]).exists():
            shutil.copy(long_mp3, target)


def play(at, net, name, tmp_path, make_library, long_mp3, monkeypatch):
    """The real answers to a contract file's requests, in order."""
    scenario = SCENARIOS[name]
    if "search_name" in scenario:
        net.search_name = fixture_json(scenario["search_name"])
    if "genres" in scenario:
        net.genres = dict(scenario["genres"])
    if "fp_ids" in scenario:
        monkeypatch.setattr(at, "fingerprint_ids", lambda path, k: scenario["fp_ids"])
    if scenario.get("library") == "absent":
        library = tmp_path / "not-mounted"
    else:
        library = make_library()
        for rel in ["Compilations/Old Hits/01 Take On Me.mp3", *scenario.get("extra_files", [])]:
            (library / rel).parent.mkdir(parents=True, exist_ok=True)
            (library / rel).write_bytes(b"x")
    inbox = tmp_path / "inbox"
    inbox.mkdir()
    server = at.Server(library, scenario.get("key", ""))
    results = []
    for entry in read_lines(name):
        request = substitute(entry["request"], inbox, library)
        ensure_inbox_file(request, inbox, long_mp3)
        results.append((entry, request, answer_to(at, server, request), inbox, library))
    return results


def answer_to(at, server, request):
    try:
        answer = server.handle(request)
    except at.Fail as e:
        answer = {"error": str(e)}
    if "id" in request:
        answer["id"] = request["id"]
    return answer


@pytest.mark.parametrize("name", sorted(SCENARIOS))
def test_autotags_answers_match_the_contract(at, net, name, tmp_path, make_library, long_mp3, monkeypatch):
    results = play(at, net, name, tmp_path, make_library, long_mp3, monkeypatch)
    assert results, f"{name} has no lines"
    for n, (entry, request, actual, inbox, library) in enumerate(results, 1):
        expected = substitute(entry["answer"], inbox, library)
        problems = mismatches(expected, actual)
        assert not problems, f"{name} line {n} ({request.get('op')}): " + "; ".join(problems)


@pytest.mark.parametrize("name", ["apply.jsonl", "error.jsonl"])
def test_the_real_process_gives_the_same_answers_as_the_contract(name, tmp_path, make_library, long_mp3, child_env):
    # The files whose requests need no network, through the framing of the real process.
    from test_serve_process import Proc

    library, inbox = make_library(), tmp_path / "inbox"
    inbox.mkdir()
    p = Proc(library, child_env)
    try:
        for n, entry in enumerate(read_lines(name), 1):
            request = substitute(entry["request"], inbox, library)
            ensure_inbox_file(request, inbox, long_mp3)
            problems = mismatches(substitute(entry["answer"], inbox, library), p.ask(request))
            assert not problems, f"{name} line {n}: " + "; ".join(problems)
        assert p.finish()[0] == 0
    finally:
        if p.p.poll() is None:
            p.p.kill()
            p.p.wait()
        for stream in (p.p.stdin, p.p.stdout, p.p.stderr):
            stream.close()


def test_the_contract_files_in_the_directory_are_all_played(at):
    on_disk = {p.name for p in PROTOCOL_DIR.glob("*.jsonl")}
    assert on_disk == set(SCENARIOS)


def test_the_contract_covers_every_op_and_every_kind_of_answer():
    answers = [e for name in SCENARIOS for e in read_lines(name)]
    ops = {e["request"]["op"] for e in answers}
    assert ops == {"lookup", "tags", "apply"} | {"frobnicate"}
    keys = [set(e["answer"]) - {"id"} for e in answers]
    assert {"by", "query", "duration_ms", "candidates"} in keys               # lookup
    assert {"tags"} in keys                                                   # tags
    assert {"destination", "same_title", "filed"} in keys                     # apply
    assert {"refused", "destination", "same_title"} in keys                   # refused
    assert {"error"} in keys
    filed = [e["answer"]["filed"] for e in answers if "filed" in e["answer"]]
    assert True in filed and False in filed
    refusals = [e["answer"]["refused"] for e in answers if "refused" in e["answer"]]
    assert any(r.startswith("already in the library") for r in refusals)
    assert any(r.startswith("same title already filed") for r in refusals)
    by = {e["answer"]["by"] for e in answers if "by" in e["answer"]}
    assert by == {"name", "fingerprint"}
    assert any(e["answer"].get("candidates") == [] for e in answers)          # no match
    candidates = [c for e in answers for c in e["answer"].get("candidates", [])]
    assert any(c["destination"] is None for c in candidates)                  # library absent
    assert any("genre" not in c["tags"] for c in candidates)                  # genre omitted
    assert any(c["tags"].get("genre") is None and "genre" in c["tags"] for c in candidates)  # null
    assert any(len(e["answer"].get("same_title", [])) == 2 for e in answers)
    assert any(isinstance(e["answer"].get("id"), str) for e in answers)


def test_the_hello_matches_the_contract(at, monkeypatch, capsys, make_library):
    monkeypatch.setattr(sys, "stdout", sys.stdout)
    monkeypatch.setattr(sys, "stdin", io.TextIOWrapper(io.BytesIO(b"")))
    monkeypatch.setattr(at.shutil, "which", lambda name: "/usr/bin/fpcalc")
    monkeypatch.setattr(at, "acoustid_key", lambda: "")
    at.serve(make_library())
    line = capsys.readouterr().out
    assert line.count("\n") == 1
    expected = json.loads((PROTOCOL_DIR / "hello.json").read_text(encoding="utf-8"))
    assert mismatches(expected["answer"], json.loads(line)) == []


def test_the_comparison_itself_catches_differences():
    # The checker is what makes the contract bite; prove it can fail.
    assert mismatches({"a": 1}, {"a": 1}) == []
    assert mismatches({"a": 1}, {"a": 2})
    assert mismatches({"a": 1}, {"a": 1, "b": 2})
    assert mismatches({"a": 1, "b": 2}, {"a": 1})
    assert mismatches({"a": True}, {"a": 1})
    assert mismatches({"a": [1, 2]}, {"a": [1]})
    assert mismatches({"a": None}, {"a": 0})
    assert mismatches({"duration_ms": 1000}, {"duration_ms": 1100}) == []
    assert mismatches({"duration_ms": 1000}, {"duration_ms": 1151})
    assert mismatches({"length_ms": None}, {"length_ms": None}) == []
    assert mismatches({"length_ms": 5}, {"length_ms": None})
    assert mismatches({"duration_ms": 5}, {"duration_ms": "5"})
