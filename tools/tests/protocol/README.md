# autotag protocol contract

These files are the contract between `tools/autotag serve` and iprune's
autotag client (`docs/technical.md` section 7.2). autotag's tests
(`tools/tests/test_protocol.py`) check its real answers against them; iprune's
tests parse them too, so a change on one side fails the other's tests.

Conventions the Rust side must follow:

- **Format.** `hello.json` is `{"answer": {...}}`: the first line `serve`
  writes. Every `*.jsonl` file has one JSON object per line,
  `{"request": {...}, "answer": {...}}`.
- **Order.** The lines of one file are played in order against one server (one
  `autotag serve` process, or one `Server`). A later line depends on the
  earlier ones: a song filed on line 2 makes line 3 a duplicate, and a
  candidate seen in a lookup is known to a later `tags`. Files are independent
  of each other; each starts with a fresh server and a fresh library.
- **Placeholders.** `$INBOX` and `$LIBRARY` stand for the absolute path of the
  folder holding the files named in requests and for the library root. Replace
  them in every string, in requests and answers, before sending or comparing.
  Answers carry paths relative to the library, except messages that quote a
  file the request named (`no such file: $INBOX/...`).
- **Comparison.** Answers match exactly (same keys, same values, same types:
  `true` is not `1`, `null` is not an absent key), apart from `duration_ms`
  and `length_ms`, which may differ by up to 150 ms with the encoder. Lists
  keep their order. `id` is echoed when the request had one, whatever its
  type, and left out when it had none.
- **Hello.** Compare `protocol` and `formats` only. `acoustid` (a key is
  configured) and `fpcalc` (the program is installed) depend on the machine and
  must not be compared; check that they are booleans.
- **Genre.** In a lookup, a candidate's `tags.genre` is `null` when its genre
  was not looked up (every candidate after the first), and absent when it was
  looked up and MusicBrainz has none. Both mean "no genre known".
- **Refused and error.** `refused` is an answer about one file (the destination
  is taken, or the same title is filed); the answer also carries `destination`
  and `same_title`. `error` fails the request only: bad request, missing file,
  unsupported file, failure while filing.
- **Scenario per file.** `lookup-no-match.jsonl` uses a search with no result,
  `lookup-fingerprint.jsonl` a server with an AcoustID key whose fingerprint
  found recording `8c0c2908-...`, `lookup-no-library.jsonl` a library that is
  not mounted, `lookup-no-genre.jsonl` a first candidate without any genre,
  `tags-after-lookup.jsonl` a release group with a genre, and
  `apply-same-title-two.jsonl` a library with two files of the same title
  (`a-ha/Zeta/01 Take On Me.mp3` and `a-ha/Alpha/05 Take On Me.mp3`). The
  others use the recorded MusicBrainz search for "Take On Me" by a-ha and a
  library holding `Compilations/Old Hits/01 Take On Me.mp3`; the inbox files
  are 218 second MP3s. The recorded MusicBrainz answers are in `../fixtures`.
