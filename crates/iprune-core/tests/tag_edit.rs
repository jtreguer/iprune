//! `tag_edit::edit` on a throwaway indexed library: every format it writes
//! (MP3 with ID3v2.2, 2.3 and 2.4, AAC and ALAC in MP4, WAV, AIFF), what must
//! survive an edit byte for byte, the refusals, and the index afterwards.
//! Files are hand-built or made by ffmpeg in a temp dir; nothing here touches
//! a real library or iPod.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Env, Kind, fairplay_m4a, fixture, id3v24_text, mp3_with_id3v2};
use iprune_core::store::{self, TrackRow};
use iprune_core::tag_edit::{self, Changes, FileResult, Outcome, Report};
use iprune_core::tags::{self, FileKind, Tags};

// ---- ffmpeg and ffprobe, as an oracle independent of iprune's readers

fn ffmpeg(args: &[&str]) {
    let o = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg must be installed");
    assert!(
        o.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn md5_of(path: &Path, args: &[&str]) -> String {
    let o = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(args)
        .args(["-f", "md5", "-"])
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "ffmpeg md5 of {}: {}",
        path.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap().trim().to_owned()
}

/// The audio stream's packets, hashed: same hash, same audio bytes.
fn audio_md5(path: &Path) -> String {
    md5_of(path, &["-map", "0:a", "-c", "copy"])
}

/// The audio stream decoded to PCM, hashed.
fn decoded_md5(path: &Path) -> String {
    md5_of(path, &["-map", "0:a"])
}

/// The attached picture's packets, hashed; None when there is no picture.
fn picture_md5(path: &Path) -> Option<String> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v", "-show_entries"])
        .arg("stream=codec_type")
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    if String::from_utf8_lossy(&o.stdout).trim().is_empty() {
        return None;
    }
    Some(md5_of(path, &["-map", "0:v", "-c", "copy"]))
}

/// Every tag ffprobe reads, by name.
fn probe_tags(path: &Path) -> BTreeMap<String, String> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format_tags", "-of"])
        .arg("default=noprint_wrappers=1")
        .arg(path)
        .output()
        .unwrap();
    assert!(o.status.success(), "ffprobe {}", path.display());
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("TAG:"))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_lowercase(), v.to_owned()))
        .collect()
}

/// The names in ffprobe's view of the fields `c` edits.
fn edited_keys(c: &Changes) -> Vec<&'static str> {
    let mut k = Vec::new();
    let mut add = |on: bool, names: &[&'static str]| {
        if on {
            k.extend_from_slice(names);
        }
    };
    add(c.title.is_some(), &["title", "tt2"]);
    add(c.artist.is_some(), &["artist", "tp1"]);
    add(c.album.is_some(), &["album", "tal"]);
    add(c.album_artist.is_some(), &["album_artist", "tp2"]);
    add(c.composer.is_some(), &["composer", "tcm"]);
    add(c.genre.is_some(), &["genre", "tco"]);
    add(c.year.is_some(), &["date", "tye", "tda", "tim"]);
    add(
        c.track_no.is_some() || c.track_total.is_some(),
        &["track", "trk"],
    );
    add(
        c.disc_no.is_some() || c.disc_total.is_some(),
        &["disc", "tpa"],
    );
    add(c.compilation.is_some(), &["compilation", "tcmp", "tcp"]);
    k
}

// ---- ID3v2 by hand

