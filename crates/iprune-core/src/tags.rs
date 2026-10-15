//! Reading tags and audio properties from files.

use std::fs::File;
use std::path::Path;

use lofty::config::{ParseOptions, ParsingMode};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use lofty::tag::Tag;

/// Audio formats the iPod Classic plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Mp3,
    Aac,
    Alac,
    Wav,
    Aiff,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Mp3 => "mp3",
            FileKind::Aac => "aac",
            FileKind::Alac => "alac",
            FileKind::Wav => "wav",
            FileKind::Aiff => "aiff",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mp3" => FileKind::Mp3,
            "aac" => FileKind::Aac,
            "alac" => FileKind::Alac,
            "wav" => FileKind::Wav,
            "aiff" => FileKind::Aiff,
            _ => return None,
        })
    }
}

/// File extensions worth reading. Anything else under a library root is
/// ignored; these may still turn out unsupported (an m4a holding FLAC).
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "m4a", "wav", "aif", "aiff"];

/// Extensions of audio the iPod cannot play, reported by the scan.
pub const UNSUPPORTED_EXTENSIONS: &[&str] =
    &["flac", "ogg", "oga", "opus", "wma", "ape", "mpc", "wv"];

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub kind: FileKind,
    pub duration_ms: Option<u32>,
    pub bitrate: Option<u32>,
    pub samplerate: Option<u32>,
}

/// The fields synced to the iPod. Empty strings are stored as `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: bool,
}

impl Tags {
    /// BLAKE3 of the synced fields, used to tell when the iPod needs a tag
    /// update. Every field is length-prefixed so no two values collide.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        for s in [
            &self.title,
            &self.artist,
            &self.album,
            &self.album_artist,
            &self.composer,
            &self.genre,
        ] {
            match s {
                None => h.update(&[0]),
                Some(s) => h
                    .update(&[1])
                    .update(&(s.len() as u64).to_le_bytes())
                    .update(s.as_bytes()),
            };
        }
        for n in [
            self.year,
            self.track_no,
            self.track_total,
            self.disc_no,
            self.disc_total,
        ] {
            match n {
                None => h.update(&[0]),
                Some(n) => h.update(&[1]).update(&n.to_le_bytes()),
            };
        }
        h.update(&[self.compilation as u8]);
        *h.finalize().as_bytes()
    }
}

/// Why a file could not be indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Unsupported(String),
    Unreadable(String),
}

fn non_empty(s: Option<impl Into<String>>) -> Option<String> {
    s.map(Into::into).filter(|s: &String| !s.trim().is_empty())
}

fn tags_from(tag: &Tag) -> Tags {
    let flag = |k| {
        tag.get_string(k)
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "True" | "TRUE"))
    };
    Tags {
        title: non_empty(tag.title()),
        artist: non_empty(tag.artist()),
        album: non_empty(tag.album()),
        album_artist: non_empty(tag.get_string(ItemKey::AlbumArtist)),
        composer: non_empty(tag.get_string(ItemKey::Composer)),
        genre: non_empty(tag.genre()),
        year: tag.date().map(|d| u32::from(d.year)).filter(|&y| y > 0),
        track_no: tag.track().filter(|&n| n > 0),
        track_total: tag.track_total().filter(|&n| n > 0),
        disc_no: tag.disk().filter(|&n| n > 0),
        disc_total: tag.disk_total().filter(|&n| n > 0),
        compilation: flag(ItemKey::FlagCompilation),
    }
}

/// The codec of an MP4 file, and whether it is FairPlay protected.
fn mp4_kind(path: &Path) -> Result<(FileKind, bool), ReadError> {
    let mut f = File::open(path).map_err(|e| ReadError::Unreadable(e.to_string()))?;
    let opts = ParseOptions::new()
        .parsing_mode(ParsingMode::Relaxed)
        .read_tags(false);
    let mp4 = Mp4File::read_from(&mut f, opts).map_err(|e| ReadError::Unreadable(e.to_string()))?;
    let props = mp4.properties();
    match props.codec() {
        Some(Mp4Codec::AAC) => Ok((FileKind::Aac, props.is_drm_protected())),
        Some(Mp4Codec::ALAC) => Ok((FileKind::Alac, props.is_drm_protected())),
        // lofty cannot see the codec inside a FairPlay `drms` entry. The
        // iTunes Store only ever sold AAC under FairPlay.
        None if props.is_drm_protected() => Ok((FileKind::Aac, true)),
        other => Err(ReadError::Unsupported(format!("MP4 codec {other:?}"))),
    }
}

/// What [`read`] learned about a file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTags {
    pub info: AudioInfo,
    pub tags: Tags,
    /// Set when the tags could not be parsed and the file was read without
    /// them; `tags` is then empty.
    pub tag_error: Option<String>,
    /// FairPlay protected (an iTunes Store purchase).
    pub drm: bool,
}

fn open(path: &Path, opts: ParseOptions) -> Result<lofty::file::TaggedFile, String> {
    Probe::open(path)
        .map_err(|e| e.to_string())?
        .options(opts)
        .guess_file_type()
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())
}

/// Reads audio properties and tags. The kind comes from the content, not the
/// extension, so an m4a is told apart as AAC or ALAC.
///
/// Tags are parsed leniently, skipping bad frames. If they still cannot be
/// parsed the file is read without them: an empty tag never overwrites the
/// iPod's value, so the track loses nothing.
pub fn read(path: &Path) -> Result<FileTags, ReadError> {
    let relaxed = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    let (tagged, tag_error) = match open(path, relaxed) {
        Ok(t) => (t, None),
        Err(e) => match open(
            path,
            ParseOptions::new()
                .parsing_mode(ParsingMode::Relaxed)
                .read_tags(false),
        ) {
            Ok(t) => (t, Some(e)),
            Err(_) => return Err(ReadError::Unreadable(e)),
        },
    };
    let mut drm = false;
    let kind = match tagged.file_type() {
        FileType::Mpeg => FileKind::Mp3,
        FileType::Mp4 => {
            let (kind, protected) = mp4_kind(path)?;
            drm = protected;
            kind
        }
        FileType::Wav => FileKind::Wav,
        FileType::Aiff => FileKind::Aiff,
        other => return Err(ReadError::Unsupported(format!("{other:?}"))),
    };
    let props = tagged.properties();
    let ms = props.duration().as_millis();
    let info = AudioInfo {
        kind,
        duration_ms: (ms > 0).then(|| u32::try_from(ms).unwrap_or(u32::MAX)),
        bitrate: props.audio_bitrate().filter(|&b| b > 0),
        samplerate: props.sample_rate().filter(|&r| r > 0),
    };
    let tags = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .map(tags_from)
        .unwrap_or_default();
    Ok(FileTags {
        info,
        tags,
        tag_error,
        drm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_distinguishes_field_boundaries() {
        let a = Tags {
            title: Some("ab".into()),
            artist: Some("c".into()),
            ..Tags::default()
        };
        let b = Tags {
            title: Some("a".into()),
            artist: Some("bc".into()),
            ..Tags::default()
        };
        let c = Tags {
            title: Some(String::new()),
            ..Tags::default()
        };
        assert_ne!(a.hash(), b.hash());
        assert_ne!(c.hash(), Tags::default().hash());
        assert_eq!(a.hash(), a.clone().hash());
    }

    #[test]
    fn kind_round_trips() {
        for k in [
            FileKind::Mp3,
            FileKind::Aac,
            FileKind::Alac,
            FileKind::Wav,
            FileKind::Aiff,
        ] {
            assert_eq!(FileKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(FileKind::parse("flac"), None);
    }
}
