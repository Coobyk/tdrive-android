//! Where the engine keeps its files.
//!
//! The application says which directory they live in, because a phone has one
//! place it may write to and the platform's own answer on a phone is a
//! relative path — which is a path the phone cannot open.

#[test]
fn the_files_live_where_the_application_says() {
    let dir = std::env::temp_dir().join("tdrive-engine-data");
    let _ = std::fs::remove_dir_all(&dir);

    tdrive_rs::config::set_data_dir(dir.clone());
    assert_eq!(tdrive_rs::config::data_dir(), dir);

    // The directory is made, so the session has somewhere to live.
    tdrive_rs::config::ensure_data_dir().expect("the directory to be made");
    assert!(dir.is_dir());

    // The session is inside it, and the path is absolute: a relative path is
    // one the phone cannot open.
    let session = tdrive_rs::config::data_dir().join("tdrive.session");
    assert!(session.is_absolute(), "{} is not absolute", session.display());
    assert!(session.starts_with(&dir));
}
