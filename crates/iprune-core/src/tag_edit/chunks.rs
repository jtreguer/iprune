//! WAV (RIFF) and AIFF (IFF) files: the ID3v2 tag lives in a chunk of its
//! own. Every other chunk keeps its bytes and its place.

/// The file `data` with its ID3 chunk replaced by what `tag` makes of the
/// old one, or a new chunk at the end when there was none.
pub(super) fn rewrite(
    data: &[u8],
    tag: impl FnOnce(Option<&[u8]>) -> std::result::Result<Vec<u8>, String>,
) -> std::result::Result<Vec<u8>, String> {
    if data.len() < 12 {
        return Err("the file is too short".into());
    }
    let (little, name): (bool, &[u8; 4]) = match (&data[..4], &data[8..12]) {
        (b"RIFF", b"WAVE") => (true, b"id3 "),
        (b"FORM", b"AIFF" | b"AIFC") => (false, b"ID3 "),
        _ => return Err("not a RIFF WAVE or AIFF file".into()),
    };
    let read = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        (if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }) as usize
    };
    let write = |n: usize| -> std::result::Result<[u8; 4], String> {
        let n = u32::try_from(n).map_err(|_| "the file would be too large".to_string())?;
        Ok(if little {
            n.to_le_bytes()
        } else {
            n.to_be_bytes()
        })
    };
    let end = 8 + read(&data[4..8]);
    if end > data.len() {
        return Err("the file is shorter than its header says".into());
    }
    // (start, end with padding) of each chunk, and the ID3 chunk's index.
    let mut chunks = Vec::new();
    let mut id3 = None;
    // Whether the last chunk is odd-sized and its pad byte missing.
    let mut unpadded = false;
    let mut pos = 12;
    while pos + 8 <= end {
        let size = read(&data[pos + 4..pos + 8]);
        let body_end = pos + 8 + size;
        if body_end > end {
            return Err(format!(
                "the chunk {:?} runs past the end of the file",
                String::from_utf8_lossy(&data[pos..pos + 4])
            ));
        }
        if data[pos..pos + 4].eq_ignore_ascii_case(b"id3 ") {
            if id3.is_some() {
                return Err("the file has several ID3 chunks".into());
            }
            id3 = Some(chunks.len());
        }
        // Chunks are padded to an even length; a missing last pad byte is
        // tolerated.
        let next = (body_end + (size & 1)).min(end);
        unpadded = body_end + (size & 1) > end;
        chunks.push((pos, next));
        pos = next;
    }
    if pos != end {
        return Err("the chunks do not fill the file".into());
    }

    let chunk = |name: &[u8], body: &[u8]| -> std::result::Result<Vec<u8>, String> {
        let mut c = Vec::with_capacity(body.len() + 9);
        c.extend_from_slice(name);
        c.extend(write(body.len())?);
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        Ok(c)
    };
    let mut out = data[..12].to_vec();
    match id3 {
        Some(i) => {
            let (start, next) = chunks[i];
            let size = read(&data[start + 4..start + 8]);
            let new = tag(Some(&data[start + 8..start + 8 + size]))?;
            out.extend_from_slice(&data[12..start]);
            out.extend(chunk(&data[start..start + 4], &new)?);
            out.extend_from_slice(&data[next..end]);
        }
        None => {
            let new = tag(None)?;
            out.extend_from_slice(&data[12..end]);
            // The new chunk must start on an even offset.
            if unpadded {
                out.push(0);
            }
            out.extend(chunk(name, &new)?);
        }
    }
    let size = write(out.len() - 8)?;
    out[4..8].copy_from_slice(&size);
    // Anything after the container (an ID3v1 tag some programs append)
    // stays after it.
    out.extend_from_slice(&data[end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn riff(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        for (n, b) in chunks {
            body.extend_from_slice(*n);
            body.extend((b.len() as u32).to_le_bytes());
            body.extend_from_slice(b);
            if b.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut f = b"RIFF".to_vec();
        f.extend((body.len() as u32).to_le_bytes());
        f.extend(body);
        f
    }

    #[test]
    fn the_id3_chunk_is_replaced_in_place() {
        let f = riff(&[(b"fmt ", b"16b"), (b"id3 ", b"old"), (b"data", b"pcm!")]);
        let out = rewrite(&f, |old| {
            assert_eq!(old, Some(&b"old"[..]));
            Ok(b"newer".to_vec())
        })
        .unwrap();
        assert_eq!(
            out,
            riff(&[(b"fmt ", b"16b"), (b"id3 ", b"newer"), (b"data", b"pcm!")])
        );
    }

    #[test]
    fn a_file_without_one_gets_it_at_the_end() {
        let f = riff(&[(b"fmt ", b"16b"), (b"data", b"pcm")]);
        let out = rewrite(&f, |old| {
            assert_eq!(old, None);
            Ok(b"tag".to_vec())
        })
        .unwrap();
        assert_eq!(
            out,
            riff(&[(b"fmt ", b"16b"), (b"data", b"pcm"), (b"id3 ", b"tag")])
        );
    }

    #[test]
    fn bytes_after_the_container_are_kept() {
        let mut f = riff(&[(b"data", b"pcm!")]);
        f.extend(b"TAG trailing");
        let out = rewrite(&f, |_| Ok(b"t!".to_vec())).unwrap();
        assert!(out.ends_with(b"TAG trailing"));
        assert_eq!(
            &out[..out.len() - 12],
            &riff(&[(b"data", b"pcm!"), (b"id3 ", b"t!")])[..]
        );
    }

    #[test]
    fn a_new_chunk_after_an_unpadded_odd_chunk_starts_on_an_even_offset() {
        let mut f = riff(&[(b"fmt ", b"16b"), (b"data", b"pcm")]);
        // Drop the data chunk's pad byte, as some writers do.
        f.pop();
        let n = (f.len() - 8) as u32;
        f[4..8].copy_from_slice(&n.to_le_bytes());
        let out = rewrite(&f, |_| Ok(b"tag".to_vec())).unwrap();
        assert_eq!(
            out,
            riff(&[(b"fmt ", b"16b"), (b"data", b"pcm"), (b"id3 ", b"tag")])
        );
    }

    #[test]
    fn a_broken_chunk_list_is_refused() {
        let mut f = riff(&[(b"data", b"pcm!")]);
        f[16] = 200; // the data chunk now runs past the end
        assert!(rewrite(&f, |_| Ok(Vec::new())).is_err());
    }
}
