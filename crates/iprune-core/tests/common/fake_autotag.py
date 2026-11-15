#!/usr/bin/env python3
"""A fake `autotag serve` for iprune's tests: standard library only, no network.

It speaks the protocol of tools/tests/protocol/README.md. Answers come from a
scenario file, `fake-autotag.json`, in the folder that holds the library root
named by `--library` (so every test, with its own temp folder, has its own).
Next to it the fake writes:

  fake-autotag.log   every request line it received, as it came
  fake-autotag.pid   "<pid> <process group> <parent pid>"

Scenario keys, all optional:

  hello          the hello object, instead of the real one
  hello_raw      a line written instead of the hello (not JSON, for instance)
  stderr         lines written to stderr when a quirk fires
  lookups        {file basename: answer, or {"error": message}}; a file not
                 listed has no candidate
  tags           {recording id: tags object}; others are an error
  destinations   {file basename: destination}, else built from the tags
  same_title     {file basename: [paths]}; refuses unless allow_duplicate
  write_tags     false: file the bytes as they are (the read-back then fails
                 for any field that differs from the file's own tags)
  tag_override   {field: value} written instead of what was sent
  apply_errors   {file basename: message}: apply answers with an error
  corrupt        [file basenames] filed as bytes that are not audio
  quirks         {"start": "exit" before the hello, or "<n>" or "<op>:<basename>": quirk}, n counting requests
                 from 1. Quirks: exit (no answer), garbage (a line that is
                 not JSON), wrong_id, no_id, hang (never answers: sleeps 5 s, then
                 exits, for a Stop that has to kill it first), slow (answers
                 normally, half a second late)
  hang_on_eof    when stdin closes, sleep instead of exiting
  wrapper        start as `uv run` does: this process only runs a second
                 copy of itself as its child (not exec) and waits for it;
                 writes fake-autotag.wrapper.pid ("<pid> <process group>")
quirk file_then_exit (for apply): files the song, then exits without answering
"""

import json
import os
import subprocess
import sys
import time

lib = sys.argv[sys.argv.index("--library") + 1]
state = os.path.dirname(lib.rstrip("/"))


def at(name):
    return os.path.join(state, name)


try:
    with open(at("fake-autotag.json")) as f:
        sc = json.load(f)
except FileNotFoundError:
    sc = {}

if sc.get("wrapper") and not os.environ.get("FAKE_AUTOTAG_INNER"):
    with open(at("fake-autotag.wrapper.pid"), "w") as f:
        f.write(f"{os.getpid()} {os.getpgrp()}\n")
    inner = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__)] + sys.argv[1:],
        env=dict(os.environ, FAKE_AUTOTAG_INNER="1"),
    )
    sys.exit(inner.wait())

with open(at("fake-autotag.pid"), "w") as f:
    f.write(f"{os.getpid()} {os.getpgrp()} {os.getppid()}\n")
log = open(at("fake-autotag.log"), "a")


def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


silent = False


def say(req, obj):
    if silent:
        return
    if "id" in req:
        obj = dict(obj, id=req["id"])
    out(obj)


def complain():
    for line in sc.get("stderr", []):
        sys.stderr.write(line + "\n")
    sys.stderr.flush()


if sc.get("quirks", {}).get("start") == "exit":
    complain()
    sys.exit(2)

if "hello_raw" in sc:
    sys.stdout.write(sc["hello_raw"] + "\n")
    sys.stdout.flush()
else:
    out(
        sc.get(
            "hello",
            {
                "protocol": 1,
                "formats": ["mp3", "m4a", "wav", "aiff"],
                "acoustid": False,
                "fpcalc": False,
            },
        )
    )


def syncsafe(n):
    return bytes([(n >> 21) & 127, (n >> 14) & 127, (n >> 7) & 127, n & 127])


def frame(fid, text):
    body = b"\x03" + text.encode("utf-8")
    return fid.encode() + syncsafe(len(body)) + b"\0\0" + body


def number(n, total):
    return f"{n}/{total}" if total else str(n)


def id3_tag(tags):
    frames = []
    for key, fid in [
        ("title", "TIT2"),
        ("artist", "TPE1"),
        ("album", "TALB"),
        ("album_artist", "TPE2"),
        ("composer", "TCOM"),
        ("genre", "TCON"),
    ]:
        if tags.get(key):
            frames.append(frame(fid, tags[key]))
    if tags.get("year"):
        frames.append(frame("TDRC", str(tags["year"])))
    if tags.get("track_no"):
        frames.append(frame("TRCK", number(tags["track_no"], tags.get("track_total"))))
    if tags.get("disc_no"):
        frames.append(frame("TPOS", number(tags["disc_no"], tags.get("disc_total"))))
    if tags.get("compilation"):
        frames.append(frame("TCMP", "1"))
    body = b"".join(frames)
    return b"ID3\x04\x00\x00" + syncsafe(len(body)) + body


