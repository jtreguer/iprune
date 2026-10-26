//! ID3v2 tags, versions 2.2, 2.3 and 2.4, edited frame by frame. The tag
//! keeps its version and every frame not edited keeps its bytes.

use super::Item;

/// Padding given to a tag that has to grow, so the next edits fit in it.
const PADDING: usize = 1024;

/// One frame as found: its id and its bytes, header included.
struct RawFrame<'a> {
    id: String,
    bytes: std::borrow::Cow<'a, [u8]>,
}

struct Parsed<'a> {
    version: u8,
    /// An ID3v2.4 tag whose frame sizes are plain integers, not syncsafe
    /// ones, as some old taggers wrote them. New frames follow it.
    plain_sizes: bool,
    frames: Vec<RawFrame<'a>>,
    /// The size of the tag after its header, padding included.
    body_len: usize,
}

fn be(b: &[u8]) -> usize {
    b.iter().fold(0, |n, &x| (n << 8) | x as usize)
}

fn syncsafe(b: &[u8]) -> Option<usize> {
    b.iter()
        .try_fold(0, |n, &x| (x < 0x80).then_some((n << 7) | x as usize))
}

fn put_syncsafe(n: usize) -> [u8; 4] {
    [
        (n >> 21 & 0x7f) as u8,
        (n >> 14 & 0x7f) as u8,
        (n >> 7 & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

/// The tag's length, header included, if `data` starts with one.
fn tag_len(data: &[u8]) -> Option<std::result::Result<usize, String>> {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return None;
    }
    Some(
        syncsafe(&data[6..10])
            .map(|n| n + 10)
            .ok_or_else(|| "the ID3v2 tag size is not valid".to_string()),
    )
}

/// The frames of a tag body, or `None` when they do not chain up.
fn frames(body: &[u8], version: u8, plain: bool) -> Option<Vec<RawFrame<'_>>> {
    let (id_len, header) = if version == 2 { (3, 6) } else { (4, 10) };
    let mut out = Vec::new();
    let mut pos = 0;
    // What follows the first zero byte where a frame would start is
    // padding.
    while pos + header <= body.len() && body[pos] != 0 {
        let id = &body[pos..pos + id_len];
        if !id
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return None;
        }
        let size_bytes = &body[pos + id_len..pos + id_len + if version == 2 { 3 } else { 4 }];
        let size = match version {
            4 if !plain => syncsafe(size_bytes)?,
            _ => be(size_bytes),
        };
        let end = pos + header + size;
        if end > body.len() {
            return None;
        }
        out.push(RawFrame {
            id: String::from_utf8_lossy(id).into_owned(),
            bytes: body[pos..end].into(),
        });
        pos = end;
    }
    Some(out)
}

fn parse(data: &[u8]) -> std::result::Result<Parsed<'_>, String> {
    let len = tag_len(data).ok_or("no ID3v2 tag")??;
    let (version, flags) = (data[3], data[5]);
    if !(2..=4).contains(&version) {
        return Err(format!("ID3v2.{version} tags are not supported"));
    }
    if flags != 0 {
        return Err(format!(
            "ID3v2 tags with header flags ({flags:#04x}: unsynchronisation, compression, \
             extended header or footer) are not supported"
        ));
    }
    if len > data.len() {
        return Err("the ID3v2 tag is longer than the file".into());
    }
    let body = &data[10..len];
    let (frames, plain_sizes) = match frames(body, version, false) {
        Some(f) => (f, false),
        None if version == 4 => (
            frames(body, version, true).ok_or("the ID3v2 frames cannot be walked")?,
            true,
        ),
        None => return Err("the ID3v2 frames cannot be walked".into()),
    };
    Ok(Parsed {
        version,
        plain_sizes,
        frames,
        body_len: body.len(),
    })
}

/// A text frame in the tag's version: UTF-8 in 2.4; Latin-1 when it can
/// hold the text, otherwise UTF-16, in 2.2 and 2.3.
fn text_frame(version: u8, plain: bool, id: &str, text: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(text.len() + 3);
    if version == 4 {
        body.push(3);
        body.extend_from_slice(text.as_bytes());
    } else if text.chars().all(|c| (c as u32) <= 0xff) {
        body.push(0);
        body.extend(text.chars().map(|c| c as u8));
    } else {
        body.extend([1, 0xff, 0xfe]);
        body.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    }
    let mut f = Vec::with_capacity(body.len() + 10);
    f.extend_from_slice(id.as_bytes());
    let n = body.len();
    match version {
        2 => f.extend_from_slice(&(n as u32).to_be_bytes()[1..]),
        4 if !plain => f.extend(put_syncsafe(n)),
        _ => f.extend((n as u32).to_be_bytes()),
    }
    if version != 2 {
        f.extend([0, 0]);
    }
    f.extend(body);
    f
}

