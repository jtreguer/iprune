//! Path handling. Files are opened by their exact on-disk path; lookups use
//! the NFC normalized form, so a playlist entry matches a file whatever
//! Unicode normalization either of them uses.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Lookup key for a path: its NFC form. `None` for non-UTF-8 paths.
pub fn key(path: &Path) -> Option<String> {
    path.to_str().map(|s| s.nfc().collect())
}

/// Makes `path` absolute against `base` and removes `.` and `..` components
/// without touching the filesystem.
pub fn absolute(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_and_nfd_have_the_same_key() {
        let nfd = Path::new("/m/Jo\u{301}hann.mp3");
        let nfc = Path::new("/m/J\u{f3}hann.mp3");
        assert_ne!(nfd, nfc);
        assert_eq!(key(nfd), key(nfc));
    }

    #[test]
    fn absolute_is_lexical() {
        let base = Path::new("/mnt/x/Playlists");
        assert_eq!(
            absolute(Path::new("../Music/./a.mp3"), base),
            Path::new("/mnt/x/Music/a.mp3")
        );
        assert_eq!(
            absolute(Path::new("/abs/b.mp3"), base),
            Path::new("/abs/b.mp3")
        );
    }
}
