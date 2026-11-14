"""`autotag serve` as a real process: framing only.

Every request here needs no network (apply, and requests that fail before
any lookup), and the library is always a tmp dir.
"""

import json
import subprocess
import sys

import pytest

from conftest import SCRIPT, Snapshot, tree

TAGS = {"title": "Take On Me", "artist": "a-ha", "album": "Hunting High and Low", "track_no": 3}
DEST = "a-ha/Hunting High and Low/03 Take On Me.mp3"


class Proc:
    def __init__(self, library, env, extra=()):
        self.p = subprocess.Popen(
            [sys.executable, str(SCRIPT), "serve", "--library", str(library), *extra],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", env=env)
        self.hello = self.read()

    def read(self):
        line = self.p.stdout.readline()
        assert line, f"no answer; stderr: {self.p.stderr.read()}"
        assert line.endswith("\n")
        return json.loads(line)

    def send(self, req):
        self.p.stdin.write((req if isinstance(req, str) else json.dumps(req)) + "\n")
        self.p.stdin.flush()

    def ask(self, req):
        self.send(req)
        return self.read()

    def finish(self):
        self.p.stdin.close()
        rest = self.p.stdout.read()
        err = self.p.stderr.read()
        code = self.p.wait(timeout=20)
        return code, rest, err


@pytest.fixture
def proc(make_library, child_env):
    lib = make_library()
    p = Proc(lib, child_env)
    p.lib = lib
    yield p
    if p.p.poll() is None:
        p.p.kill()
        p.p.wait()
    for stream in (p.p.stdin, p.p.stdout, p.p.stderr):
        stream.close()


def test_the_first_line_is_the_hello(proc):
    assert set(proc.hello) == {"protocol", "formats", "acoustid", "fpcalc"}
    assert proc.hello["protocol"] == 1
    assert proc.hello["formats"] == ["mp3", "m4a", "wav", "aiff"]
    assert proc.hello["acoustid"] is False      # no key in the test environment
    assert isinstance(proc.hello["fpcalc"], bool)


def test_it_ends_at_eof_with_status_0_and_nothing_more_on_stdout(proc):
    code, rest, err = proc.finish()
    assert (code, rest) == (0, "")


def test_one_answer_per_request_in_order_with_the_id_echoed(proc, make_audio):
    f = make_audio("mp3")
    proc.send({"op": "apply", "file": str(f), "tags": TAGS, "dry_run": True, "id": 41})
    proc.send({"op": "nope", "id": "second"})
    proc.send({"op": "nope"})
    a, b, c = proc.read(), proc.read(), proc.read()
    assert a == {"destination": DEST, "same_title": [], "filed": False, "id": 41}
    assert b["id"] == "second" and "unknown op" in b["error"]
    assert "id" not in c and "error" in c


def test_non_json_and_non_object_lines_get_an_error_and_serving_goes_on(proc, make_audio):
    f = make_audio("mp3")
    for junk in ("hello there", "{broken", "[1,2]", "42", "null", '"s"'):
        ans = proc.ask(junk)
        assert list(ans) == ["error"], junk
    ans = proc.ask({"op": "apply", "file": str(f), "tags": TAGS, "dry_run": True})
    assert ans["filed"] is False
    assert proc.finish()[0] == 0


def test_blank_lines_are_ignored(proc):
    proc.send("")
    proc.send("   ")
    assert proc.ask({"op": "nope", "id": 1})["id"] == 1


def test_an_error_does_not_stop_the_next_request_and_filing_still_works(proc, make_audio):
    f = make_audio("mp3")
    assert "error" in proc.ask({"op": "apply", "file": str(f), "tags": {"bogus": 1}})
    assert "error" in proc.ask({"op": "apply", "file": "relative.mp3", "tags": TAGS})
    ans = proc.ask({"op": "apply", "file": str(f), "tags": TAGS})
    assert ans == {"destination": DEST, "same_title": [], "filed": True}
    assert (proc.lib / DEST).is_file() and not f.exists()


def test_a_refusal_is_an_answer_not_an_error(proc, make_audio):
    f1, f2 = make_audio("mp3"), make_audio("mp3")
    proc.ask({"op": "apply", "file": str(f1), "tags": TAGS})
    snap, before = Snapshot(f2), tree(proc.lib)
    ans = proc.ask({"op": "apply", "file": str(f2), "tags": TAGS})
    assert "refused" in ans and "error" not in ans and ans["destination"] == DEST
    snap.assert_unchanged()
    assert tree(proc.lib) == before


def test_stdout_carries_only_json_lines(proc, make_audio):
    f = make_audio("mp3")
    proc.send("not json")
    proc.send({"op": "apply", "file": str(f), "tags": TAGS})
    proc.send({"op": "lookup", "file": "/nonexistent.mp3"})
    lines = [proc.read() for _ in range(3)]
    code, rest, err = proc.finish()
    assert rest == "" and code == 0
    assert all(isinstance(l, dict) for l in lines)


def test_unicode_survives_the_pipe(proc, make_audio):
    f = make_audio("mp3", name="Déjà vu.mp3")
    ans = proc.ask({"op": "apply", "file": str(f), "dry_run": True,
                    "tags": {"title": "Déjà vu", "artist": "Beyoncé", "album": "Lemonade – Deluxe"}})
    assert ans["destination"] == "Beyoncé/Lemonade – Deluxe/Déjà vu.mp3"


def test_a_missing_library_is_an_error_answer_not_a_crash(make_audio, child_env, tmp_path):
    p = Proc(tmp_path / "not-mounted", child_env)
    try:
        f = make_audio("mp3")
        snap = Snapshot(f)
        ans = p.ask({"op": "apply", "file": str(f), "tags": TAGS})
        assert "error" in ans
        assert p.ask({"op": "nope"})["error"]       # still alive
        snap.assert_unchanged()
        assert p.finish()[0] == 0
        assert not (tmp_path / "not-mounted").exists()
    finally:
        if p.p.poll() is None:
            p.p.kill()
            p.p.wait()
        for stream in (p.p.stdin, p.p.stdout, p.p.stderr):
            stream.close()


def test_the_key_in_the_environment_shows_in_the_hello(make_library, child_env):
    env = dict(child_env, ACOUSTID_KEY="abc")
    p = Proc(make_library(), env)
    try:
        assert p.hello["acoustid"] is True
        assert p.finish()[0] == 0
    finally:
        for stream in (p.p.stdin, p.p.stdout, p.p.stderr):
            stream.close()


def test_invalid_utf8_on_stdin_gets_an_error_and_serving_goes_on(proc):
    proc.p.stdin.flush()
    proc.p.stdin.buffer.write(b'\xff\xfe{"op": "nope"}\n')
    proc.p.stdin.buffer.flush()
    ans = proc.read()
    assert list(ans) == ["error"] and "not JSON" in ans["error"]
    assert proc.ask({"op": "nope", "id": 5})["id"] == 5     # still alive and in step
    assert proc.finish()[0] == 0