/// A track or disc number pair as ID3v2 writes it. A total without a
/// number is "0/12": "/12" would read back as number 12.
fn number_pair(n: Option<u32>, total: Option<u32>) -> Option<String> {
    match (n, total) {
        (None, None) => None,
        (n, None) => n.map(|n| n.to_string()),
        (n, Some(t)) => Some(format!("{}/{t}", n.unwrap_or(0))),
    }
}

/// The frames an item replaces in `version`, the id of the one it writes,
/// and its text.
fn frame_for(item: &Item, version: u8) -> (&'static [&'static str], Option<String>) {
    let v2 = version == 2;
    let pick = |a: &'static [&'static str], b: &'static [&'static str]| if v2 { a } else { b };
    match item {
        Item::Title(v) => (pick(&["TT2"], &["TIT2"]), v.clone()),
        Item::Artist(v) => (pick(&["TP1"], &["TPE1"]), v.clone()),
        Item::Album(v) => (pick(&["TAL"], &["TALB"]), v.clone()),
        Item::AlbumArtist(v) => (pick(&["TP2"], &["TPE2"]), v.clone()),
        Item::Composer(v) => (pick(&["TCM"], &["TCOM"]), v.clone()),
        Item::Genre(v) => (pick(&["TCO"], &["TCON"]), v.clone()),
        // A date in other frames goes with the year it replaces.
        Item::Year(y) => (
            match version {
                2 => &["TYE", "TDA", "TIM"],
                3 => &["TYER", "TDAT", "TIME", "TDRC"],
                _ => &["TDRC", "TYER", "TDAT", "TIME"],
            },
            y.map(|y| y.to_string()),
        ),
        Item::Track(n, t) => (pick(&["TRK"], &["TRCK"]), number_pair(*n, *t)),
        Item::Disc(n, t) => (pick(&["TPA"], &["TPOS"]), number_pair(*n, *t)),
        Item::Compilation(on) => (pick(&["TCP"], &["TCMP"]), on.then(|| "1".to_string())),
    }
}

/// The tag `p` with `items` written: the frames of each replaced by one
/// new frame where the first of them was, or at the end.
fn build(p: Parsed<'_>, items: &[Item]) -> Vec<u8> {
    let Parsed {
        version,
        plain_sizes,
        mut frames,
        body_len,
    } = p;
    for item in items {
        let (ids, text) = frame_for(item, version);
        let at = frames.iter().position(|f| ids.contains(&f.id.as_str()));
        frames.retain(|f| !ids.contains(&f.id.as_str()));
        if let Some(text) = text {
            let id = ids[0];
            let new = RawFrame {
                id: id.to_string(),
                bytes: text_frame(version, plain_sizes, id, &text).into(),
            };
            match at {
                Some(i) => frames.insert(i, new),
                None => frames.push(new),
            }
        }
    }
    let used: usize = frames.iter().map(|f| f.bytes.len()).sum();
    // A tag that still fits keeps its size, so the audio does not move.
    let size = if used <= body_len {
        body_len
    } else {
        used + PADDING
    };
    let mut out = Vec::with_capacity(size + 10);
    out.extend_from_slice(b"ID3");
    out.extend([version, 0, 0]);
    out.extend(put_syncsafe(size));
    for f in &frames {
        out.extend_from_slice(&f.bytes);
    }
    out.resize(size + 10, 0);
    out
}

/// A new ID3v2.3 tag holding `items`.
fn new_tag(items: &[Item]) -> Vec<u8> {
    let empty = Parsed {
        version: 3,
        plain_sizes: false,
        frames: Vec::new(),
        body_len: 0,
    };
    build(empty, items)
}

/// The tag `tag` (an ID3 chunk's body, which must start with the tag)
/// with the items written, or a new tag holding every value when there is
/// none. `items(all)` gives the items, every one with a value for a new
/// tag.
pub(super) fn rewrite_tag(
    tag: Option<&[u8]>,
    items: impl Fn(bool) -> Vec<Item>,
) -> std::result::Result<Vec<u8>, String> {
    match tag {
        Some(t) => Ok(build(parse(t)?, &items(false))),
        None => Ok(new_tag(&items(true))),
    }
}

