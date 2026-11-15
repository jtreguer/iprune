//! iprune's half of the autotag protocol contract (technical document,
//! section 16): every request and answer in `tools/tests/protocol` is parsed
//! with iprune's own types, so a change on either side fails a test. The
//! conventions are those of `tools/tests/protocol/README.md`.

use std::fs;
use std::path::PathBuf;

use iprune_core::import::autotag::PROTOCOL;
use iprune_core::import::{Fields, Hello, Lookup};
use serde_json::Value;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/tests/protocol")
}

/// `$INBOX` and `$LIBRARY` replaced in every string, as the README says.
fn fill(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(
            s.replace("$INBOX", "/tmp/inbox")
                .replace("$LIBRARY", "/tmp/library"),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(fill).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, fill(v))).collect()),
        other => other,
    }
}

/// `(file name, line number, request, answer)` of every `*.jsonl` line.
fn pairs() -> Vec<(String, usize, Value, Value)> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 10,
        "the contract files are there: {} found in {}",
        files.len(),
        dir().display()
    );
    let mut out = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_str().unwrap().to_owned();
        for (n, line) in fs::read_to_string(&f).unwrap().lines().enumerate() {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{name}:{}: not JSON ({e})", n + 1));
            let o = v.as_object().expect("a line is an object");
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            assert_eq!(keys, ["answer", "request"], "{name}:{}", n + 1);
            out.push((
                name.clone(),
                n + 1,
                fill(v["request"].clone()),
                fill(v["answer"].clone()),
            ));
        }
    }
    out
}

fn is_error(a: &Value) -> bool {
    a.get("error").is_some()
}

