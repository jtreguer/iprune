"""Shared fixtures for the autotag tests.

No test here touches the network or anything outside a pytest tmp dir:
`get_json` is replaced for every test, the library default is pointed at a
folder that does not exist, and HOME is a tmp dir (so no AcoustID key file).

AUTOTAG_PATH names another copy of the script to test (used to run the
suite against deliberately broken code); the default is tools/autotag.
"""

import hashlib
import importlib.machinery
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import urllib.parse
from pathlib import Path

import pytest

sys.dont_write_bytecode = True  # never leave a .pyc next to tools/autotag

HERE = Path(__file__).resolve().parent
SCRIPT = Path(os.environ.get("AUTOTAG_PATH") or HERE.parent / "autotag").resolve()
FIXTURES = HERE / "fixtures"
PROTOCOL_DIR = HERE / "protocol"

KINDS = ["mp3", "aac", "alac", "wav", "aiff"]
CODECS = {
    "mp3": (".mp3", ["-c:a", "libmp3lame", "-b:a", "64k"]),
    "aac": (".m4a", ["-c:a", "aac", "-b:a", "64k"]),
    "alac": (".m4a", ["-c:a", "alac"]),
    "wav": (".wav", ["-c:a", "pcm_s16le"]),
    "aiff": (".aiff", ["-c:a", "pcm_s16be"]),
}

# Ids in mb-search-take-on-me.json.
REC1 = "8c0c2908-54c7-4abb-872e-86f33ff90853"
REL1 = "0d56dbfe-894e-470a-9997-b2489d53b6a7"
REC2 = "1c0c8409-9615-4aef-ac2b-62a83110fa81"
REL2 = "fada9ff4-f757-4e12-aabd-3c7d6237eefa"
REC3 = "a533f2c9-09b0-430a-8f21-240307966c8d"
REL3 = "90f35a24-f113-49eb-8f2e-c55df96cf6e9"


def fixture_json(name):
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


def load_module():
    loader = importlib.machinery.SourceFileLoader("autotag_under_test", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


_MODULE = load_module()


class Net:
    """A stand-in for `get_json`, answering from recorded JSON. Anything it
    does not know fails the test, so a stray request cannot go unnoticed."""

    def __init__(self, at):
        self.at = at
        self.search_name = fixture_json("mb-search-take-on-me.json")
        self.search_rid = fixture_json("mb-search-rid.json")
        self.genres = {("recording", REC1): fixture_json("mb-genres-recording.json")}
        self.acoustid = fixture_json("acoustid-lookup.json")
        self.calls = []  # (kind, detail) in order

    def kinds(self, kind):
        return [d for k, d in self.calls if k == kind]

    def __call__(self, url, data=None, retries=5):
        parts = urllib.parse.urlsplit(url)
        if url.startswith("https://api.acoustid.org/v2/lookup"):
            self.calls.append(("acoustid", urllib.parse.parse_qs(data.decode())))
            return self.acoustid
        assert url.startswith(self.at.MB + "/"), f"unexpected request: {url}"
        path = parts.path.removeprefix("/ws/2/")
        query = urllib.parse.parse_qs(parts.query)
        if path == "recording":
            q = query["query"][0]
            if q.startswith("rid:("):
                self.calls.append(("rid", q))
                return self.search_rid
            self.calls.append(("name", q))
            return self.search_name
        kind, _, mbid = path.partition("/")
        assert query.get("inc") == ["genres"], f"unexpected request: {url}"
        self.calls.append(("genre", (kind, mbid)))
        return self.genres.get((kind, mbid), {"id": mbid, "genres": []})


@pytest.fixture(autouse=True)
def at(monkeypatch, tmp_path):
    """The script as a module, with pacing off and nothing reachable."""
    mod = _MODULE
    monkeypatch.setattr(mod, "MB_INTERVAL", 0)
    monkeypatch.setattr(mod, "LIBRARY", tmp_path / "no-default-library")

    def no_network(url, *a, **k):
        raise AssertionError(f"network request without a Net stub: {url}")

    monkeypatch.setattr(mod, "get_json", no_network)
    monkeypatch.setattr(mod.urllib.request, "urlopen", no_network)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.delenv("ACOUSTID_KEY", raising=False)
    return mod


@pytest.fixture
def net(at, monkeypatch):
    stub = Net(at)
    monkeypatch.setattr(at, "get_json", stub)
    return stub


@pytest.fixture
def child_env(tmp_path):
    env = dict(os.environ)
    env.pop("ACOUSTID_KEY", None)
    env["HOME"] = str(tmp_path / "home")
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return env


# -- audio files --------------------------------------------------------------

def ffmpeg(*args):
    subprocess.run(["ffmpeg", "-v", "error", "-y", *args], check=True)


@pytest.fixture(scope="session")
def templates(tmp_path_factory):
    """One two-second file per kind, made once."""
    d = tmp_path_factory.mktemp("templates")
    out = {}
    for kind, (suffix, codec) in CODECS.items():
        path = d / f"{kind}{suffix}"
        ffmpeg("-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-ar", "44100",
               "-ac", "1", *codec, str(path))
        out[kind] = path
    return out


@pytest.fixture(scope="session")
def long_mp3(tmp_path_factory):
    """A 218 second MP3, within a second of the first recording's length."""
    path = tmp_path_factory.mktemp("long") / "long.mp3"
    ffmpeg("-f", "lavfi", "-i", "sine=frequency=330:duration=218", "-ar", "22050",
           "-ac", "1", "-c:a", "libmp3lame", "-b:a", "16k", str(path))
    return path


@pytest.fixture
def make_audio(templates, tmp_path):
    """make_audio(kind, name=None, folder=None) copies a template and returns
    its path. The default name is unique and the default folder is tmp_path/inbox."""
    counter = iter(range(1, 1000))

    def make(kind="mp3", name=None, folder=None):
        folder = Path(folder) if folder else tmp_path / "inbox"
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / (name or f"song{next(counter)}{CODECS[kind][0]}")
        shutil.copy(templates[kind], path)
        return path

    return make


@pytest.fixture
def make_library(tmp_path):
    """A library that is not empty (an empty one counts as unmounted)."""
    def make(name="library"):
        lib = tmp_path / name
        other = lib / "Someone Else" / "Some Album"
        other.mkdir(parents=True)
        (other / "01 Unrelated.mp3").write_bytes(b"x")
        return lib

    return make


def audio_md5(path):
    """MD5 of the audio stream alone, as ffmpeg decodes the packets."""
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-map", "0:a", "-c", "copy", "-f", "md5", "-"],
        capture_output=True, text=True, check=True).stdout.strip()
    assert out.startswith("MD5="), out
    return out


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def tree(root):
    """Every path under root with its size, to compare before and after."""
    root = Path(root)
    return sorted((str(p.relative_to(root)), p.stat().st_size if p.is_file() else -1)
                  for p in root.rglob("*"))


def temp_files(root):
    return [str(p) for p in Path(root).rglob(".autotag-*")]


class Snapshot:
    """A file's bytes and modification time, to prove it was not written."""

    def __init__(self, path):
        self.path = Path(path)
        self.data = self.path.read_bytes()
        self.mtime = self.path.stat().st_mtime_ns

    def assert_unchanged(self):
        assert self.path.exists(), f"{self.path} is gone"
        assert self.path.read_bytes() == self.data, f"{self.path} bytes changed"
        assert self.path.stat().st_mtime_ns == self.mtime, f"{self.path} mtime changed"
