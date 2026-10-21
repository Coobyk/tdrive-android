//! The bridge, walked against a real drive.
//!
//! These tests need a signed-in account and a drive, so the one that reaches
//! Telegram is ignored by default and run on purpose:
//!
//! ```text
//! cargo test -- --ignored --nocapture
//! ```

use tdrive_engine::bridge;

/// The account's own credentials, as the application stores them.
fn credentials() -> (i32, String) {
    let raw = std::fs::read_to_string(tdrive_rs::config::data_dir().join("imp_config.json"))
        .expect("the application credentials");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("the credentials to be read");
    (
        json.get("API_ID")
            .and_then(|value| value.as_i64())
            .unwrap_or_default() as i32,
        json.get("API_HASH")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    )
}

/// The account signs in, the drive opens, and the files come back.
#[test]
#[ignore = "needs a signed-in account and a drive"]
fn the_bridge_reads_a_drive() {
    let (api_id, api_hash) = credentials();

    // The session is already authorised, so no code is wanted.
    let answer = bridge::sign_in(api_id, &api_hash, "+10000000000");
    assert!(
        answer.get("ok").and_then(|value| value.as_bool()).unwrap_or(false),
        "{answer}"
    );
    assert_eq!(bridge::session_state(), "ready");

    let drives = bridge::list_drives();
    let list = drives
        .get("drives")
        .and_then(|value| value.as_array())
        .expect("the drives to be listed");
    assert!(!list.is_empty(), "{drives}");

    // The first drive is the one the test device uses.
    let drive = &list[0];
    let opened = bridge::open_drive(
        drive.get("id").and_then(|value| value.as_i64()).unwrap_or_default(),
        drive
            .get("accessHash")
            .and_then(|value| value.as_i64())
            .unwrap_or_default(),
    );
    assert!(
        opened.get("ok").and_then(|value| value.as_bool()).unwrap_or(false),
        "{opened}"
    );
    assert!(
        opened.get("items").and_then(|value| value.as_i64()).unwrap_or_default() > 0,
        "{opened}"
    );

    // The files come back with the names, the kinds and the parents the phone
    // draws them by.
    let files = bridge::files_json();
    let rows = files
        .get("files")
        .and_then(|value| value.as_array())
        .expect("the files to be listed");
    assert!(!rows.is_empty(), "{files}");
    let first = &rows[0];
    for key in [
        "messageId",
        "objectId",
        "name",
        "size",
        "kind",
        "parent",
        "revision",
    ] {
        assert!(first.get(key).is_some(), "{key} is missing from {first}");
    }

    // An encrypted drive needs its key before it will give anything up. The
    // passphrase comes from the environment, so the test carries no secret.
    if let Ok(passphrase) = std::env::var("TDRIVE_TEST_PASSPHRASE") {
        let unlocked = bridge::unlock(&passphrase);
        assert!(
            unlocked.get("ok").and_then(|value| value.as_bool()).unwrap_or(false),
            "{unlocked}"
        );
    }

    // A file that has a small picture beside it gives that picture up, and
    // the phone turns it into a bitmap.
    if let Some(row) = rows
        .iter()
        .find(|row| row.get("derivative").and_then(|value| value.as_i64()).is_some())
    {
        let picture = bridge::derivative(row.get("messageId").unwrap().as_i64().unwrap() as i32);
        assert!(
            picture.get("ok").and_then(|value| value.as_bool()).unwrap_or(false),
            "{picture}"
        );
        let bytes = picture
            .get("bytes")
            .and_then(|value| value.as_array())
            .expect("the picture to come as bytes");
        assert!(!bytes.is_empty());
    }

    // A file comes down whole, and lands where the phone asked.
    let file = rows
        .iter()
        .find(|row| row.get("kind").and_then(|value| value.as_str()) != Some("folder"))
        .expect("a file to fetch");
    let target = std::env::temp_dir().join("tdrive-bridge-test.bin");
    let answer = bridge::download(
        file.get("messageId").unwrap().as_i64().unwrap() as i32,
        &target.to_string_lossy(),
    );
    if answer.get("ok").and_then(|value| value.as_bool()).unwrap_or(false) {
        assert!(target.exists());
        let _ = std::fs::remove_file(&target);
    } else {
        // A locked drive says so, and that is the whole of the answer.
        assert!(answer.get("error").is_some(), "{answer}");
    }
}

/// The theme catalogue reaches the phone with the whole palette.
#[test]
fn the_catalogue_reaches_the_phone() {
    let themes = bridge::themes_json();
    let list = themes
        .get("themes")
        .and_then(|value| value.as_array())
        .expect("the themes to be listed");
    assert_eq!(list.len(), 17);
    let first = &list[0];
    assert!(first.get("id").is_some());
    assert!(first.get("name").is_some());
    let palette = first.get("palette").expect("the palette to be there");
    for token in [
        "canvas",
        "surface1",
        "text",
        "accent",
        "onAccent",
        "danger",
    ] {
        assert!(palette.get(token).is_some(), "{token} is missing from {palette}");
    }
}