def retag(data, tags):
    """The MP3 bytes with their ID3v2 tag replaced by one holding `tags`."""
    if data.startswith(b"ID3"):
        size = 0
        for b in data[6:10]:
            size = (size << 7) | b
        data = data[10 + size :]
    return id3_tag(tags) + data


def default_destination(tags, src):
    artist = tags.get("album_artist") or tags.get("artist") or "Unknown Artist"
    album = tags.get("album") or "Unknown Album"
    track = f"{tags['track_no']:02d} " if tags.get("track_no") else ""
    stem, ext = os.path.splitext(os.path.basename(src))
    title = tags.get("title") or stem
    return f"{artist}/{album}/{track}{title}{ext}"


def apply(req):
    src = req["file"]
    if not os.path.isfile(src):
        say(req, {"error": f"no such file: {src}"})
        return
    base = os.path.basename(src)
    if base in sc.get("apply_errors", {}):
        say(req, {"error": sc["apply_errors"][base]})
        return
    tags = dict(req.get("tags", {}))
    tags.update(sc.get("tag_override", {}))
    dest = sc.get("destinations", {}).get(base) or default_destination(tags, src)
    same = sc.get("same_title", {}).get(base, [])
    full = os.path.join(lib, dest)
    if os.path.exists(full):
        say(
            req,
            {
                "refused": f"already in the library: {dest}",
                "destination": dest,
                "same_title": same,
            },
        )
        return
    if same and not req.get("allow_duplicate"):
        say(
            req,
            {
                "refused": "same title already filed: " + ", ".join(same),
                "destination": dest,
                "same_title": same,
            },
        )
        return
    if req.get("dry_run"):
        say(req, {"destination": dest, "same_title": same, "filed": False})
        return
    with open(src, "rb") as f:
        data = f.read()
    if sc.get("write_tags", True) and src.lower().endswith(".mp3"):
        data = retag(data, tags)
    if base in sc.get("corrupt", []):
        data = b"this is not audio"
    os.makedirs(os.path.dirname(full), exist_ok=True)
    tmp = os.path.join(os.path.dirname(full), ".autotag-" + str(os.getpid()))
    with open(tmp, "wb") as f:
        f.write(data)
    os.rename(tmp, full)
    os.unlink(src)
    say(req, {"destination": dest, "same_title": same, "filed": True})


n = 0
for line in sys.stdin:
    n += 1
    log.write(line if line.endswith("\n") else line + "\n")
    log.flush()
    req = json.loads(line)
    op = req.get("op")
    base = os.path.basename(req.get("file", ""))
    quirks = sc.get("quirks", {})
    quirk = quirks.get(str(n)) or quirks.get(f"{op}:{base}")
    if quirk == "exit":
        complain()
        sys.exit(3)
    if quirk == "garbage":
        sys.stdout.write("this is not json\n")
        sys.stdout.flush()
        continue
    if quirk == "hang":
        time.sleep(5)
        sys.exit(3)
    if quirk == "slow":
        time.sleep(0.5)
    if quirk == "wrong_id":
        out({"error": "wrong id", "id": req.get("id", 0) + 100})
        continue
    if quirk == "no_id":
        out({"error": "no id"})
        continue
    if op == "lookup":
        if not os.path.isfile(req["file"]):
            say(req, {"error": f"no such file: {req['file']}"})
            continue
        empty = {
            "by": "name",
            "query": {"artist": "", "title": ""},
            "duration_ms": 0,
            "candidates": [],
        }
        say(req, sc.get("lookups", {}).get(base, empty))
    elif op == "tags":
        found = sc.get("tags", {}).get(req.get("recording"))
        if found is None:
            say(req, {"error": "not found on MusicBrainz"})
        else:
            say(req, {"tags": found})
    elif op == "apply":
        if quirk == "file_then_exit":
            silent = True
            apply(req)
            complain()
            sys.exit(3)
        apply(req)
    else:
        say(req, {"error": f"unknown op: {op!r}"})

if sc.get("hang_on_eof"):
    time.sleep(60)