/// An MP3 file with the items written into the ID3v2 tag at its start, or
/// into a new one placed there. The audio after the tag is copied as it is.
pub(super) fn rewrite_mp3(
    data: &[u8],
    items: impl Fn(bool) -> Vec<Item>,
) -> std::result::Result<Vec<u8>, String> {
    let (tag, rest) = match tag_len(data) {
        Some(len) => {
            let len = len?;
            (
                Some(&data[..len.min(data.len())]),
                &data[len.min(data.len())..],
            )
        }
        None => (None, data),
    };
    let mut out = rewrite_tag(tag, items)?;
    out.extend_from_slice(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(version: u8, frames: &[Vec<u8>], padding: usize) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let mut t = b"ID3".to_vec();
        t.extend([version, 0, 0]);
        t.extend(put_syncsafe(body.len() + padding));
        t.extend(body);
        t.resize(t.len() + padding, 0);
        t
    }

    fn ids(t: &[u8]) -> Vec<String> {
        parse(t).unwrap().frames.into_iter().map(|f| f.id).collect()
    }

    #[test]
    fn untouched_frames_keep_their_bytes_and_order() {
        let priv_frame = {
            let mut f = b"PRIV".to_vec();
            f.extend(5u32.to_be_bytes());
            f.extend([0, 0, b'x', 0, 1, 2, 3]);
            f
        };
        let title = text_frame(3, false, "TIT2", "Old");
        let artist = text_frame(3, false, "TPE1", "a-ha");
        let old = tag(3, &[title, priv_frame.clone(), artist.clone()], 100);
        let new = build(
            parse(&old).unwrap(),
            &[
                Item::Title(Some("New".into())),
                Item::Genre(Some("Pop".into())),
            ],
        );
        assert_eq!(new.len(), old.len(), "fits in the padding");
        assert_eq!(ids(&new), ["TIT2", "PRIV", "TPE1", "TCON"]);
        let p = parse(&new).unwrap();
        assert_eq!(*p.frames[1].bytes, priv_frame[..]);
        assert_eq!(*p.frames[2].bytes, artist[..]);
        assert_eq!(*p.frames[0].bytes, text_frame(3, false, "TIT2", "New")[..]);
    }

    #[test]
    fn a_tag_that_outgrows_its_padding_gets_more() {
        let old = tag(4, &[text_frame(4, false, "TIT2", "x")], 0);
        let new = build(
            parse(&old).unwrap(),
            &[Item::Composer(Some("c".repeat(50)))],
        );
        let p = parse(&new).unwrap();
        let used: usize = p.frames.iter().map(|f| f.bytes.len()).sum();
        assert_eq!(p.body_len, used + PADDING);
    }

    #[test]
    fn version_2_2_uses_three_letter_frames() {
        let old = tag(2, &[text_frame(2, false, "TT2", "Old")], 20);
        let new = build(
            parse(&old).unwrap(),
            &[
                Item::Title(Some("Neu".into())),
                Item::Track(Some(3), Some(12)),
            ],
        );
        assert_eq!(new[3], 2);
        assert_eq!(ids(&new), ["TT2", "TRK"]);
    }

    #[test]
    fn clearing_removes_every_frame_of_the_field() {
        let old = tag(
            3,
            &[
                text_frame(3, false, "TYER", "1984"),
                text_frame(3, false, "TDAT", "0101"),
                text_frame(3, false, "TIT2", "T"),
            ],
            0,
        );
        let new = build(parse(&old).unwrap(), &[Item::Year(None)]);
        assert_eq!(ids(&new), ["TIT2"]);
    }

    #[test]
    fn text_encoding_follows_the_version() {
        assert_eq!(text_frame(3, false, "TIT2", "é")[10..], [0, 0xe9]);
        assert_eq!(
            text_frame(3, false, "TIT2", "€")[10..],
            [1, 0xff, 0xfe, 0xac, 0x20]
        );
        assert_eq!(
            text_frame(4, false, "TIT2", "€")[10..],
            [3, 0xe2, 0x82, 0xac]
        );
    }

    #[test]
    fn plain_sized_v24_frames_are_walked_and_followed() {
        // 200 bytes: 0xC8 is not a syncsafe byte.
        let long = "x".repeat(199);
        let frame = text_frame(4, true, "TIT2", &long);
        let old = tag(4, &[frame], 0);
        let p = parse(&old).unwrap();
        assert!(p.plain_sizes);
        let new = build(p, &[Item::Artist(Some("y".repeat(199)))]);
        let p = parse(&new).unwrap();
        assert!(p.plain_sizes);
        assert_eq!(p.frames.len(), 2);
    }

    #[test]
    fn header_flags_are_refused() {
        let mut t = tag(3, &[text_frame(3, false, "TIT2", "T")], 0);
        t[5] = 0x80;
        assert!(parse(&t).is_err());
    }

    #[test]
    fn mp3_without_a_tag_gets_one_in_front() {
        let audio = [0xff, 0xfb, 0x90, 0x00, 1, 2, 3];
        let out = rewrite_mp3(&audio, |all| {
            assert!(all);
            vec![Item::Title(Some("T".into()))]
        })
        .unwrap();
        assert_eq!(&out[..4], b"ID3\x03");
        assert!(out.ends_with(&audio));
        assert_eq!(ids(&out), ["TIT2"]);
    }
}
