//! MP4 (m4a) files: the iTunes tag is the `ilst` atom in
//! `moov/udta/meta`. Only the item atoms of edited fields change. A size
//! change is taken up by the `free` atom iTunes leaves after `ilst` when
//! there is room; otherwise the containers grow or shrink, and when the
//! audio follows `moov` its chunk offsets move with it.

use super::Item;

#[derive(Debug, Clone, Copy)]
struct Atom {
    name: [u8; 4],
    start: usize,
    /// Where the content starts: after an 8 or 16 byte header.
    body: usize,
    end: usize,
}

fn be32(b: &[u8]) -> usize {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
}

fn children(data: &[u8], from: usize, to: usize) -> std::result::Result<Vec<Atom>, String> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos + 8 <= to {
        let mut size = be32(&data[pos..]);
        let mut header = 8;
        if size == 1 {
            if pos + 16 > to {
                return Err("an MP4 atom header is cut short".into());
            }
            size = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap())
                .try_into()
                .map_err(|_| "an MP4 atom is too large".to_string())?;
            header = 16;
        } else if size == 0 {
            size = to - pos;
        }
        if size < header || pos + size > to {
            return Err("the MP4 atoms do not fit together".into());
        }
        out.push(Atom {
            name: data[pos + 4..pos + 8].try_into().unwrap(),
            start: pos,
            body: pos + header,
            end: pos + size,
        });
        pos += size;
    }
    Ok(out)
}

fn find(atoms: &[Atom], name: &[u8; 4]) -> std::result::Result<Option<Atom>, String> {
    let mut found = atoms.iter().filter(|a| &a.name == name);
    match (found.next(), found.next()) {
        (Some(_), Some(_)) => Err(format!(
            "the file has several {:?} atoms",
            String::from_utf8_lossy(name)
        )),
        (a, _) => Ok(a.copied()),
    }
}

/// An `ilst` item: the atom named `fourcc` holding one `data` atom.
fn item_atom(fourcc: &[u8; 4], kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut a = Vec::with_capacity(24 + payload.len());
    a.extend(((24 + payload.len()) as u32).to_be_bytes());
    a.extend_from_slice(fourcc);
    a.extend(((16 + payload.len()) as u32).to_be_bytes());
    a.extend_from_slice(b"data");
    a.extend(kind.to_be_bytes());
    a.extend([0; 4]);
    a.extend_from_slice(payload);
    a
}

const UTF8: u32 = 1;
const IMPLICIT: u32 = 0;
const INTEGER: u32 = 21;

/// The item atoms an item replaces, and the one it writes.
fn atom_for(item: &Item) -> (&'static [&'static [u8; 4]], Option<Vec<u8>>) {
    let text = |fourcc: &'static [u8; 4], v: &Option<String>| {
        v.as_ref().map(|v| item_atom(fourcc, UTF8, v.as_bytes()))
    };
    // `trkn` holds 8 bytes, `disk` 6, as iTunes writes them.
    let pair = |fourcc: &'static [u8; 4], n: Option<u32>, t: Option<u32>, len: usize| {
        (n.is_some() || t.is_some()).then(|| {
            let n = (n.unwrap_or(0) as u16).to_be_bytes();
            let t = (t.unwrap_or(0) as u16).to_be_bytes();
            let mut p = vec![0, 0, n[0], n[1], t[0], t[1], 0, 0];
            p.truncate(len);
            item_atom(fourcc, IMPLICIT, &p)
        })
    };
    match item {
        Item::Title(v) => (&[b"\xa9nam"], text(b"\xa9nam", v)),
        Item::Artist(v) => (&[b"\xa9ART"], text(b"\xa9ART", v)),
        Item::Album(v) => (&[b"\xa9alb"], text(b"\xa9alb", v)),
        Item::AlbumArtist(v) => (&[b"aART"], text(b"aART", v)),
        Item::Composer(v) => (&[b"\xa9wrt"], text(b"\xa9wrt", v)),
        // A numeric genre (`gnre`) would read as a second value.
        Item::Genre(v) => (&[b"\xa9gen", b"gnre"], text(b"\xa9gen", v)),
        Item::Year(y) => (
            &[b"\xa9day"],
            y.map(|y| item_atom(b"\xa9day", UTF8, y.to_string().as_bytes())),
        ),
        Item::Track(n, t) => (&[b"trkn"], pair(b"trkn", *n, *t, 8)),
        Item::Disc(n, t) => (&[b"disk"], pair(b"disk", *n, *t, 6)),
        Item::Compilation(on) => (&[b"cpil"], on.then(|| item_atom(b"cpil", INTEGER, &[1]))),
    }
}

