use std::path::Path;

#[test]
fn missing_file_is_a_parse_error() {
    let err = gpod::Database::open_file(Path::new("/nonexistent/iTunesDB")).unwrap_err();
    assert!(matches!(err, gpod::Error::Parse { .. }), "{err}");
}

#[test]
fn nul_in_path_is_rejected() {
    use std::os::unix::ffi::OsStrExt;
    let p = Path::new(std::ffi::OsStr::from_bytes(b"a\0b"));
    assert!(matches!(
        gpod::Database::open_file(p),
        Err(gpod::Error::NulInPath(_))
    ));
}