fn syncsafe(n: usize) -> [u8; 4] {
    [
        (n >> 21 & 0x7f) as u8,
        (n >> 14 & 0x7f) as u8,
        (n >> 7 & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

fn unsyncsafe(b: &[u8]) -> usize {
    b.iter().fold(0, |n, &x| (n << 7) | x as usize)
}

/// One frame of ID3v2.`ver`: 3 letter ids and sizes in 2.2, plain sizes in
/// 2.3, syncsafe ones in 2.4.
fn frame(ver: u8, id: &str, body: &[u8]) -> Vec<u8> {
    let mut f = id.as_bytes().to_vec();
    match ver {
        2 => {
            assert_eq!(id.len(), 3);
            f.extend(&(body.len() as u32).to_be_bytes()[1..]);
        }
        3 => {
            assert_eq!(id.len(), 4);
            f.extend((body.len() as u32).to_be_bytes());
            f.extend([0, 0]);
        }
        _ => {
            assert_eq!(id.len(), 4);
            f.extend(syncsafe(body.len()));
            f.extend([0, 0]);
        }
    }
    f.extend(body);
    f
}

/// A Latin-1 text frame (UTF-8 in 2.4, where encoding 3 is allowed).
fn text(ver: u8, id: &str, s: &str) -> Vec<u8> {
    let mut b = vec![if ver == 4 { 3 } else { 0 }];
    b.extend(s.as_bytes());
    frame(ver, id, &b)
}

fn tag(ver: u8, frames: &[Vec<u8>], padding: usize) -> Vec<u8> {
    let body: Vec<u8> = frames.concat();
    let mut t = b"ID3".to_vec();
    t.extend([ver, 0, 0]);
    t.extend(syncsafe(body.len() + padding));
    t.extend(body);
    t.resize(t.len() + padding, 0);
    t
}

/// The ids of the frames of each field, in ID3v2.`ver`.
struct Ids {
    title: &'static str,
    artist: &'static str,
    album: &'static str,
    album_artist: &'static str,
    composer: &'static str,
    genre: &'static str,
    track: &'static str,
    disc: &'static str,
    comment: &'static str,
    user_text: &'static str,
}

fn ids(ver: u8) -> Ids {
    if ver == 2 {
        Ids {
            title: "TT2",
            artist: "TP1",
            album: "TAL",
            album_artist: "TP2",
            composer: "TCM",
            genre: "TCO",
            track: "TRK",
            disc: "TPA",
            comment: "COM",
            user_text: "TXX",
        }
    } else {
        Ids {
            title: "TIT2",
            artist: "TPE1",
            album: "TALB",
            album_artist: "TPE2",
            composer: "TCOM",
            genre: "TCON",
            track: "TRCK",
            disc: "TPOS",
            comment: "COMM",
            user_text: "TXXX",
        }
    }
}

/// An 8 by 8 red PNG.
const PNG_BYTES: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x02, 0x00, 0x00, 0x00, 0x4b, 0x6d, 0x29,
    0xdc, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x80,
    0x15, 0x61, 0x17, 0x1d, 0xb4, 0x12, 0x00, 0x28, 0xff, 0x3f, 0xc1, 0x6e, 0xec, 0xdf, 0x61, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A full ID3v2.`ver` tag: every field, a date in the version's own frames,
/// a comment, a user text, and (2.3 and 2.4) a PRIV and a picture, 2.2 a
/// picture too. The values start with "Old".
fn rich_tag(ver: u8, padding: usize) -> Vec<u8> {
    let i = ids(ver);
    let mut frames = vec![
        text(ver, i.title, "Old Title"),
        text(ver, i.artist, "Old Artist"),
        text(ver, i.album, "Old Album"),
        text(ver, i.album_artist, "Old Album Artist"),
        text(ver, i.composer, "Old Composer"),
        text(ver, i.genre, "Rock"),
    ];
    match ver {
        2 => {
            frames.push(text(2, "TYE", "1984"));
            frames.push(text(2, "TDA", "0101"));
            frames.push(text(2, "TIM", "1200"));
        }
        3 => {
            frames.push(text(3, "TYER", "1984"));
            frames.push(text(3, "TDAT", "0101"));
            frames.push(text(3, "TIME", "1200"));
        }
        _ => frames.push(text(4, "TDRC", "1984-01-01")),
    }
    frames.push(text(ver, i.track, "2/9"));
    frames.push(text(ver, i.disc, "1/2"));
    // COMM / COM: encoding, language, short description, text.
    frames.push(frame(
        ver,
        i.comment,
        b"\x00engnote\x00this comment must survive",
    ));
    // TXXX / TXX: encoding, description, value.
    frames.push(frame(ver, i.user_text, b"\x00MYKEY\x00my value"));
    if ver == 2 {
        // PIC: encoding, image format, picture type, description, data.
        let mut pic = b"\x00PNG\x03front\x00".to_vec();
        pic.extend(PNG_BYTES);
        frames.push(frame(2, "PIC", &pic));
        // UFI: owner, identifier; a frame iprune has no field for.
        frames.push(frame(2, "UFI", b"owner@example\x00\x01\x02\x03"));
    } else {
        let mut apic = b"\x00image/png\x00\x03front\x00".to_vec();
        apic.extend(PNG_BYTES);
        frames.push(frame(ver, "APIC", &apic));
        frames.push(frame(ver, "PRIV", b"owner@example\x00\x01\x02\xff\xfe"));
    }
    tag(ver, &frames, padding)
}

/// The audio of the cached Mp3 fixture `variant`, with no tag in front.
fn mp3_audio(variant: u32) -> Vec<u8> {
    let raw = fs::read(fixture(Kind::Mp3, variant)).unwrap();
    if raw.starts_with(b"ID3") {
        let size = unsyncsafe(&raw[6..10]);
        raw[10 + size..].to_vec()
    } else {
        raw
    }
}

/// The frames of an ID3v2 tag that starts `tag`, header included.
fn id3_frames(tag: &[u8]) -> Vec<(String, Vec<u8>)> {
    assert_eq!(&tag[..3], b"ID3");
    let ver = tag[3];
    let size = unsyncsafe(&tag[6..10]);
    let body = &tag[10..10 + size];
    let (id_len, header) = if ver == 2 { (3, 6) } else { (4, 10) };
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + header <= body.len() && body[pos] != 0 {
        let sz = match ver {
            2 => body[pos + 3..pos + 6]
                .iter()
                .fold(0, |n, &x| (n << 8) | x as usize),
            3 => body[pos + 4..pos + 8]
                .iter()
                .fold(0, |n, &x| (n << 8) | x as usize),
            _ => unsyncsafe(&body[pos + 4..pos + 8]),
        };
        let end = pos + header + sz;
        out.push((
            String::from_utf8_lossy(&body[pos..pos + id_len]).into_owned(),
            body[pos..end].to_vec(),
        ));
        pos = end;
    }
    out
}

/// The frames of `file`'s tag (at its start) other than those named.
fn frames_but(tag: &[u8], edited: &[&str]) -> Vec<(String, Vec<u8>)> {
    id3_frames(tag)
        .into_iter()
        .filter(|(id, _)| !edited.contains(&id.as_str()))
        .collect()
}

fn frame_ids(tag: &[u8]) -> Vec<String> {
    id3_frames(tag).into_iter().map(|(id, _)| id).collect()
}

fn text_of(tag: &[u8], id: &str) -> Option<String> {
    id3_frames(tag)
        .into_iter()
        .find(|(i, _)| i == id)
        .map(|(_, b)| {
            let body = &b[if id.len() == 3 { 6 } else { 10 }..];
            match body[0] {
                0 => body[1..].iter().map(|&c| c as char).collect(),
                3 => String::from_utf8(body[1..].to_vec()).unwrap(),
                1 => {
                    // UTF-16 with a BOM.
                    let le = body[1..3] == [0xff, 0xfe];
                    let units: Vec<u16> = body[3..]
                        .chunks(2)
                        .map(|c| {
                            if le {
                                u16::from_le_bytes([c[0], c[1]])
                            } else {
                                u16::from_be_bytes([c[0], c[1]])
                            }
                        })
                        .collect();
                    String::from_utf16(&units).unwrap()
                }
                e => panic!("encoding {e}"),
            }
        })
}

// ---- RIFF and IFF chunks

fn chunks(data: &[u8]) -> Vec<(String, Vec<u8>)> {
    let little = &data[..4] == b"RIFF";
    let read = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        (if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }) as usize
    };
    let end = 8 + read(&data[4..8]);
    let mut out = Vec::new();
    let mut pos = 12;
    while pos + 8 <= end {
        let size = read(&data[pos + 4..pos + 8]);
        out.push((
            String::from_utf8_lossy(&data[pos..pos + 4]).into_owned(),
            data[pos + 8..pos + 8 + size].to_vec(),
        ));
        pos += 8 + size + (size & 1);
    }
    out
}

fn is_id3_chunk(name: &str) -> bool {
    name.eq_ignore_ascii_case("id3 ")
}

/// A WAV with a chunk added at the end and the RIFF size fixed.
fn wav_with_chunk(wav: &[u8], name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = wav.to_vec();
    out.extend_from_slice(name);
    out.extend((body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    let size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    out
}

// ---- MP4 atoms

type Atom = ([u8; 4], usize, usize);

fn atoms(d: &[u8], from: usize, to: usize) -> Vec<Atom> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos + 8 <= to {
        let size = u32::from_be_bytes(d[pos..pos + 4].try_into().unwrap()) as usize;
        out.push((d[pos + 4..pos + 8].try_into().unwrap(), pos, pos + size));
        pos += size;
    }
    out
}

fn child(d: &[u8], parent: Atom, name: &[u8; 4]) -> Option<Atom> {
    // `meta` is a full atom: 4 bytes of version and flags first.
    let from = parent.1 + if &parent.0 == b"meta" { 12 } else { 8 };
    atoms(d, from, parent.2).into_iter().find(|a| &a.0 == name)
}

fn top(d: &[u8], name: &[u8; 4]) -> Atom {
    atoms(d, 0, d.len())
        .into_iter()
        .find(|a| &a.0 == name)
        .unwrap_or_else(|| panic!("no {}", String::from_utf8_lossy(name)))
}

struct Loc {
    moov: Atom,
    udta: Atom,
    meta: Atom,
    ilst: Atom,
}

fn locate(d: &[u8]) -> Loc {
    let moov = top(d, b"moov");
    let udta = child(d, moov, b"udta").unwrap();
    let meta = child(d, udta, b"meta").unwrap();
    let ilst = child(d, meta, b"ilst").unwrap();
    Loc {
        moov,
        udta,
        meta,
        ilst,
    }
}

/// The `ilst` items in order, header included.
fn ilst_items(d: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let l = locate(d);
    atoms(d, l.ilst.1 + 8, l.ilst.2)
        .into_iter()
        .map(|(n, s, e)| (n, d[s..e].to_vec()))
        .collect()
}

fn ilst_but(d: &[u8], edited: &[&[u8; 4]]) -> Vec<([u8; 4], Vec<u8>)> {
    ilst_items(d)
        .into_iter()
        .filter(|(n, _)| !edited.contains(&n))
        .collect()
}

fn stco_entries(d: &[u8]) -> Vec<u32> {
    let moov = top(d, b"moov");
    let mut out = Vec::new();
    for trak in atoms(d, moov.1 + 8, moov.2)
        .into_iter()
        .filter(|a| &a.0 == b"trak")
    {
        let mdia = child(d, trak, b"mdia").unwrap();
        let minf = child(d, mdia, b"minf").unwrap();
        let stbl = child(d, minf, b"stbl").unwrap();
        let stco = child(d, stbl, b"stco").unwrap();
        let n = u32::from_be_bytes(d[stco.1 + 12..stco.1 + 16].try_into().unwrap()) as usize;
        for i in 0..n {
            let at = stco.1 + 16 + 4 * i;
            out.push(u32::from_be_bytes(d[at..at + 4].try_into().unwrap()));
        }
    }
    out
}

fn mp4_atom(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut a = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    a.extend_from_slice(name);
    a.extend_from_slice(body);
    a
}

/// An `ilst` item: the atom `name` holding one `data` atom.
fn mp4_item(name: &[u8; 4], kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut data = kind.to_be_bytes().to_vec();
    data.extend([0; 4]);
    data.extend(payload);
    mp4_atom(name, &mp4_atom(b"data", &data))
}

/// `bytes` put right after the `ilst` atom of `d`, inside `meta` (as a
/// `free` atom is) or, with `inside_ilst`, at the end of `ilst` (as an item
/// is). The sizes of the containers and the chunk offsets are fixed.
fn m4a_insert(d: &[u8], bytes: &[u8], inside_ilst: bool) -> Vec<u8> {
    let l = locate(d);
    let p = l.ilst.2;
    let mut out = d[..p].to_vec();
    out.extend_from_slice(bytes);
    out.extend_from_slice(&d[p..]);
    let mut grown = vec![l.moov, l.udta, l.meta];
    if inside_ilst {
        grown.push(l.ilst);
    }
    for (_, start, end) in grown {
        let size = (end - start + bytes.len()) as u32;
        out[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
    // Chunk offsets past the insertion move with the audio.
    let moov = top(&out, b"moov");
    for trak in atoms(&out, moov.1 + 8, moov.2)
        .into_iter()
        .filter(|a| &a.0 == b"trak")
    {
        let mdia = child(&out, trak, b"mdia").unwrap();
        let minf = child(&out, mdia, b"minf").unwrap();
        let stbl = child(&out, minf, b"stbl").unwrap();
        let stco = child(&out, stbl, b"stco").unwrap();
        let n = u32::from_be_bytes(out[stco.1 + 12..stco.1 + 16].try_into().unwrap()) as usize;
        for i in 0..n {
            let at = stco.1 + 16 + 4 * i;
            let off = u32::from_be_bytes(out[at..at + 4].try_into().unwrap());
            if off as usize >= p {
                out[at..at + 4].copy_from_slice(&(off + bytes.len() as u32).to_be_bytes());
            }
        }
    }
    out
}

/// A `free` atom of `total` bytes, header included.
fn free_atom(total: usize) -> Vec<u8> {
    mp4_atom(b"free", &vec![0; total - 8])
}

/// A freeform `----` item: mean, name and a value.
fn freeform_item() -> Vec<u8> {
    let mut body = mp4_atom(b"mean", b"\0\0\0\0com.example");
    body.extend(mp4_atom(b"name", b"\0\0\0\0MYKEY"));
    let mut data = 1u32.to_be_bytes().to_vec();
    data.extend([0; 4]);
    data.extend(b"my value");
    body.extend(mp4_atom(b"data", &data));
    mp4_atom(b"----", &body)
}

// ---- generating files

const META: [&str; 20] = [
    "-metadata",
    "title=Old Title",
    "-metadata",
    "artist=Old Artist",
    "-metadata",
    "album=Old Album",
    "-metadata",
    "album_artist=Old Album Artist",
    "-metadata",
    "composer=Old Composer",
    "-metadata",
    "date=1984",
    "-metadata",
    "track=2/9",
    "-metadata",
    "disc=1/2",
    "-metadata",
    "comment=a comment that must survive",
    "-metadata",
    "genre=Rock",
];

struct Work {
    dir: tempfile::TempDir,
}

impl Work {
    fn new() -> Work {
        Work {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn cover(&self) -> PathBuf {
        let p = self.path("cover.png");
        if !p.exists() {
            ffmpeg(&[
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=32x32",
                "-frames:v",
                "1",
                p.to_str().unwrap(),
            ]);
        }
        p
    }

    /// An audio file made by ffmpeg with the fields above, and a cover when
    /// asked. `fmt` is the muxer, `opts` codec and muxer options.
    fn make(&self, name: &str, fmt: &str, opts: &[&str], cover: bool) -> Vec<u8> {
        let out = self.path(name);
        let mut args: Vec<String> = ["-f", "lavfi", "-i", "sine=frequency=330:duration=1"]
            .map(String::from)
            .to_vec();
        if cover {
            args.extend(["-i".into(), self.cover().to_str().unwrap().into()]);
            args.extend(["-map", "0:a", "-map", "1:v", "-c:v", "copy"].map(String::from));
            args.extend(["-disposition:v:0", "attached_pic"].map(String::from));
        }
        args.extend(META.map(String::from));
        args.extend(opts.iter().map(|s| s.to_string()));
        args.extend(["-f".into(), fmt.into(), out.to_str().unwrap().into()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        ffmpeg(&refs);
        fs::read(out).unwrap()
    }

    fn mp3(&self, id3v2_version: &str, cover: bool) -> Vec<u8> {
        self.make(
            "x.mp3",
            "mp3",
            &["-c:a", "libmp3lame", "-id3v2_version", id3v2_version],
            cover,
        )
    }

    /// An m4a with its `moov` before the audio (faststart) or after it.
    fn m4a(&self, codec: &str, faststart: bool, cover: bool) -> Vec<u8> {
        let mut opts = vec!["-c:a", codec];
        if faststart {
            opts.extend(["-movflags", "+faststart"]);
        }
        self.make("x.m4a", "ipod", &opts, cover)
    }
}

// ---- the library

struct Lib {
    env: Env,
    path: PathBuf,
    id: i64,
    before: TrackRow,
    bytes: Vec<u8>,
    mode: u32,
}

impl Lib {
    /// `bytes` as `rel` in a new library, scanned.
    fn new(rel: &str, bytes: &[u8]) -> Lib {
        let mut env = Env::new();
        let path = env.add_bytes(rel, bytes);
        let r = env.scan();
        assert_eq!(r.added, 1, "{r:?}");
        let before = env.track(&path);
        assert_eq!(before.tag_error, None, "the fixture's tags must read");
        assert!(!before.drm);
        // A mode the temporary file would not have by default.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let r = env.scan();
        assert_eq!(r.unchanged, 1, "{r:?}");
        Lib {
            mode: 0o640,
            id: before.id,
            env,
            path,
            before,
            bytes: bytes.to_vec(),
        }
    }

    fn edit(&self, changes: &Changes) -> Report {
        edit_ids(&self.env, &[self.id], changes)
    }

    fn now(&self) -> Vec<u8> {
        fs::read(&self.path).unwrap()
    }

    fn row(&self) -> TrackRow {
        store::track(self.env.store.conn(), self.id)
            .unwrap()
            .expect("the track row")
    }

    /// Hidden files left in the folder of the track.
    fn stray_files(&self) -> Vec<String> {
        fs::read_dir(self.path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect()
    }
}

fn edit_ids(env: &Env, ids: &[i64], changes: &Changes) -> Report {
    tag_edit::edit(
        env.store.conn(),
        ids,
        changes,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap()
}

fn some(s: &str) -> Option<Option<String>> {
    Some(Some(s.to_owned()))
}

/// The edit most tests make: six fields, the rest left alone.
fn full_changes() -> Changes {
    Changes {
        title: some("New Title"),
        artist: some("New Artist"),
        genre: some("Electronic"),
        year: Some(Some(1999)),
        track_no: Some(Some(3)),
        track_total: Some(Some(12)),
        compilation: Some(true),
        ..Changes::default()
    }
}

fn only(report: &Report) -> &Outcome {
    assert_eq!(report.results.len(), 1, "{report:?}");
    &report.results[0].outcome
}

/// What every successful edit must leave behind: the file read back, the
/// row, the audio, a scan that finds nothing to do.
fn assert_written(lib: &mut Lib, report: &Report, changes: &Changes) {
    assert_eq!(only(report), &Outcome::Written, "{report:?}");
    assert!(!report.stopped);
    let after = lib.now();
    let expected = changes.cleaned().apply(&lib.before.tags);
    assert_ne!(expected, lib.before.tags, "the edit must change something");

    // The file reads back with the new values and nothing else changed.
    let read = tags::read(&lib.path).unwrap();
    assert_eq!(read.tag_error, None);
    assert_eq!(read.tags, expected);
    assert_eq!(read.info.kind, lib.before.info.kind);
    assert_eq!(read.info.duration_ms, lib.before.info.duration_ms);

    // The row: same id, path and first-seen time; size, time, hash and tags
    // of the new file.
    let row = lib.row();
    let meta = fs::metadata(&lib.path).unwrap();
    assert_eq!(
        meta.mode() & 0o777,
        lib.mode,
        "the file keeps its permissions"
    );
    assert_eq!(row.id, lib.before.id);
    assert_eq!(row.path, lib.before.path);
    assert_eq!(row.path_key, lib.before.path_key);
    assert_eq!(row.added_at, lib.before.added_at);
    assert_eq!(row.size, meta.size());
    assert_eq!(row.size, after.len() as u64);
    assert_eq!(
        row.mtime_ns,
        meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
    );
    assert_eq!(row.hash, *blake3::hash(&after).as_bytes());
    assert_ne!(row.hash, lib.before.hash, "the bytes changed");
    assert_eq!(row.tags, expected);
    assert_eq!(row.tags_hash, expected.hash());
    assert_ne!(row.tags_hash, lib.before.tags_hash);
    assert_eq!(row.info, read.info);
    assert_eq!(row.tag_error, None);
    assert_eq!(row.missing_since, None);

    // No temporary file left, the folder holds the one track.
    assert_eq!(lib.stray_files(), Vec::<String>::new());

    // The audio and the picture are the same.
    let orig = lib.env.music.parent().unwrap().join("orig-copy");
    fs::write(&orig, &lib.bytes).unwrap();
    let ext = lib.path.extension().unwrap().to_string_lossy().into_owned();
    let orig = {
        let renamed = orig.with_extension(ext);
        fs::rename(&orig, &renamed).unwrap();
        renamed
    };
    assert_eq!(audio_md5(&lib.path), audio_md5(&orig), "audio packets");
    assert_eq!(decoded_md5(&lib.path), decoded_md5(&orig), "decoded audio");
    assert_eq!(picture_md5(&lib.path), picture_md5(&orig), "picture");

    // Tags ffprobe reads, other than the edited ones, are as they were.
    let strip = |mut m: BTreeMap<String, String>| {
        for k in edited_keys(changes) {
            m.remove(k);
        }
        m
    };
    assert_eq!(
        strip(probe_tags(&lib.path)),
        strip(probe_tags(&orig)),
        "ffprobe tags other than the edited ones"
    );

    // A scan sees an unchanged file, keeps the row, finds nothing to update.
    let r = lib.env.scan();
    assert_eq!(
        (r.unchanged, r.added, r.updated, r.returned),
        (1, 0, 0, 0),
        "{r:?}"
    );
    assert!(r.moved.is_empty() && r.missing.is_empty(), "{r:?}");
    assert_eq!(lib.env.tracks().len(), 1);
    assert_eq!(lib.row().tags, expected);
}

// =============================== MP3 ===============================

fn mp3_case(ver: u8, padding: usize) -> Lib {
    let mut bytes = rich_tag(ver, padding);
    bytes.extend(mp3_audio(1));
    Lib::new("a/01.mp3", &bytes)
}

/// The ids of the frames `full_changes` replaces.
fn edited_frames(ver: u8) -> Vec<&'static str> {
    match ver {
        2 => vec!["TT2", "TP1", "TCO", "TYE", "TDA", "TIM", "TRK", "TCP"],
        3 => vec![
            "TIT2", "TPE1", "TCON", "TYER", "TDAT", "TIME", "TDRC", "TRCK", "TCMP",
        ],
        _ => vec![
            "TIT2", "TPE1", "TCON", "TDRC", "TYER", "TDAT", "TIME", "TRCK", "TCMP",
        ],
    }
}

fn check_mp3(ver: u8) {
    let mut lib = mp3_case(ver, 200);
    // Guard: the reader sees what was built.
    let t = &lib.before.tags;
    assert_eq!(t.title.as_deref(), Some("Old Title"));
    assert_eq!(t.album.as_deref(), Some("Old Album"));
    assert_eq!(t.album_artist.as_deref(), Some("Old Album Artist"));
    assert_eq!(t.composer.as_deref(), Some("Old Composer"));
    assert_eq!(t.year, Some(1984));
    assert_eq!((t.track_no, t.track_total), (Some(2), Some(9)));
    assert_eq!((t.disc_no, t.disc_total), (Some(1), Some(2)));

    let report = lib.edit(&full_changes());
    assert_written(&mut lib, &report, &full_changes());

    let after = lib.now();
    let t = tags::read(&lib.path).unwrap().tags;
    assert_eq!(t.album.as_deref(), Some("Old Album"));
    assert_eq!(t.album_artist.as_deref(), Some("Old Album Artist"));
    assert_eq!(t.composer.as_deref(), Some("Old Composer"));
    assert_eq!((t.disc_no, t.disc_total), (Some(1), Some(2)));
    assert_eq!(t.year, Some(1999));

    // The version stays, and every frame not edited keeps its bytes, in order.
    assert_eq!(
        &after[..4],
        &lib.bytes[..3]
            .iter()
            .chain(&[ver])
            .copied()
            .collect::<Vec<u8>>()[..]
    );
    let edited = edited_frames(ver);
    assert_eq!(
        frames_but(&after, &edited),
        frames_but(&lib.bytes, &edited),
        "unedited frames: bytes and order"
    );
    let i = ids(ver);
    let kept = frame_ids(&after);
    for must in [
        i.comment,
        i.user_text,
        i.album,
        i.album_artist,
        i.composer,
        i.disc,
    ] {
        assert!(kept.iter().any(|k| k == must), "{must} in {kept:?}");
    }
    if ver == 2 {
        assert!(kept.contains(&"PIC".to_string()) && kept.contains(&"UFI".to_string()));
    } else {
        assert!(kept.contains(&"APIC".to_string()) && kept.contains(&"PRIV".to_string()));
    }
    // The year: one frame of the version, no leftover date or time frame.
    let year_frames: Vec<&String> = kept
        .iter()
        .filter(|k| ["TYE", "TDA", "TIM", "TYER", "TDAT", "TIME", "TDRC"].contains(&k.as_str()))
        .collect();
    let year_id = match ver {
        2 => "TYE",
        3 => "TYER",
        _ => "TDRC",
    };
    assert_eq!(year_frames, [year_id]);
    assert_eq!(text_of(&after, year_id).as_deref(), Some("1999"));
    // The audio after the tag is the same bytes.
    let audio = mp3_audio(1);
    assert!(after.ends_with(&audio));
    assert!(lib.bytes.ends_with(&audio));
}

#[test]
fn mp3_id3v22_keeps_its_version_frames_and_audio() {
    check_mp3(2);
}

#[test]
fn mp3_id3v23_keeps_its_version_frames_and_audio() {
    check_mp3(3);
}

#[test]
fn mp3_id3v24_keeps_its_version_frames_and_audio() {
    check_mp3(4);
}

/// ffmpeg's own ID3v2.3 and 2.4 tags, with a comment and a cover.
fn check_ffmpeg_mp3(version: &str) {
    let w = Work::new();
    let bytes = w.mp3(version, true);
    assert_eq!(bytes[3], version.parse::<u8>().unwrap());
    let mut lib = Lib::new("a/01.mp3", &bytes);
    assert!(picture_md5(&lib.path).is_some(), "set up: a cover");
    let report = lib.edit(&full_changes());
    assert_written(&mut lib, &report, &full_changes());
    let after = lib.now();
    assert_eq!(after[3], bytes[3], "tag version");
    let edited = edited_frames(bytes[3]);
    assert_eq!(frames_but(&after, &edited), frames_but(&bytes, &edited));
    let kept = frame_ids(&after);
    assert!(kept.contains(&"APIC".to_string()), "{kept:?}");
    assert!(kept.contains(&"TXXX".to_string()), "{kept:?}");
    assert_eq!(
        probe_tags(&lib.path).get("comment").map(String::as_str),
        Some("a comment that must survive")
    );
}

#[test]
fn ffmpeg_mp3_id3v23_with_cover_and_comment() {
    check_ffmpeg_mp3("3");
}

#[test]
fn ffmpeg_mp3_id3v24_with_cover_and_comment() {
    check_ffmpeg_mp3("4");
}

#[test]
fn mp3_without_a_tag_gets_an_id3v23_tag_with_every_value() {
    let mut lib = Lib::new("a/01.mp3", &mp3_audio(1));
    assert_eq!(lib.before.tags, Tags::default());
    let c = Changes {
        title: some("Brand New"),
        year: Some(Some(2001)),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let after = lib.now();
    assert_eq!(&after[..4], b"ID3\x03");
    assert!(after.ends_with(&mp3_audio(1)), "audio untouched");
    assert_eq!(frame_ids(&after), ["TIT2", "TYER"]);
}

// An MP3 whose only tag is an ID3v1 one at its end gets an ID3v2.3 tag in
// front that holds the values the index has (the reader prefers it to the
// v1 tag), and keeps the v1 tag.
#[test]
fn mp3_with_only_an_id3v1_tag_gets_a_v2_tag_with_every_value() {
    let mut v1 = b"TAG".to_vec();
    for (field, len) in [
        ("V1 Title", 30),
        ("V1 Artist", 30),
        ("V1 Album", 30),
        ("1977", 4),
    ] {
        let mut f = field.as_bytes().to_vec();
        f.resize(len, 0);
        v1.extend(f);
    }
    v1.extend([0u8; 30]); // comment
    v1.push(17); // genre: Rock
    assert_eq!(v1.len(), 128);
    let mut bytes = mp3_audio(1);
    bytes.extend(&v1);
    let mut lib = Lib::new("a/01.mp3", &bytes);
    let t = lib.before.tags.clone();
    assert_eq!(t.title.as_deref(), Some("V1 Title"), "set up: v1 is read");
    assert_eq!(t.album.as_deref(), Some("V1 Album"));
    assert_eq!(t.year, Some(1977));
    let c = Changes {
        artist: some("Edited Artist"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let after = lib.now();
    assert_eq!(&after[..4], b"ID3\x03");
    assert!(after.ends_with(&bytes), "audio and the v1 tag unchanged");
    let ids = frame_ids(&after);
    for want in ["TIT2", "TPE1", "TALB", "TYER", "TCON"] {
        assert!(ids.iter().any(|i| i == want), "{want} in {ids:?}");
    }
    assert_eq!(text_of(&after, "TIT2").as_deref(), Some("V1 Title"));
    assert_eq!(text_of(&after, "TALB").as_deref(), Some("V1 Album"));
    assert_eq!(text_of(&after, "TYER").as_deref(), Some("1977"));
}

/// A tag with padding takes a small edit without moving the audio; one with
/// none grows by padding, which then takes the next edit.
#[test]
fn id3_padding_keeps_the_audio_in_place() {
    let padded = mp3_case(3, 600);
    let c = Changes {
        title: some("Short"),
        ..Changes::default()
    };
    assert_eq!(padded.edit(&c).written(), 1);
    assert_eq!(padded.now().len(), padded.bytes.len(), "fits the padding");

    let tight = {
        let mut bytes = rich_tag(3, 0);
        bytes.extend(mp3_audio(1));
        Lib::new("a/01.mp3", &bytes)
    };
    let long = Changes {
        title: some(&"L".repeat(300)),
        ..Changes::default()
    };
    assert_eq!(tight.edit(&long).written(), 1);
    let grown = tight.now().len();
    assert!(grown >= tight.bytes.len() + 1024, "{grown}");
    let c2 = Changes {
        artist: some("Another"),
        ..Changes::default()
    };
    assert_eq!(tight.edit(&c2).written(), 1);
    assert_eq!(
        tight.now().len(),
        grown,
        "the padding takes the second edit"
    );
}

#[test]
fn text_outside_latin1_reads_back_in_every_version() {
    for ver in [2u8, 3, 4] {
        let mut lib = mp3_case(ver, 0);
        let c = Changes {
            title: some("Ça ira \u{20ac} 日本語"),
            ..Changes::default()
        };
        let report = lib.edit(&c);
        assert_written(&mut lib, &report, &c);
        assert_eq!(
            lib.row().tags.title.as_deref(),
            Some("Ça ira \u{20ac} 日本語"),
            "ID3v2.{ver}"
        );
        assert_eq!(
            text_of(&lib.now(), ids(ver).title).as_deref(),
            Some("Ça ira \u{20ac} 日本語")
        );
    }
}

// Prevents: the temporary file losing the file's extension, which made the
// reader take the wrong format for a file with junk between tag and audio.
#[test]
fn mp3_with_junk_between_the_tag_and_the_audio_is_edited() {
    let mut bytes = rich_tag(3, 0);
    bytes.extend(vec![0u8; 3000]);
    bytes.extend(mp3_audio(1));
    let mut lib = Lib::new("a/01.mp3", &bytes);
    let c = Changes {
        title: some("After Junk"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    assert!(lib.now().ends_with(&bytes[bytes.len() - 5000..]));
}

#[test]
fn a_file_name_in_nfd_keeps_its_path_and_key() {
    let nfd = "Cafe\u{301}.mp3";
    let mut bytes = rich_tag(3, 100);
    bytes.extend(mp3_audio(1));
    let mut lib = Lib::new(&format!("a/{nfd}"), &bytes);
    assert!(lib.path.to_string_lossy().contains(nfd), "set up: NFD name");
    let c = Changes {
        album: some("Café"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    assert!(lib.row().path.to_string_lossy().contains(nfd));
}

// ============================== MP4 ================================

/// An m4a with an unknown `----` item added to its `ilst`.
fn with_freeform(bytes: &[u8]) -> Vec<u8> {
    m4a_insert(bytes, &freeform_item(), true)
}

/// `ilst` atoms `full_changes` replaces.
const EDITED_ATOMS: [&[u8; 4]; 6] = [
    b"\xa9nam", b"\xa9ART", b"\xa9gen", b"\xa9day", b"trkn", b"cpil",
];

fn check_m4a(bytes: &[u8]) -> (Lib, Vec<u8>) {
    let mut lib = Lib::new("a/01.m4a", bytes);
    let t = &lib.before.tags;
    assert_eq!(t.title.as_deref(), Some("Old Title"), "{t:?}");
    assert_eq!(t.album.as_deref(), Some("Old Album"));
    assert_eq!(t.year, Some(1984));
    assert_eq!((t.track_no, t.track_total), (Some(2), Some(9)));
    let report = lib.edit(&full_changes());
    assert_written(&mut lib, &report, &full_changes());
    let after = lib.now();
    assert_eq!(
        ilst_but(&after, &EDITED_ATOMS),
        ilst_but(bytes, &EDITED_ATOMS),
        "items not edited keep their bytes and order"
    );
    let names: Vec<[u8; 4]> = ilst_items(&after).into_iter().map(|(n, _)| n).collect();
    for must in [
        b"----", b"\xa9cmt", b"\xa9alb", b"aART", b"\xa9wrt", b"disk",
    ] {
        assert!(names.contains(must), "{must:?} in {names:?}");
    }
    // The tail of the file after moov/the audio is intact.
    let t = tags::read(&lib.path).unwrap().tags;
    assert_eq!(t.album_artist.as_deref(), Some("Old Album Artist"));
    assert_eq!(t.composer.as_deref(), Some("Old Composer"));
    assert_eq!((t.disc_no, t.disc_total), (Some(1), Some(2)));
    (lib, after)
}

#[test]
fn aac_with_moov_after_the_audio_grows_without_moving_offsets() {
    let w = Work::new();
    let bytes = with_freeform(&w.m4a("aac", false, true));
    // Guard: moov is last, no free atom after ilst.
    let l = locate(&bytes);
    assert!(l.moov.1 > top(&bytes, b"mdat").1);
    let (_, after) = check_m4a(&bytes);
    assert!(after.len() > bytes.len(), "no free atom to take the growth");
    assert_eq!(
        stco_entries(&after),
        stco_entries(&bytes),
        "audio did not move"
    );
    assert_eq!(top(&after, b"mdat").1, top(&bytes, b"mdat").1);
}

#[test]
fn alac_with_moov_after_the_audio() {
    let w = Work::new();
    let bytes = with_freeform(&w.m4a("alac", false, true));
    let (lib, after) = check_m4a(&bytes);
    assert_eq!(stco_entries(&after), stco_entries(&bytes));
    assert_eq!(lib.row().info.kind, FileKind::Alac);
}

#[test]
fn aac_with_moov_before_the_audio_shifts_the_chunk_offsets() {
    let w = Work::new();
    let bytes = with_freeform(&w.m4a("aac", true, true));
    let before_mdat = top(&bytes, b"mdat").1;
    assert!(locate(&bytes).moov.1 < before_mdat, "set up: faststart");
    let (_, after) = check_m4a(&bytes);
    let grown = after.len() - bytes.len();
    assert!(grown > 0);
    assert_eq!(top(&after, b"mdat").1, before_mdat + grown);
    let want: Vec<u32> = stco_entries(&bytes)
        .iter()
        .map(|o| o + grown as u32)
        .collect();
    assert_eq!(stco_entries(&after), want);
}

#[test]
fn alac_with_moov_before_the_audio_shifts_the_chunk_offsets() {
    let w = Work::new();
    let bytes = with_freeform(&w.m4a("alac", true, true));
    let (_, after) = check_m4a(&bytes);
    let grown = (after.len() - bytes.len()) as u32;
    assert!(grown > 0);
    let want: Vec<u32> = stco_entries(&bytes).iter().map(|o| o + grown).collect();
    assert_eq!(stco_entries(&after), want);
}

#[test]
fn a_shrinking_edit_with_moov_before_the_audio_moves_offsets_back() {
    let w = Work::new();
    let bytes = with_freeform(&w.m4a("aac", true, false));
    let mut lib = Lib::new("a/01.m4a", &bytes);
    let c = Changes {
        composer: Some(None),
        album_artist: Some(None),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let after = lib.now();
    let shrunk = (bytes.len() - after.len()) as u32;
    assert!(shrunk > 0);
    let want: Vec<u32> = stco_entries(&bytes).iter().map(|o| o - shrunk).collect();
    assert_eq!(stco_entries(&after), want);
}

/// The same m4a with a `free` atom of `total` bytes after `ilst`.
fn with_free(bytes: &[u8], total: usize) -> Vec<u8> {
    m4a_insert(bytes, &free_atom(total), false)
}

fn free_size(d: &[u8]) -> Option<usize> {
    let l = locate(d);
    atoms(d, l.meta.1 + 12, l.meta.2)
        .into_iter()
        .find(|a| &a.0 == b"free")
        .map(|a| a.2 - a.1)
}

#[test]
fn growth_is_absorbed_by_the_free_atom_after_ilst() {
    let w = Work::new();
    for codec in ["aac", "alac"] {
        let base = with_freeform(&w.m4a(codec, true, true));
        let bytes = with_free(&base, 2000);
        let mdat = top(&bytes, b"mdat").1;
        let (_, after) = check_m4a(&bytes);
        assert_eq!(after.len(), bytes.len(), "{codec}: same size");
        assert_eq!(top(&after, b"mdat").1, mdat, "{codec}: audio did not move");
        assert_eq!(stco_entries(&after), stco_entries(&bytes));
        let grew = locate(&after).ilst.2
            - locate(&after).ilst.1
            - (locate(&bytes).ilst.2 - locate(&bytes).ilst.1);
        assert!(grew > 0);
        assert_eq!(free_size(&after), Some(2000 - grew), "{codec}");
    }
}

#[test]
fn a_free_atom_that_would_be_left_under_eight_bytes_is_not_used() {
    let w = Work::new();
    let base = with_freeform(&w.m4a("aac", true, false));
    // Find the growth the edit causes with room to spare.
    let delta = {
        let big = with_free(&base, 4000);
        let lib = Lib::new("a/01.m4a", &big);
        assert_eq!(lib.edit(&full_changes()).written(), 1);
        4000 - free_size(&lib.now()).unwrap()
    };
    // Exactly enough: the free atom is left as a bare header.
    let exact = with_free(&base, delta + 8);
    let (_, after) = check_m4a(&exact);
    assert_eq!(after.len(), exact.len());
    assert_eq!(free_size(&after), Some(8));
    // One byte short of that: the containers grow instead.
    let short = with_free(&base, delta + 7);
    let (_, after) = check_m4a(&short);
    assert_eq!(
        after.len(),
        short.len() + delta,
        "grown by the whole change"
    );
    assert_eq!(free_size(&after), Some(delta + 7), "free atom untouched");
}

#[test]
fn m4a_without_free_or_cover_still_edits() {
    let w = Work::new();
    let bytes = w.m4a("aac", false, false);
    let mut lib = Lib::new("a/01.m4a", &bytes);
    let report = lib.edit(&full_changes());
    assert_written(&mut lib, &report, &full_changes());
}

#[test]
fn a_numeric_genre_atom_is_replaced_and_not_left_as_a_second_value() {
    let w = Work::new();
    let plain = w.m4a("aac", true, false);
    // Replace the ©gen item by a numeric gnre (18 is Rock, one-based).
    let genre_item = ilst_items(&plain)
        .into_iter()
        .find(|(n, _)| n == b"\xa9gen")
        .unwrap()
        .1;
    let pos = plain
        .windows(genre_item.len())
        .position(|w| w == genre_item)
        .unwrap();
    let mut bytes = plain.clone();
    let gnre = mp4_item(b"gnre", 0, &[0, 18]);
    // Same length is not needed: rebuild through the insert helper after
    // blanking the ©gen item into a `free` atom of its own size.
    bytes[pos + 4..pos + 8].copy_from_slice(b"free");
    let bytes = m4a_insert(&bytes, &gnre, true);
    let mut lib = Lib::new("a/01.m4a", &bytes);
    assert_eq!(lib.before.tags.genre.as_deref(), Some("Rock"), "set up");
    let c = Changes {
        genre: some("Jazz"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let names: Vec<[u8; 4]> = ilst_items(&lib.now()).into_iter().map(|(n, _)| n).collect();
    assert!(!names.contains(b"gnre"), "{names:?}");
    assert_eq!(names.iter().filter(|n| *n == b"\xa9gen").count(), 1);
}

#[test]
fn an_m4a_without_an_ilst_atom_is_refused_and_left_alone() {
    let w = Work::new();
    let mut bytes = w.m4a("aac", true, false);
    let at = bytes.windows(4).position(|w| w == b"ilst").unwrap();
    bytes[at..at + 4].copy_from_slice(b"skip");
    let lib = Lib::new("a/01.m4a", &bytes);
    let report = lib.edit(&full_changes());
    assert!(matches!(only(&report), Outcome::Refused(_)), "{report:?}");
    assert_eq!(lib.now(), bytes);
    assert_eq!(lib.stray_files(), Vec::<String>::new());
    assert_eq!(lib.row(), lib.before);
}

// ======================== WAV and AIFF ============================

fn riff_chunk_names(d: &[u8]) -> Vec<String> {
    chunks(d).into_iter().map(|(n, _)| n).collect()
}

fn chunks_but_id3(d: &[u8]) -> Vec<(String, Vec<u8>)> {
    chunks(d)
        .into_iter()
        .filter(|(n, _)| !is_id3_chunk(n))
        .collect()
}

fn id3_chunk(d: &[u8]) -> Vec<u8> {
    let mut found: Vec<Vec<u8>> = chunks(d)
        .into_iter()
        .filter(|(n, _)| is_id3_chunk(n))
        .map(|(_, b)| b)
        .collect();
    assert_eq!(found.len(), 1, "exactly one ID3 chunk");
    found.remove(0)
}

fn wav_base(w: &Work, with_info: bool) -> Vec<u8> {
    if with_info {
        w.make("x.wav", "wav", &[], false)
    } else {
        let out = w.path("none.wav");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=330:duration=1",
            "-map_metadata",
            "-1",
            "-fflags",
            "+bitexact",
            "-f",
            "wav",
            out.to_str().unwrap(),
        ]);
        fs::read(out).unwrap()
    }
}

#[test]
fn wav_with_an_id3_chunk_edits_the_chunk_and_keeps_the_others() {
    let w = Work::new();
    let base = wav_base(&w, true);
    assert!(riff_chunk_names(&base).contains(&"LIST".to_string()));
    let mut t = rich_tag(3, 100);
    // The ID3 chunk holds the title; the LIST/INFO holds the same.
    t.truncate(t.len());
    let bytes = wav_with_chunk(&base, b"id3 ", &t);
    let mut lib = Lib::new("a/01.wav", &bytes);
    assert_eq!(lib.before.tags.title.as_deref(), Some("Old Title"));
    assert_eq!(lib.before.info.kind, FileKind::Wav);
    let report = lib.edit(&full_changes());
    assert_written(&mut lib, &report, &full_changes());
    let after = lib.now();
    assert_eq!(
        chunks_but_id3(&after),
        chunks_but_id3(&bytes),
        "other chunks"
    );
    assert_eq!(riff_chunk_names(&after), riff_chunk_names(&bytes), "order");
    let new_tag = id3_chunk(&after);
    assert_eq!(&new_tag[..4], b"ID3\x03", "version kept");
    let old_tag = id3_chunk(&bytes);
    let edited = edited_frames(3);
    assert_eq!(frames_but(&new_tag, &edited), frames_but(&old_tag, &edited));
    let riff_size = u32::from_le_bytes(after[4..8].try_into().unwrap()) as usize;
    assert_eq!(riff_size + 8, after.len(), "RIFF size matches");
}

#[test]
fn wav_with_only_riff_info_gets_an_id3_chunk_with_every_value() {
    let w = Work::new();
    let bytes = wav_base(&w, true);
    assert!(!riff_chunk_names(&bytes).iter().any(|n| is_id3_chunk(n)));
    let mut lib = Lib::new("a/01.wav", &bytes);
    let before = lib.before.tags.clone();
    assert_eq!(before.title.as_deref(), Some("Old Title"), "from INFO");
    assert_eq!(before.album.as_deref(), Some("Old Album"), "from INFO");
    let c = Changes {
        artist: some("New Artist"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let after = lib.now();
    // The new chunk comes last; the INFO list and the others are the same.
    let names = riff_chunk_names(&after);
    assert!(is_id3_chunk(names.last().unwrap()), "{names:?}");
    assert_eq!(chunks_but_id3(&after), chunks(&bytes));
    // It holds the values the index had, so the reader (which prefers it)
    // gives the same plus the edit.
    let tag = id3_chunk(&after);
    assert_eq!(&tag[..4], b"ID3\x03");
    assert_eq!(text_of(&tag, "TIT2").as_deref(), Some("Old Title"));
    assert_eq!(text_of(&tag, "TALB").as_deref(), Some("Old Album"));
    assert_eq!(text_of(&tag, "TPE1").as_deref(), Some("New Artist"));
}

#[test]
fn wav_with_no_tag_at_all() {
    let w = Work::new();
    let bytes = wav_base(&w, false);
    assert_eq!(riff_chunk_names(&bytes), ["fmt ", "data"]);
    let mut lib = Lib::new("a/01.wav", &bytes);
    assert_eq!(lib.before.tags, Tags::default());
    let c = Changes {
        title: some("Only Title"),
        track_no: Some(Some(4)),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let after = lib.now();
    assert_eq!(chunks_but_id3(&after), chunks(&bytes));
    assert_eq!(frame_ids(&id3_chunk(&after)), ["TIT2", "TRCK"]);
}

#[test]
fn aiff_edits_its_id3_chunk_and_keeps_the_others() {
    let w = Work::new();
    let bytes = w.make("x.aiff", "aiff", &["-write_id3v2", "1"], false);
    let names = riff_chunk_names(&bytes);
    assert!(names.iter().any(|n| is_id3_chunk(n)), "{names:?}");
    let mut lib = Lib::new("a/01.aiff", &bytes);
    assert_eq!(lib.before.info.kind, FileKind::Aiff);
    assert_eq!(lib.before.tags.album.as_deref(), Some("Old Album"));
    let report = lib.edit(&full_changes());
    assert_written(&mut lib, &report, &full_changes());
    let after = lib.now();
    assert_eq!(chunks_but_id3(&after), chunks_but_id3(&bytes));
    assert_eq!(riff_chunk_names(&after), riff_chunk_names(&bytes));
    let (old, new) = (id3_chunk(&bytes), id3_chunk(&after));
    assert_eq!(old[3], new[3], "tag version kept");
    let edited = edited_frames(old[3]);
    assert_eq!(frames_but(&new, &edited), frames_but(&old, &edited));
    let form = u32::from_be_bytes(after[4..8].try_into().unwrap()) as usize;
    assert_eq!(form + 8, after.len());
}

// ============================ refusals =============================

/// Everything that must still be true of a file an edit refused.
struct Snapshot {
    bytes: Vec<u8>,
    mode: u32,
    mtime: (i64, i64),
}

impl Snapshot {
    fn of(p: &Path) -> Snapshot {
        let m = fs::metadata(p).unwrap();
        Snapshot {
            bytes: fs::read(p).unwrap(),
            mode: m.mode(),
            mtime: (m.mtime(), m.mtime_nsec()),
        }
    }

    fn assert_same(&self, p: &Path) {
        let now = Snapshot::of(p);
        assert!(self.bytes == now.bytes, "{} changed", p.display());
        assert_eq!(self.mode, now.mode);
        assert_eq!(self.mtime, now.mtime);
    }
}

fn hidden_files(dir: &Path) -> Vec<String> {
    walk(dir)
        .into_iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .map(|p| p.display().to_string())
        .collect()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn refused(r: &FileResult) -> &str {
    match &r.outcome {
        Outcome::Refused(why) => why,
        o => panic!("expected a refusal, got {o:?} for {}", r.path.display()),
    }
}

#[test]
fn each_refusal_is_reported_leaves_its_file_alone_and_lets_the_others_through() {
    let mut env = Env::new();
    let ok = env.add_bytes("ok.mp3", &{
        let mut b = rich_tag(3, 100);
        b.extend(mp3_audio(1));
        b
    });
    let drm = env.add_bytes("drm.m4a", &fairplay_m4a(1));
    let broken = env.add_bytes(
        "broken.mp3",
        // A TRCK frame in UTF-16 with a BOM that is neither FF FE nor FE FF.
        &mp3_with_id3v2(
            2,
            &[
                id3v24_text("TIT2", 3, b"Lost"),
                id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00]),
            ],
        ),
    );
    let gone = env.add_bytes(
        "gone.mp3",
        &mp3_with_id3v2(3, &[id3v24_text("TIT2", 3, b"G")]),
    );
    let changed = env.add_bytes(
        "changed.mp3",
        &mp3_with_id3v2(4, &[id3v24_text("TIT2", 3, b"C")]),
    );
    env.scan();
    let id = |p: &Path| env.track(p).id;
    let (ok_id, drm_id, broken_id, gone_id, changed_id) =
        (id(&ok), id(&drm), id(&broken), id(&gone), id(&changed));
    assert!(env.track(&drm).drm, "set up: FairPlay");
    assert!(env.track(&broken).tag_error.is_some(), "set up: tag_error");
    fs::remove_file(&gone).unwrap();
    env.scan();
    assert!(
        env.tracks()
            .iter()
            .any(|t| t.id == gone_id && t.missing_since.is_some())
    );
    // Touch a file after the scan.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(&changed)
        .unwrap()
        .set_modified(later)
        .unwrap();

    let snaps: Vec<(PathBuf, Snapshot)> = [&drm, &broken, &changed]
        .iter()
        .map(|p| ((*p).clone(), Snapshot::of(p)))
        .collect();
    let rows_before: Vec<TrackRow> = env.tracks();
    let unknown = 987_654;
    let c = Changes {
        title: some("Edited"),
        ..Changes::default()
    };
    let report = edit_ids(
        &env,
        &[drm_id, unknown, broken_id, gone_id, changed_id, ok_id],
        &c,
    );

    // One result per track, in the order given.
    let got: Vec<i64> = report.results.iter().map(|r| r.track_id).collect();
    assert_eq!(
        got,
        [drm_id, unknown, broken_id, gone_id, changed_id, ok_id]
    );
    assert!(
        refused(&report.results[0])
            .to_lowercase()
            .contains("fairplay")
    );
    assert!(refused(&report.results[1]).contains("index"));
    assert_eq!(report.results[1].path, PathBuf::new());
    assert!(
        refused(&report.results[2])
            .to_lowercase()
            .contains("tags could not be read")
    );
    assert!(
        refused(&report.results[3])
            .to_lowercase()
            .contains("missing")
    );
    assert!(refused(&report.results[4]).to_lowercase().contains("scan"));
    assert_eq!(report.results[5].outcome, Outcome::Written, "{report:?}");
    assert_eq!(report.written(), 1);
    assert_eq!(report.refused().count(), 5);

    for (p, s) in &snaps {
        s.assert_same(p);
    }
    assert_eq!(hidden_files(&env.music), Vec::<String>::new());
    // Only the written track's row changed.
    for before in rows_before.iter().filter(|t| t.id != ok_id) {
        let now = store::track(env.store.conn(), before.id).unwrap().unwrap();
        assert_eq!(&now, before, "{}", before.path.display());
    }
    assert_eq!(env.track(&ok).tags.title.as_deref(), Some("Edited"));

    // After a scan the touched file can be edited.
    env.scan();
    let again = edit_ids(&env, &[changed_id], &c);
    assert_eq!(only(&again), &Outcome::Written, "{again:?}");
}

#[test]
fn a_file_rewritten_after_the_scan_is_refused_even_at_the_same_size() {
    let mut bytes = rich_tag(3, 100);
    bytes.extend(mp3_audio(1));
    let lib = Lib::new("a/01.mp3", &bytes);
    let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&lib.path)
        .unwrap()
        .set_modified(earlier)
        .unwrap();
    let snap = Snapshot::of(&lib.path);
    let report = lib.edit(&Changes {
        title: some("X"),
        ..Changes::default()
    });
    assert!(
        refused(&report.results[0])
            .to_lowercase()
            .contains("changed")
    );
    snap.assert_same(&lib.path);
    assert_eq!(lib.row(), lib.before);
}

#[test]
fn out_of_range_values_are_refused_for_every_track_and_change_nothing() {
    let mut env = Env::new();
    let mut ids_ = Vec::new();
    let mut files = Vec::new();
    for (rel, v) in [("a.mp3", 1), ("b.mp3", 2)] {
        let mut b = rich_tag(3, 100);
        b.extend(mp3_audio(v));
        files.push(env.add_bytes(rel, &b));
    }
    env.scan();
    for f in &files {
        ids_.push(env.track(f).id);
    }
    let snaps: Vec<Snapshot> = files.iter().map(|f| Snapshot::of(f)).collect();
    let rows = env.tracks();
    for bad in [
        Changes {
            year: Some(Some(0)),
            ..Changes::default()
        },
        Changes {
            year: Some(Some(10_000)),
            ..Changes::default()
        },
        Changes {
            track_no: Some(Some(1000)),
            ..Changes::default()
        },
        Changes {
            track_no: Some(Some(0)),
            ..Changes::default()
        },
        Changes {
            track_total: Some(Some(1000)),
            ..Changes::default()
        },
        Changes {
            disc_no: Some(Some(0)),
            ..Changes::default()
        },
        Changes {
            disc_total: Some(Some(1000)),
            ..Changes::default()
        },
        // One bad value spoils the whole edit, a good field beside it too.
        Changes {
            title: some("Fine"),
            year: Some(Some(0)),
            ..Changes::default()
        },
    ] {
        let report = edit_ids(&env, &ids_, &bad);
        assert_eq!(report.results.len(), 2, "{bad:?}");
        for r in &report.results {
            assert!(refused(r).contains("is not between"), "{bad:?}: {r:?}");
        }
        for (f, s) in files.iter().zip(&snaps) {
            s.assert_same(f);
        }
        assert_eq!(env.tracks(), rows, "{bad:?}");
    }
    assert_eq!(hidden_files(&env.music), Vec::<String>::new());
}

#[test]
fn the_largest_accepted_values_are_written_and_read_back() {
    let mut lib = mp3_case(4, 200);
    let c = Changes {
        year: Some(Some(9999)),
        track_no: Some(Some(999)),
        track_total: Some(Some(999)),
        disc_no: Some(Some(1)),
        disc_total: Some(Some(999)),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    assert_eq!(lib.row().tags.year, Some(9999));
    // And the smallest numbers.
    let c = Changes {
        year: Some(Some(1000)),
        track_no: Some(Some(1)),
        track_total: Some(Some(1)),
        disc_total: Some(Some(1)),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_eq!(only(&report), &Outcome::Written, "{report:?}");
    let t = tags::read(&lib.path).unwrap().tags;
    assert_eq!(
        (t.year, t.track_no, t.track_total, t.disc_total),
        (Some(1000), Some(1), Some(1), Some(1))
    );
    assert_eq!(lib.row().tags, t);
}

/// Gives the permissions back when dropped, so the temp dir can be removed.
struct Restore(PathBuf, u32);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}

#[test]
fn a_folder_that_cannot_be_written_refuses_and_leaves_the_file() {
    let mut bytes = rich_tag(3, 100);
    bytes.extend(mp3_audio(1));
    let lib = Lib::new("a/01.mp3", &bytes);
    let dir = lib.path.parent().unwrap().to_owned();
    let _restore = Restore(dir.clone(), 0o755);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::File::create(dir.join(".probe")).is_ok() {
        eprintln!("running as a user the folder mode does not stop; test not meaningful");
        let _ = fs::remove_file(dir.join(".probe"));
        return;
    }
    let snap = Snapshot::of(&lib.path);
    let report = lib.edit(&Changes {
        title: some("X"),
        ..Changes::default()
    });
    assert!(matches!(only(&report), Outcome::Refused(_)), "{report:?}");
    snap.assert_same(&lib.path);
    assert_eq!(lib.row(), lib.before);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(lib.stray_files(), Vec::<String>::new());
}

// Spec 7.1: a track is refused when "the file or its folder is not
// writable". The file is replaced by rename, which only needs the folder, so
// the file's own mode is checked first.
#[test]
fn a_read_only_file_is_refused() {
    let mut bytes = rich_tag(3, 100);
    bytes.extend(mp3_audio(1));
    let lib = Lib::new("a/01.mp3", &bytes);
    fs::set_permissions(&lib.path, fs::Permissions::from_mode(0o444)).unwrap();
    let snap = Snapshot::of(&lib.path);
    let report = lib.edit(&Changes {
        title: some("X"),
        ..Changes::default()
    });
    assert!(
        refused(&report.results[0]).contains("read-only"),
        "{report:?}"
    );
    snap.assert_same(&lib.path);
    assert_eq!(lib.row(), lib.before);
    assert_eq!(lib.stray_files(), Vec::<String>::new());
}

#[test]
fn an_id3_tag_with_header_flags_is_refused_and_left_alone() {
    let mut t = rich_tag(3, 100);
    t[5] = 0x80; // unsynchronisation flag; the tag has no byte it would change
    let mut bytes = t;
    bytes.extend(mp3_audio(1));
    let lib = Lib::new("a/01.mp3", &bytes);
    let report = lib.edit(&Changes {
        title: some("X"),
        ..Changes::default()
    });
    assert!(matches!(only(&report), Outcome::Refused(_)), "{report:?}");
    assert_eq!(lib.now(), bytes);
    assert_eq!(lib.stray_files(), Vec::<String>::new());
    assert_eq!(lib.row(), lib.before);
}

// Prevents: writing into the first of two stacked ID3v2 tags when the reader
// takes some fields from the second: the read-back check must refuse.
#[test]
fn two_stacked_id3_tags_are_refused_by_the_read_back_check() {
    let first = tag(3, &[text(3, "TIT2", "First Title")], 50);
    let second = tag(
        3,
        &[
            text(3, "TIT2", "Second Title"),
            text(3, "TPE1", "Second Artist"),
        ],
        50,
    );
    let mut bytes = first;
    bytes.extend(second);
    bytes.extend(mp3_audio(1));
    let lib = Lib::new("a/01.mp3", &bytes);
    let report = lib.edit(&Changes {
        title: some("Edited Title"),
        ..Changes::default()
    });
    assert!(
        refused(&report.results[0]).contains("did not check out"),
        "{report:?}"
    );
    assert_eq!(lib.now(), bytes);
    assert_eq!(lib.stray_files(), Vec::<String>::new());
    assert_eq!(lib.row(), lib.before);
}

// ======================= fields and the call =======================

#[test]
fn clearing_a_field_removes_its_frames_and_only_those() {
    for ver in [2u8, 3, 4] {
        let mut lib = mp3_case(ver, 200);
        let c = Changes {
            artist: Some(None),
            composer: some("   "), // blank: a clear
            genre: Some(None),
            year: Some(None),
            track_no: Some(None),
            track_total: Some(None),
            ..Changes::default()
        };
        let report = lib.edit(&c);
        assert_written(&mut lib, &report, &c);
        let t = lib.row().tags;
        assert_eq!(
            (
                t.artist,
                t.composer,
                t.genre,
                t.year,
                t.track_no,
                t.track_total
            ),
            (None, None, None, None, None, None),
            "ID3v2.{ver}"
        );
        let after = lib.now();
        let i = ids(ver);
        let kept = frame_ids(&after);
        for gone in [i.artist, i.composer, i.genre, i.track] {
            assert!(
                !kept.iter().any(|k| k == gone),
                "ID3v2.{ver}: {gone} in {kept:?}"
            );
        }
        for date in ["TYE", "TDA", "TIM", "TYER", "TDAT", "TIME", "TDRC"] {
            assert!(!kept.iter().any(|k| k == date), "ID3v2.{ver}: {date}");
        }
        // The rest stays.
        assert_eq!(t_title(&lib), Some("Old Title".to_string()));
        assert_eq!(lib.row().tags.album.as_deref(), Some("Old Album"));
        assert_eq!(
            (lib.row().tags.disc_no, lib.row().tags.disc_total),
            (Some(1), Some(2))
        );
    }
}

fn t_title(lib: &Lib) -> Option<String> {
    lib.row().tags.title
}

#[test]
fn clearing_fields_of_an_m4a_removes_its_atoms() {
    let w = Work::new();
    let bytes = with_freeform(&w.m4a("aac", true, true));
    let mut lib = Lib::new("a/01.m4a", &bytes);
    let c = Changes {
        artist: Some(None),
        genre: Some(None),
        year: Some(None),
        track_no: Some(None),
        track_total: Some(None),
        disc_no: Some(None),
        disc_total: Some(None),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let names: Vec<[u8; 4]> = ilst_items(&lib.now()).into_iter().map(|(n, _)| n).collect();
    for gone in [b"\xa9ART", b"\xa9gen", b"\xa9day", b"trkn", b"disk"] {
        assert!(!names.contains(gone), "{gone:?} in {names:?}");
    }
    for kept in [b"\xa9nam", b"\xa9alb", b"covr", b"----", b"\xa9cmt"] {
        assert!(names.contains(kept), "{kept:?} in {names:?}");
    }
}

/// A total with no number is "0/12" and reads back as a total only.
#[test]
fn a_track_total_without_a_number_round_trips() {
    // MP3 with no track at all.
    let mut bytes = tag(3, &[text(3, "TIT2", "T")], 100);
    bytes.extend(mp3_audio(1));
    let mut lib = Lib::new("a/01.mp3", &bytes);
    assert_eq!(lib.before.tags.track_no, None);
    let c = Changes {
        track_total: Some(Some(12)),
        disc_total: Some(Some(3)),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let t = lib.row().tags;
    assert_eq!((t.track_no, t.track_total), (None, Some(12)));
    assert_eq!((t.disc_no, t.disc_total), (None, Some(3)));
    assert_eq!(text_of(&lib.now(), "TRCK").as_deref(), Some("0/12"));

    // M4A.
    let w = Work::new();
    let m4a = w.make(
        "x.m4a",
        "ipod",
        &["-c:a", "aac", "-movflags", "+faststart"],
        false,
    );
    // Without a track number: drop trkn with a clear first.
    let mut lib = Lib::new("a/01.m4a", &m4a);
    let clear = Changes {
        track_no: Some(None),
        track_total: Some(None),
        ..Changes::default()
    };
    assert_eq!(lib.edit(&clear).written(), 1);
    lib.env.scan();
    let c = Changes {
        track_total: Some(Some(12)),
        ..Changes::default()
    };
    lib.before = lib.row();
    let report = lib.edit(&c);
    assert_eq!(only(&report), &Outcome::Written, "{report:?}");
    let t = tags::read(&lib.path).unwrap().tags;
    assert_eq!((t.track_no, t.track_total), (None, Some(12)));
    assert_eq!(lib.row().tags, t);
    // Setting the number later keeps the total.
    let c = Changes {
        track_no: Some(Some(5)),
        ..Changes::default()
    };
    assert_eq!(lib.edit(&c).written(), 1);
    let t = tags::read(&lib.path).unwrap().tags;
    assert_eq!((t.track_no, t.track_total), (Some(5), Some(12)));
}

#[test]
fn compilation_is_set_and_cleared_in_mp3_and_m4a() {
    let w = Work::new();
    let m4a = w.m4a("aac", true, false);
    let mut mp3 = rich_tag(3, 200);
    mp3.extend(mp3_audio(1));
    for (rel, bytes, name) in [("a/01.mp3", mp3, "TCMP"), ("a/01.m4a", m4a, "cpil")] {
        let mut lib = Lib::new(rel, &bytes);
        assert!(!lib.before.tags.compilation);
        let on = Changes {
            compilation: Some(true),
            ..Changes::default()
        };
        let report = lib.edit(&on);
        assert_written(&mut lib, &report, &on);
        assert!(lib.row().tags.compilation, "{rel}");
        assert!(tags::read(&lib.path).unwrap().tags.compilation);
        assert!(
            lib.now().windows(4).any(|w| w == name.as_bytes()),
            "{rel}: {name} written"
        );
        lib.before = lib.row();
        lib.bytes = lib.now();
        let off = Changes {
            compilation: Some(false),
            ..Changes::default()
        };
        let report = lib.edit(&off);
        assert_written(&mut lib, &report, &off);
        assert!(!tags::read(&lib.path).unwrap().tags.compilation, "{rel}");
        assert!(
            !lib.now().windows(4).any(|w| w == name.as_bytes()),
            "{rel}: {name} removed"
        );
    }
}

#[test]
fn a_track_listed_twice_is_edited_and_reported_once() {
    let lib = mp3_case(3, 200);
    let seen = std::cell::RefCell::new(Vec::new());
    let report = tag_edit::edit(
        lib.env.store.conn(),
        &[lib.id, lib.id, lib.id],
        &Changes {
            title: some("Once"),
            ..Changes::default()
        },
        &mut |p| seen.borrow_mut().push((p.done, p.total, p.path.to_owned())),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        report.results,
        [FileResult {
            track_id: lib.id,
            path: lib.path.clone(),
            outcome: Outcome::Written,
        }]
    );
    assert_eq!(*seen.borrow(), [(0, 1, lib.path.clone())]);
    assert_eq!(lib.row().tags.title.as_deref(), Some("Once"));
}

#[test]
fn progress_names_each_file_before_it_is_written() {
    let mut env = Env::new();
    let mut files = Vec::new();
    for rel in ["a.mp3", "b.mp3", "c.mp3"] {
        let mut b = rich_tag(3, 100);
        b.extend(mp3_audio(1));
        files.push(env.add_bytes(rel, &b));
    }
    env.scan();
    let ids_: Vec<i64> = files.iter().map(|f| env.track(f).id).collect();
    let seen = std::cell::RefCell::new(Vec::new());
    let report = tag_edit::edit(
        env.store.conn(),
        &ids_,
        &Changes {
            genre: some("Pop"),
            ..Changes::default()
        },
        &mut |p| {
            // At this point the file is not yet written.
            let on_disk = tags::read(p.path).unwrap().tags.genre;
            seen.borrow_mut()
                .push((p.done, p.total, p.path.to_owned(), on_disk));
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.written(), 3);
    let want: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i, 3, f.clone(), Some("Rock".to_string())))
        .collect();
    assert_eq!(*seen.borrow(), want);
}

#[test]
fn stop_between_files_leaves_the_rest_untouched() {
    let mut env = Env::new();
    let mut files = Vec::new();
    for rel in ["a.mp3", "b.mp3", "c.mp3"] {
        let mut b = rich_tag(3, 100);
        b.extend(mp3_audio(1));
        files.push(env.add_bytes(rel, &b));
    }
    env.scan();
    let ids_: Vec<i64> = files.iter().map(|f| env.track(f).id).collect();
    let snaps: Vec<Snapshot> = files.iter().map(|f| Snapshot::of(f)).collect();
    let rows = env.tracks();
    let stop = AtomicBool::new(false);
    let report = tag_edit::edit(
        env.store.conn(),
        &ids_,
        &Changes {
            genre: some("Pop"),
            ..Changes::default()
        },
        // Stop is raised while the first file is announced: that file is
        // finished, the next is not started.
        &mut |p| {
            if p.done == 0 {
                stop.store(true, Ordering::Relaxed);
            }
        },
        &stop,
    )
    .unwrap();
    assert!(report.stopped);
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].track_id, ids_[0]);
    assert_eq!(report.results[0].outcome, Outcome::Written);
    assert_eq!(env.track(&files[0]).tags.genre.as_deref(), Some("Pop"));
    assert_ne!(fs::read(&files[0]).unwrap(), snaps[0].bytes);
    for i in [1, 2] {
        snaps[i].assert_same(&files[i]);
        assert_eq!(
            env.track(&files[i]),
            rows.iter().find(|r| r.id == ids_[i]).unwrap().clone()
        );
    }
}

#[test]
fn a_stop_raised_before_the_start_writes_nothing() {
    let lib = mp3_case(3, 100);
    let snap = Snapshot::of(&lib.path);
    let report = tag_edit::edit(
        lib.env.store.conn(),
        &[lib.id],
        &Changes {
            genre: some("Pop"),
            ..Changes::default()
        },
        &mut |_| {},
        &AtomicBool::new(true),
    )
    .unwrap();
    assert!(report.stopped && report.results.is_empty());
    snap.assert_same(&lib.path);
}

#[test]
fn values_the_file_already_holds_write_nothing() {
    for (rel, bytes) in {
        let w = Work::new();
        let mut mp3 = rich_tag(3, 100);
        mp3.extend(mp3_audio(1));
        [
            ("a/01.mp3", mp3),
            ("a/01.m4a", with_freeform(&w.m4a("aac", true, true))),
        ]
    } {
        let lib = Lib::new(rel, &bytes);
        // Hours old, so a rewrite would move the modification time.
        let snap = Snapshot::of(&lib.path);
        let same = Changes {
            artist: some("  Old Artist "), // trimmed, equal
            year: Some(Some(1984)),
            track_total: Some(Some(9)),
            compilation: Some(false),
            genre: some("Rock"),
            ..Changes::default()
        };
        let report = lib.edit(&same);
        assert_eq!(only(&report), &Outcome::Unchanged, "{rel}: {report:?}");
        assert_eq!(report.written(), 0);
        snap.assert_same(&lib.path);
        assert_eq!(lib.row(), lib.before);
        assert_eq!(lib.stray_files(), Vec::<String>::new());
    }
}

#[test]
fn clearing_a_field_that_is_already_empty_is_unchanged() {
    let lib = mp3_case(3, 100);
    // Clear the composer once, then again.
    let c = Changes {
        composer: Some(None),
        ..Changes::default()
    };
    assert_eq!(lib.edit(&c).written(), 1);
    let after_first = lib.now();
    let report = lib.edit(&c);
    assert_eq!(only(&report), &Outcome::Unchanged, "{report:?}");
    assert_eq!(lib.now(), after_first);
}

#[test]
fn the_same_edit_twice_writes_once() {
    let lib = mp3_case(4, 100);
    let first = lib.edit(&full_changes());
    assert_eq!(only(&first), &Outcome::Written);
    let bytes = lib.now();
    let row = lib.row();
    let second = lib.edit(&full_changes());
    assert_eq!(only(&second), &Outcome::Unchanged, "{second:?}");
    assert_eq!(lib.now(), bytes);
    assert_eq!(lib.row(), row);
}

#[test]
fn an_edit_that_changes_nothing_is_unchanged_even_for_a_refusable_file() {
    // The no-op is decided before the file is looked at, but refusals come
    // first: a FairPlay file is refused whatever the edit says.
    let mut env = Env::new();
    let p = env.add_bytes("a.m4a", &fairplay_m4a(1));
    env.scan();
    let id = env.track(&p).id;
    let report = edit_ids(&env, &[id], &Changes::default());
    assert!(
        refused(&report.results[0])
            .to_lowercase()
            .contains("fairplay")
    );
}

#[test]
fn an_edit_of_one_track_leaves_the_others_and_the_playlists_alone() {
    let mut env = Env::new();
    let mut files = Vec::new();
    for (i, rel) in ["a.mp3", "b.mp3"].iter().enumerate() {
        let mut b = rich_tag(3, 100);
        b.extend(mp3_audio(i as u32 + 1));
        files.push(env.add_bytes(rel, &b));
    }
    env.write_playlist(
        "Road",
        &format!("{}\n{}\n", files[0].display(), files[1].display()),
    );
    env.scan();
    iprune_core::playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
    let id_a = env.track(&files[0]).id;
    let items_before = env.indexed_items("Road");
    let playlist_file = env.read_playlist("Road");
    let b_before = (fs::read(&files[1]).unwrap(), env.track(&files[1]));
    assert_eq!(edit_ids(&env, &[id_a], &full_changes()).written(), 1);
    assert_eq!(env.indexed_items("Road"), items_before);
    assert_eq!(env.read_playlist("Road"), playlist_file);
    assert_eq!(fs::read(&files[1]).unwrap(), b_before.0);
    assert_eq!(env.track(&files[1]), b_before.1);
    let _ = Kind::Mp3;
}

// Spec 7.1: a year is 1000 to 9999. The reader gives no year for fewer than
// four digits, so a smaller one is refused before anything is read, with the
// range, rather than by the read-back check.
#[test]
fn a_year_below_1000_is_refused_up_front() {
    let w = Work::new();
    let mut mp3 = rich_tag(3, 200);
    mp3.extend(mp3_audio(1));
    for (rel, bytes) in [("a/01.mp3", mp3), ("a/01.m4a", w.m4a("aac", true, false))] {
        for year in [1u32, 999] {
            let lib = Lib::new(rel, &bytes);
            let report = lib.edit(&Changes {
                year: Some(Some(year)),
                ..Changes::default()
            });
            let why = refused(&report.results[0]);
            assert!(
                why.contains("between 1000 and 9999"),
                "{rel} year {year}: {why}"
            );
            assert_eq!(lib.now(), bytes);
        }
    }
}

// ---- a field set to the value it has is not written

#[test]
fn a_year_equal_to_the_current_one_keeps_the_full_date() {
    // ID3v2.4: TDRC holds a full date.
    let mut v24 = tag(
        4,
        &[text(4, "TIT2", "Old"), text(4, "TDRC", "1984-05-02")],
        200,
    );
    v24.extend(mp3_audio(1));
    // ID3v2.3: TYER, TDAT and TIME.
    let mut v23 = tag(
        3,
        &[
            text(3, "TIT2", "Old"),
            text(3, "TYER", "1984"),
            text(3, "TDAT", "0205"),
            text(3, "TIME", "1230"),
        ],
        200,
    );
    v23.extend(mp3_audio(1));
    for (ver, bytes, date_ids) in [
        (4u8, v24, &["TDRC"][..]),
        (3, v23, &["TYER", "TDAT", "TIME"][..]),
    ] {
        let mut lib = Lib::new("a/01.mp3", &bytes);
        assert_eq!(lib.before.tags.year, Some(1984), "set up v2.{ver}");
        let c = Changes {
            title: some("New"),
            year: Some(Some(1984)),
            ..Changes::default()
        };
        let report = lib.edit(&c);
        assert_written(&mut lib, &report, &c);
        let after = lib.now();
        assert_eq!(text_of(&after, "TIT2").as_deref(), Some("New"));
        assert_eq!(
            frames_but(&after, &["TIT2"]),
            frames_but(&bytes, &["TIT2"]),
            "v2.{ver}: the date frames keep their bytes"
        );
        for id in date_ids {
            assert!(frame_ids(&after).iter().any(|f| f == id), "{id}");
        }
        // A different year does replace the whole date.
        let c = Changes {
            year: Some(Some(1990)),
            ..Changes::default()
        };
        assert_eq!(lib.edit(&c).written(), 1);
        let ids = frame_ids(&lib.now());
        assert_eq!(
            ids.iter()
                .filter(|i| ["TDRC", "TYER", "TDAT", "TIME"].contains(&i.as_str()))
                .count(),
            1,
            "{ids:?}"
        );
    }
}

#[test]
fn a_year_equal_to_the_current_one_keeps_the_full_date_in_an_m4a() {
    let w = Work::new();
    let bytes = w.make(
        "x.m4a",
        "ipod",
        &[
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
            "-metadata",
            "date=1984-05-02",
        ],
        false,
    );
    let mut lib = Lib::new("a/01.m4a", &bytes);
    assert_eq!(lib.before.tags.year, Some(1984), "set up");
    let c = Changes {
        album: some("New Album"),
        year: Some(Some(1984)),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let day = |d: &[u8]| {
        ilst_items(d)
            .into_iter()
            .find(|(n, _)| n == b"\xa9day")
            .map(|(_, b)| b)
    };
    assert!(day(&bytes).is_some());
    assert_eq!(day(&lib.now()), day(&bytes));
}

#[test]
fn only_the_fields_that_differ_are_rewritten() {
    // The artist frame is UTF-16, which the editor would write as Latin-1:
    // it keeps its bytes when the artist is set to what it already is.
    let mut utf16 = vec![1, 0xff, 0xfe];
    utf16.extend("Old Artist".encode_utf16().flat_map(u16::to_le_bytes));
    let mut bytes = tag(
        3,
        &[
            text(3, "TIT2", "Old Title"),
            frame(3, "TPE1", &utf16),
            text(3, "TALB", "Old Album"),
        ],
        200,
    );
    bytes.extend(mp3_audio(1));
    let mut lib = Lib::new("a/01.mp3", &bytes);
    assert_eq!(lib.before.tags.artist.as_deref(), Some("Old Artist"));
    let c = Changes {
        title: some("New Title"),
        artist: some("Old Artist"),
        album: some("New Album"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    assert_eq!(
        frames_but(&lib.now(), &["TIT2", "TALB"]),
        frames_but(&bytes, &["TIT2", "TALB"])
    );
    // Every field equal: nothing written, mtime untouched.
    let snap = Snapshot::of(&lib.path);
    let row = lib.row();
    let same = Changes {
        title: some("New Title"),
        artist: some("Old Artist"),
        album: some("New Album"),
        ..Changes::default()
    };
    let report = lib.edit(&same);
    assert_eq!(only(&report), &Outcome::Unchanged, "{report:?}");
    snap.assert_same(&lib.path);
    assert_eq!(lib.row(), row);
}

// ---- the index fails after a file was written

fn three_files(env: &mut Env) -> (Vec<PathBuf>, Vec<i64>) {
    let mut files = Vec::new();
    for rel in ["a.mp3", "b.mp3", "c.mp3"] {
        let mut b = rich_tag(3, 100);
        b.extend(mp3_audio(1));
        files.push(env.add_bytes(rel, &b));
    }
    env.scan();
    let ids = files.iter().map(|f| env.track(f).id).collect();
    (files, ids)
}

#[test]
fn a_failing_index_update_is_reported_stops_the_edit_and_leaves_the_file_written() {
    let mut env = Env::new();
    let (files, ids_) = three_files(&mut env);
    let rows = env.tracks();
    let untouched = Snapshot::of(&files[2]);
    // Only this connection has the trigger; it refuses the second track.
    env.store
        .conn()
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER boom BEFORE UPDATE ON tracks WHEN OLD.id = {} \
             BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            ids_[1]
        ))
        .unwrap();
    let c = Changes {
        genre: some("Pop"),
        ..Changes::default()
    };
    let report = edit_ids(&env, &ids_, &c);
    assert_eq!(report.results.len(), 2, "the third is not started");
    assert_eq!(report.results[0].outcome, Outcome::Written);
    assert_eq!(report.results[1].outcome, Outcome::Written);
    assert_eq!(report.results[1].track_id, ids_[1]);
    assert!(report.stopped);
    let why = report.index_error.as_deref().expect("index_error");
    assert!(why.contains("boom"), "{why}");
    // Both files carry the new genre; the first row too, the second row
    // still has the old tags; the third file was not touched.
    assert_eq!(
        tags::read(&files[0]).unwrap().tags.genre.as_deref(),
        Some("Pop")
    );
    assert_eq!(
        tags::read(&files[1]).unwrap().tags.genre.as_deref(),
        Some("Pop")
    );
    assert_eq!(env.track(&files[0]).tags.genre.as_deref(), Some("Pop"));
    let old = rows.iter().find(|r| r.id == ids_[1]).unwrap();
    assert_eq!(&env.track(&files[1]), old, "the row was not updated");
    untouched.assert_same(&files[2]);
    assert_eq!(hidden_files(&env.music), Vec::<String>::new());

    // A scan brings the index up to date, and the edit can go on.
    env.store
        .conn()
        .execute_batch("DROP TRIGGER boom;")
        .unwrap();
    let r = env.scan();
    assert_eq!(r.updated, 1, "{r:?}");
    assert_eq!(env.track(&files[1]).tags.genre.as_deref(), Some("Pop"));
    let again = edit_ids(&env, &ids_, &c);
    assert_eq!(again.index_error, None);
    assert!(!again.stopped);
    assert_eq!(again.written(), 1, "only the third was left: {again:?}");
}

#[test]
fn a_successful_edit_has_no_index_error() {
    let lib = mp3_case(3, 100);
    let report = lib.edit(&full_changes());
    assert_eq!(report.index_error, None);
    assert!(!report.stopped);
}

// ---- WAV whose last chunk is odd and has no pad byte

#[test]
fn a_new_id3_chunk_after_an_unpadded_odd_chunk_starts_on_an_even_offset() {
    let w = Work::new();
    let mut bytes = wav_base(&w, false);
    bytes.extend_from_slice(b"odd ");
    bytes.extend(3u32.to_le_bytes());
    bytes.extend_from_slice(b"abc"); // no pad byte
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    assert_eq!(bytes.len() % 2, 1, "set up: unpadded");
    let mut lib = Lib::new("a/01.wav", &bytes);
    let c = Changes {
        title: some("After Odd"),
        ..Changes::default()
    };
    let report = lib.edit(&c);
    assert_written(&mut lib, &report, &c);
    let after = lib.now();
    assert_eq!(riff_chunk_names(&after), ["fmt ", "data", "odd ", "id3 "]);
    let odd = chunks(&after)
        .into_iter()
        .find(|(n, _)| n == "odd ")
        .unwrap()
        .1;
    assert_eq!(odd, b"abc");
    // The pad byte sits right after "abc", so the id3 chunk starts even.
    let at = after.windows(4).rposition(|w| w == b"id3 ").unwrap();
    assert_eq!(at % 2, 0, "chunk starts at {at}");
    assert_eq!(after.len() % 2, 0);
    assert_eq!(lib.row().tags.title.as_deref(), Some("After Odd"));
}

// ---- fragmented MP4

#[test]
fn a_fragmented_mp4_is_refused_and_left_alone() {
    let w = Work::new();
    let base = w.m4a("aac", true, false);
    for name in [b"moof", b"sidx", b"mfra"] {
        let mut bytes = base.clone();
        bytes.extend(mp4_atom(name, &[0; 8]));
        let lib = Lib::new("a/01.m4a", &bytes);
        let report = lib.edit(&full_changes());
        assert!(
            refused(&report.results[0])
                .to_lowercase()
                .contains("fragmented"),
            "{}: {report:?}",
            String::from_utf8_lossy(name)
        );
        assert_eq!(lib.now(), bytes);
        assert_eq!(lib.stray_files(), Vec::<String>::new());
        assert_eq!(lib.row(), lib.before);
    }
}