fn put_size(buf: &mut [u8], at: usize, size: usize) -> std::result::Result<(), String> {
    let s = u32::try_from(size).map_err(|_| "an MP4 atom would be too large".to_string())?;
    buf[at..at + 4].copy_from_slice(&s.to_be_bytes());
    Ok(())
}

/// Adds `delta` to every chunk offset in `moov` (in `buf`) that points at
/// or past `from`.
fn shift_offsets(
    buf: &mut [u8],
    moov: Atom,
    from: usize,
    delta: isize,
) -> std::result::Result<(), String> {
    let overflow = || "the audio offsets would overflow".to_string();
    for trak in children(buf, moov.body, moov.end)?
        .into_iter()
        .filter(|a| &a.name == b"trak")
    {
        let mut tables = Vec::new();
        for mdia in children(buf, trak.body, trak.end)?
            .into_iter()
            .filter(|a| &a.name == b"mdia")
        {
            for minf in children(buf, mdia.body, mdia.end)?
                .into_iter()
                .filter(|a| &a.name == b"minf")
            {
                for stbl in children(buf, minf.body, minf.end)?
                    .into_iter()
                    .filter(|a| &a.name == b"stbl")
                {
                    tables.extend(
                        children(buf, stbl.body, stbl.end)?
                            .into_iter()
                            .filter(|a| &a.name == b"stco" || &a.name == b"co64"),
                    );
                }
            }
        }
        for t in tables {
            let width = if &t.name == b"co64" { 8 } else { 4 };
            if t.body + 8 > t.end {
                return Err("a chunk offset table is cut short".into());
            }
            let count = be32(&buf[t.body + 4..]);
            if t.body + 8 + count * width > t.end {
                return Err("a chunk offset table is cut short".into());
            }
            for i in 0..count {
                let at = t.body + 8 + i * width;
                let old = if width == 4 {
                    be32(&buf[at..]) as u64
                } else {
                    u64::from_be_bytes(buf[at..at + 8].try_into().unwrap())
                };
                if old < from as u64 {
                    continue;
                }
                let new = old.checked_add_signed(delta as i64).ok_or_else(overflow)?;
                if width == 4 {
                    let new = u32::try_from(new).map_err(|_| overflow())?;
                    buf[at..at + 4].copy_from_slice(&new.to_be_bytes());
                } else {
                    buf[at..at + 8].copy_from_slice(&new.to_be_bytes());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn rewrite(data: &[u8], items: &[Item]) -> std::result::Result<Vec<u8>, String> {
    let no_tag = || "the file has no iTunes tag (ilst atom) to edit".to_string();
    let top = children(data, 0, data.len())?;
    let moov = find(&top, b"moov")?.ok_or("the file has no moov atom")?;
    // Fragments hold offsets of their own that a change to `moov` would
    // leave pointing at the wrong bytes.
    if top
        .iter()
        .any(|a| matches!(&a.name, b"moof" | b"sidx" | b"mfra"))
    {
        return Err("fragmented MP4 files are not supported".into());
    }
    let udta = find(&children(data, moov.body, moov.end)?, b"udta")?.ok_or_else(no_tag)?;
    let meta = find(&children(data, udta.body, udta.end)?, b"meta")?.ok_or_else(no_tag)?;
    // `meta` is a full atom (4 bytes of version and flags) in iTunes files,
    // a plain one in some QuickTime files.
    let meta_kids = match (
        data.get(meta.body + 4..meta.body + 8),
        data.get(meta.body + 8..meta.body + 12),
    ) {
        (_, Some(b"hdlr")) => meta.body + 4,
        (Some(b"hdlr"), _) => meta.body,
        _ => return Err("the meta atom has no handler".into()),
    };
    let kids = children(data, meta_kids, meta.end)?;
    let ilst_at = kids
        .iter()
        .position(|a| &a.name == b"ilst")
        .ok_or_else(no_tag)?;
    let ilst = kids[ilst_at];
    for a in [moov, udta, meta, ilst] {
        if a.body - a.start != 8 {
            return Err("MP4 containers with 64-bit sizes are not supported".into());
        }
    }

    // The new `ilst`: each item's atoms replaced where the first of them
    // was, or added at the end.
    let mut entries: Vec<(&[u8; 4], std::borrow::Cow<'_, [u8]>)> =
        children(data, ilst.body, ilst.end)?
            .into_iter()
            .map(|a| {
                (
                    <&[u8; 4]>::try_from(&data[a.start + 4..a.start + 8]).unwrap(),
                    data[a.start..a.end].into(),
                )
            })
            .collect();
    for item in items {
        let (names, new) = atom_for(item);
        let at = entries.iter().position(|(n, _)| names.contains(n));
        entries.retain(|(n, _)| !names.contains(n));
        if let Some(new) = new {
            let e = (names[0], new.into());
            match at {
                Some(i) => entries.insert(i, e),
                None => entries.push(e),
            }
        }
    }
    let mut new_ilst = Vec::new();
    new_ilst.extend([0; 4]);
    new_ilst.extend_from_slice(b"ilst");
    for (_, bytes) in &entries {
        new_ilst.extend_from_slice(bytes);
    }
    let len = new_ilst.len();
    put_size(&mut new_ilst, 0, len)?;
    let delta = new_ilst.len() as isize - (ilst.end - ilst.start) as isize;

    // A `free` atom right after `ilst` takes up the change when it can,
    // keeping at least its own header.
    let free = kids
        .get(ilst_at + 1)
        .filter(|a| &a.name == b"free" && a.body - a.start == 8)
        .filter(|a| (a.end - a.start) as isize - delta >= 8)
        .copied();
    let (replaced_end, filler) = match free {
        Some(f) => {
            let size = ((f.end - f.start) as isize - delta) as usize;
            let mut filler = vec![0; size];
            put_size(&mut filler, 0, size)?;
            filler[4..8].copy_from_slice(b"free");
            (f.end, filler)
        }
        None => (ilst.end, Vec::new()),
    };
    let growth = if free.is_some() { 0 } else { delta };

    let mut moov_bytes = Vec::with_capacity(moov.end - moov.start + new_ilst.len());
    moov_bytes.extend_from_slice(&data[moov.start..ilst.start]);
    moov_bytes.extend_from_slice(&new_ilst);
    moov_bytes.extend_from_slice(&filler);
    moov_bytes.extend_from_slice(&data[replaced_end..moov.end]);
    if growth != 0 {
        for a in [moov, udta, meta] {
            let size = ((a.end - a.start) as isize + growth) as usize;
            put_size(&mut moov_bytes, a.start - moov.start, size)?;
        }
        let new_moov = Atom {
            name: *b"moov",
            start: 0,
            body: 8,
            end: moov_bytes.len(),
        };
        // The audio after `moov` moves by the change.
        shift_offsets(&mut moov_bytes, new_moov, moov.end, growth)?;
    }
    let mut out = Vec::with_capacity(data.len() + moov_bytes.len());
    out.extend_from_slice(&data[..moov.start]);
    out.extend_from_slice(&moov_bytes);
    out.extend_from_slice(&data[moov.end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut a = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        a.extend_from_slice(name);
        a.extend_from_slice(body);
        a
    }

    fn stco(offsets: &[u32]) -> Vec<u8> {
        let mut b = vec![0; 4];
        b.extend((offsets.len() as u32).to_be_bytes());
        for o in offsets {
            b.extend(o.to_be_bytes());
        }
        atom(b"stco", &b)
    }

    /// ftyp, moov (one trak, udta/meta/ilst, optional free), mdat after it.
    fn file(ilst_items: &[Vec<u8>], free: Option<usize>, offset: u32) -> Vec<u8> {
        let trak = atom(
            b"trak",
            &atom(b"mdia", &atom(b"minf", &atom(b"stbl", &stco(&[offset])))),
        );
        let mut meta_body = vec![0; 4];
        meta_body.extend(atom(b"hdlr", &[0; 25]));
        meta_body.extend(atom(b"ilst", &ilst_items.concat()));
        if let Some(n) = free {
            meta_body.extend(atom(b"free", &vec![0; n]));
        }
        let mut moov_body = trak;
        moov_body.extend(atom(b"udta", &atom(b"meta", &meta_body)));
        let mut f = atom(b"ftyp", b"M4A ");
        f.extend(atom(b"moov", &moov_body));
        f.extend(atom(b"mdat", b"audio"));
        f
    }

    fn mdat_offset(f: &[u8]) -> usize {
        let top = children(f, 0, f.len()).unwrap();
        find(&top, b"mdat").unwrap().unwrap().body
    }

    fn stco_entry(f: &[u8]) -> usize {
        let i = f.windows(4).position(|w| w == b"stco").unwrap();
        be32(&f[i + 12..])
    }

    #[test]
    fn padding_takes_up_the_growth() {
        let artist = item_atom(b"\xa9ART", UTF8, b"a-ha");
        let old = file(std::slice::from_ref(&artist), Some(100), 0);
        let off = mdat_offset(&old) as u32;
        let old = file(std::slice::from_ref(&artist), Some(100), off);
        let new = rewrite(&old, &[Item::Title(Some("Take On Me".into()))]).unwrap();
        assert_eq!(new.len(), old.len());
        assert_eq!(mdat_offset(&new), off as usize);
        assert_eq!(stco_entry(&new), off as usize);
        let i = new.windows(4).position(|w| w == b"ilst").unwrap();
        let ilst = &new[i - 4..];
        assert_eq!(&ilst[8..8 + artist.len()], &artist[..], "kept, first");
    }

    #[test]
    fn without_padding_the_offsets_move() {
        let old0 = file(&[], None, 0);
        let off = mdat_offset(&old0) as u32;
        let old = file(&[], None, off);
        let new = rewrite(&old, &[Item::Year(Some(1985))]).unwrap();
        let grown = new.len() - old.len();
        assert_eq!(grown, item_atom(b"\xa9day", UTF8, b"1985").len());
        assert_eq!(mdat_offset(&new), off as usize + grown);
        assert_eq!(stco_entry(&new), off as usize + grown);
        assert_eq!(&new[mdat_offset(&new)..], b"audio");
    }

    #[test]
    fn genre_replaces_a_numeric_genre_and_clearing_removes() {
        let gnre = item_atom(b"gnre", IMPLICIT, &[0, 17]);
        let cpil = item_atom(b"cpil", INTEGER, &[1]);
        let old = file(&[gnre, cpil], Some(64), 0);
        let new = rewrite(
            &old,
            &[Item::Genre(Some("Rock".into())), Item::Compilation(false)],
        )
        .unwrap();
        assert!(!new.windows(4).any(|w| w == b"gnre"));
        assert!(!new.windows(4).any(|w| w == b"cpil"));
        assert!(new.windows(4).any(|w| w == b"\xa9gen"));
    }

    #[test]
    fn a_file_without_ilst_is_refused() {
        let mut f = atom(b"ftyp", b"M4A ");
        f.extend(atom(b"moov", &atom(b"mvhd", &[0; 10])));
        assert!(rewrite(&f, &[Item::Year(Some(1))]).is_err());
    }
}