#[test]
fn the_hello_line_parses_as_a_hello_of_this_protocol() {
    let text = fs::read_to_string(dir().join("hello.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v.as_object().unwrap().len(),
        1,
        "{{\"answer\": ...}} and nothing else"
    );
    let answer = &v["answer"];
    let hello: Hello = serde_json::from_value(answer.clone()).unwrap();
    assert_eq!(
        hello.protocol, PROTOCOL,
        "both sides speak the same version"
    );
    for f in ["mp3", "m4a", "wav", "aiff"] {
        assert!(
            hello.formats.iter().any(|x| x == f),
            "{f} in {:?}",
            hello.formats
        );
    }
    // acoustid and fpcalc depend on the machine: only their type is checked,
    // and Hello's bool fields already insisted on it.
    assert!(answer["acoustid"].is_boolean() && answer["fpcalc"].is_boolean());
    assert!(
        answer.get("id").is_none(),
        "the first line answers no request"
    );
}

#[test]
fn every_lookup_answer_parses_and_keeps_the_genre_convention() {
    let mut n = 0;
    for (file, line, req, ans) in pairs() {
        if req["op"] != "lookup" || is_error(&ans) {
            continue;
        }
        n += 1;
        let at = format!("{file}:{line}");
        let l = Lookup::parse(&ans.to_string()).unwrap_or_else(|e| panic!("{at}: {e}"));
        let raw = ans["candidates"].as_array().unwrap();
        assert_eq!(l.candidates.len(), raw.len(), "{at}");
        for (c, r) in l.candidates.iter().zip(raw) {
            // Null: not looked up. Absent: looked up, none known. Both mean
            // no genre in the tags, and only null asks for a `tags` call.
            assert_eq!(
                c.genre_unknown,
                r["tags"].get("genre") == Some(&Value::Null),
                "{at}"
            );
            assert_eq!(
                c.tags.genre.is_some(),
                r["tags"]["genre"].is_string(),
                "{at}"
            );
            assert_eq!(c.recording, r["recording"].as_str().unwrap(), "{at}");
            assert_eq!(c.release, r["release"].as_str().unwrap(), "{at}");
            assert_eq!(c.certain, r["certain"].as_bool().unwrap(), "{at}");
            assert_eq!(c.destination.as_deref(), r["destination"].as_str(), "{at}");
            assert_eq!(c.length_ms, r["length_ms"].as_u64(), "{at}");
            assert_eq!(
                c.same_title.len(),
                r["same_title"].as_array().unwrap().len(),
                "{at}"
            );
        }
    }
    assert!(n >= 6, "{n} lookup answers found");
}

#[test]
fn the_recorded_lookups_say_what_their_scenario_says() {
    let all = pairs();
    let lookup = |file: &str, line: usize| -> Lookup {
        let (_, _, _, ans) = all.iter().find(|p| p.0 == file && p.1 == line).unwrap();
        Lookup::parse(&ans.to_string()).unwrap()
    };

    let l = lookup("lookup.jsonl", 1);
    assert_eq!(l.candidates.len(), 3);
    assert_eq!(
        l.candidates
            .iter()
            .map(|c| c.genre_unknown)
            .collect::<Vec<_>>(),
        [false, true, true],
        "only the first candidate's genre is looked up"
    );
    assert_eq!(l.candidates[0].tags.genre.as_deref(), Some("Synth-Pop"));
    assert!(l.candidates[0].certain && !l.candidates[1].certain);
    assert_eq!(l.candidates[1].length_ms, None);
    assert_eq!(
        l.candidates[0].same_title,
        ["Compilations/Old Hits/01 Take On Me.mp3"]
    );
    // `query` is given even when the lookup is by name; Search again fills it.
    assert_eq!(
        (l.query.artist.as_str(), l.query.title.as_str()),
        ("a-ha", "Take On Me")
    );

    assert_eq!(lookup("lookup.jsonl", 2).candidates.len(), 1, "choices: 1");

    let l = lookup("lookup-fingerprint.jsonl", 1);
    assert_eq!(l.by, iprune_core::import::By::Fingerprint);
    assert!(l.candidates[0].certain);

    let l = lookup("lookup-no-match.jsonl", 1);
    assert!(l.candidates.is_empty());

    // A library not mounted: no destination for any candidate.
    let l = lookup("lookup-no-library.jsonl", 1);
    assert!(l.candidates.iter().all(|c| c.destination.is_none()));

    // A first candidate MusicBrainz has no genre for: absent, not null.
    let l = lookup("lookup-no-genre.jsonl", 1);
    assert!(!l.candidates[0].genre_unknown && l.candidates[0].tags.genre.is_none());
    assert!(l.candidates[1].genre_unknown);
}

#[test]
fn every_tags_answer_parses_as_fields_and_the_errors_carry_a_message() {
    let mut tags = 0;
    for (file, line, req, ans) in pairs() {
        if req["op"] != "tags" {
            continue;
        }
        let at = format!("{file}:{line}");
        if is_error(&ans) {
            assert!(ans["error"].as_str().is_some_and(|e| !e.is_empty()), "{at}");
            continue;
        }
        tags += 1;
        let f: Fields =
            serde_json::from_value(ans["tags"].clone()).unwrap_or_else(|e| panic!("{at}: {e}"));
        assert!(f.genre.is_some(), "{at}: `tags` answers with the genre");
        assert!(
            f.title.is_some() && f.year.is_some() && f.track_no.is_some(),
            "{at}"
        );
    }
    assert!(tags >= 2, "{tags} tags answers found");
}

#[test]
fn every_error_answer_is_a_string_and_fails_only_its_request() {
    let mut errors = 0;
    for (file, line, _, ans) in pairs() {
        if !is_error(&ans) {
            continue;
        }
        errors += 1;
        let at = format!("{file}:{line}");
        assert!(ans["error"].is_string(), "{at}");
        let keys: Vec<&String> = ans.as_object().unwrap().keys().collect();
        assert!(
            keys.iter().all(|k| *k == "error" || *k == "id"),
            "{at}: {keys:?}"
        );
    }
    assert!(errors >= 8, "{errors} error answers found");
}

#[test]
fn the_tags_iprune_sends_serialize_exactly_as_the_requests_of_the_contract_have_them() {
    // Field names and types, left-out fields left out (not null).
    let mut n = 0;
    for (file, line, req, _) in pairs() {
        let Some(sent) = req.get("tags") else {
            continue;
        };
        let Ok(fields) = serde_json::from_value::<Fields>(sent.clone()) else {
            continue; // an invalid request, such as a year of 99 as text
        };
        n += 1;
        assert_eq!(
            serde_json::to_value(&fields).unwrap(),
            *sent,
            "{file}:{line}"
        );
    }
    assert!(n >= 5, "{n} requests with tags");
}

#[test]
fn every_answer_has_a_shape_iprune_reads() {
    // A new kind of answer in the contract must be taught to iprune: this
    // fails until a test above covers it.
    for (file, line, req, ans) in pairs() {
        let at = format!("{file}:{line}");
        let kind = if is_error(&ans) {
            "error"
        } else if ans.get("candidates").is_some() {
            "lookup"
        } else if ans.get("tags").is_some() {
            "tags"
        } else if ans.get("refused").is_some() || ans.get("filed").is_some() {
            "apply"
        } else {
            "unknown"
        };
        let expected = match req["op"].as_str().unwrap() {
            "lookup" if kind != "error" => "lookup",
            "tags" if kind != "error" => "tags",
            "apply" if kind != "error" => "apply",
            _ => "error",
        };
        assert_eq!(kind, expected, "{at}");
        // The id is echoed when the request had one, and only then.
        assert_eq!(ans.get("id"), req.get("id"), "{at}");
    }
}
