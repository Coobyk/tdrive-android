//! The bridge between the phone and the engine.
//!
//! Everything the application does goes through here: the Telegram session,
//! the drive, the files, the vault, and the small pictures published beside a
//! file. The engine itself is the same code the desktop application runs, so
//! the two read and write the same drive in the same way.
//!
//! The boundary is deliberately narrow and deliberately plain: a handful of
//! functions, each of which answers with a JSON document or a string. The
//! phone's side is Kotlin, and a JSON document is something both sides already
//! know how to read.

use std::path::{Path, PathBuf};

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring};
use jni::{JNIEnv, JavaStr};
use serde_json::{Value, json};
use tdrive_rs::config::{DriveConfig, StoredDrive};
use tdrive_rs::crypto;
use tdrive_rs::models::{AppState, ContentRef, Credentials, FileKind, RemoteFile, TrashEntry};
use tdrive_rs::ops;
use tdrive_rs::rendition;
use tdrive_rs::tele::{self, OwnedChannel};
use tdrive_rs::themes;

/// The Telegram client, as the engine holds it.
type Client = grammers_client::Client;
/// The peer a drive is addressed by.
type PeerRef = grammers_session::types::PeerRef;
/// The token that asks for the code.
type LoginToken = grammers_client::client::LoginToken;
/// The token that asks for the password.
type PasswordToken = grammers_client::client::PasswordToken;

/// What the engine holds while the application is open.
struct Engine {
    /// The Telegram client, once the account is signed in.
    client: Option<Client>,
    /// The token the code is checked against, between the two steps.
    login_token: Option<LoginToken>,
    /// The token the password is checked against, when the account has one.
    password_token: Option<PasswordToken>,
    /// Where the session lives, so the sign-in survives a restart.
    session_path: PathBuf,
    /// The drive that is open.
    drive: Option<OpenDrive>,
    /// The state the drive's contents are read into.
    state: AppState,
}

impl Engine {
    /// An engine with nothing in it yet.
    fn new() -> Self {
        Self {
            client: None,
            login_token: None,
            password_token: None,
            session_path: tdrive_rs::config::data_dir().join("tdrive.session"),
            drive: None,
            state: AppState::default(),
        }
    }
}

/// The drive that is open.
struct OpenDrive {
    id: i64,
    title: String,
    peer_ref: PeerRef,
}

/// The engine, if it is there.
static mut ENGINE: Option<Box<Engine>> = None;

/// The engine. There is exactly one, and the application never asks twice.
fn engine() -> Result<&'static mut Engine, String> {
    unsafe {
        if ENGINE.is_none() {
            ENGINE = Some(Box::new(Engine::new()));
        }
        Ok(ENGINE.as_mut().unwrap())
    }
}

/// A string for the phone.
fn string(env: &mut JNIEnv, value: &str) -> jstring {
    match env.new_string(value) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// A JSON document for the phone.
fn json_string(env: &mut JNIEnv, value: &Value) -> jstring {
    string(env, &value.to_string())
}

/// A string from the phone.
fn read_string(env: &mut JNIEnv, value: &JString) -> Result<String, String> {
    let value: JavaStr = env
        .get_string(value)
        .map_err(|_| "the string could not be read".to_string())?;
    Ok(value.into())
}

/// The error the phone is told about.
fn failure(message: &str) -> Value {
    json!({ "ok": false, "error": message })
}

/// The result the phone is told about.
fn success() -> Value {
    json!({ "ok": true })
}

// --- The session ------------------------------------------------------------

/// Set the engine up.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeInit<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = match engine() {
        Ok(_) => success(),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Whether the account is signed in.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeSessionState<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let state = match engine() {
        Ok(engine) => {
            if engine.client.is_some() {
                "ready"
            } else {
                "none"
            }
        }
        Err(_) => "none",
    };
    string(&mut env, state)
}

/// Sign in: the code is sent to the account's Telegram.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeSignIn<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    api_id: jint,
    api_hash: JString<'local>,
    phone: JString<'local>,
) -> jstring {
    let api_hash = match read_string(&mut env, &api_hash) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let phone = match read_string(&mut env, &phone) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => sign_in(engine, api_id as i32, &api_hash, &phone),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Sign in, and ask for the code.
fn sign_in(engine: &mut Engine, api_id: i32, api_hash: &str, phone: &str) -> Value {
    let credentials = Credentials {
        api_id,
        api_hash: api_hash.to_string(),
    };
    let client = match tele::connect(&credentials, engine.session_path.clone()) {
        Ok(client) => client,
        Err(message) => return failure(&message),
    };
    // A session that is already authorised needs no code at all.
    if tele::is_authorized(&client) {
        engine.client = Some(client);
        return success();
    }
    match tele::request_code(&client, phone, api_hash) {
        Ok(token) => {
            engine.client = Some(client);
            engine.login_token = Some(token);
            success()
        }
        Err(message) => failure(&message),
    }
}

/// Check the code the account was sent.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeVerifyCode<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    code: JString<'local>,
) -> jstring {
    let code = match read_string(&mut env, &code) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => verify_code(engine, &code),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Check the code, and say whether a password is still wanted.
fn verify_code(engine: &mut Engine, code: &str) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(token) = engine.login_token.take() else {
        return failure("the code was not asked for");
    };
    match tele::sign_in(client, token, code) {
        Ok(None) => success(),
        Ok(Some(password_token)) => {
            // The account has a password of its own; the phone asks for it.
            engine.password_token = Some(password_token);
            json!({ "ok": true, "needsPassword": true })
        }
        Err(message) => failure(&message),
    }
}

/// Check the password the account has.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeVerifyPassword<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    password: JString<'local>,
) -> jstring {
    let password = match read_string(&mut env, &password) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => verify_password(engine, &password),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Check the password.
fn verify_password(engine: &mut Engine, password: &str) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(token) = engine.password_token.take() else {
        return failure("no password was asked for");
    };
    match tele::check_password(client, token, password) {
        Ok(_) => success(),
        Err(message) => failure(&message),
    }
}

/// Forget the account.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeSignOut<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = match engine() {
        Ok(engine) => {
            engine.client = None;
            engine.drive = None;
            success()
        }
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

// --- The drive --------------------------------------------------------------

/// The channels this account may use as a drive.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeListDrives<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = match engine() {
        Ok(engine) => {
            let Some(client) = engine.client.as_ref() else {
                return json_string(&mut env, &failure("not signed in"));
            };
            match tele::list_owned_channels(client) {
                Ok(channels) => {
                    let list: Vec<Value> = channels.iter().map(channel_json).collect();
                    json!({ "ok": true, "drives": list })
                }
                Err(message) => failure(&message),
            }
        }
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// One channel, as the phone reads it.
fn channel_json(channel: &OwnedChannel) -> Value {
    json!({
        "id": channel.id,
        "accessHash": channel.access_hash,
        "title": channel.title,
        "created": channel.created,
        "hasActivity": channel.has_activity,
    })
}

/// Open a drive and read it.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeOpenDrive<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    channel_id: jint,
    access_hash: jint,
) -> jstring {
    let result = match engine() {
        Ok(engine) => open_drive(engine, channel_id as i64, access_hash as i64),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Open the drive, and read it.
fn open_drive(engine: &mut Engine, channel_id: i64, access_hash: i64) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let peer_ref = tele::peer_ref(channel_id, access_hash);
    let resume = tdrive_rs::cache::load(channel_id);
    let (contents, state, newest) = match tele::load_drive(client, peer_ref, resume) {
        Ok(loaded) => loaded,
        Err(message) => return failure(&message),
    };
    let _ = tdrive_rs::cache::save(&tdrive_rs::cache::Snapshot {
        version: tdrive_rs::cache::VERSION,
        drive_id: channel_id,
        newest_message_id: newest,
        state,
    });
    let title = "TDrive".to_string();
    apply_contents(&mut engine.state, &title, contents);
    engine.drive = Some(OpenDrive {
        id: channel_id,
        title,
        peer_ref,
    });
    // The choice is remembered, so the next launch opens the same drive.
    let mut config = DriveConfig::load().unwrap_or_default();
    config.remember_personal(StoredDrive {
        id: channel_id,
        access_hash,
        title: engine.drive.as_ref().map(|d| d.title.clone()).unwrap_or_default(),
        kind: "personal".to_string(),
    });
    let _ = config.save();
    drive_json(engine)
}

/// Put the drive's contents into the state, the way the shell does.
fn apply_contents(state: &mut AppState, title: &str, contents: tele::DriveContents) {
    state.files_loading = false;
    let count = contents.files.len();
    state.files = contents.files;
    state.clear_selection();
    state.trash = contents
        .trash
        .into_iter()
        .map(|entry| TrashEntry {
            deleted_at: chrono::DateTime::from_timestamp(entry.deleted_at, 0)
                .unwrap_or_else(chrono::Utc::now),
            origin: title.to_string(),
            file: entry.file,
        })
        .collect();
    state.set_vault(contents.vault);
    state.touch();
    state.status = format!("{count} items in {title}");
}

/// The drive that is open, as the phone reads it.
fn drive_json(engine: &Engine) -> Value {
    let state = &engine.state;
    json!({
        "ok": true,
        "title": engine.drive.as_ref().map(|drive| drive.title.clone()).unwrap_or_default(),
        "id": engine.drive.as_ref().map(|drive| drive.id).unwrap_or_default(),
        "items": state.files.len(),
        "size": state.total_size(),
        "encrypted": state.vault.is_some(),
        "unlocked": state.vault.as_ref().is_some_and(|vault| vault.key.is_some()),
        "hint": state.vault.as_ref().map(|vault| vault.hint.clone()).unwrap_or_default(),
        "trash": state.trash.len(),
    })
}

/// The drive that is open.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeDriveState<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = match engine() {
        Ok(engine) => drive_json(engine),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// The files of the drive, as the phone reads them.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeFilesJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = match engine() {
        Ok(engine) => {
            let files: Vec<Value> = engine.state.files.iter().map(file_json).collect();
            let trash = trash_json(engine);
            json!({ "ok": true, "files": files, "trash": trash })
        }
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// One file, as the phone reads it.
fn file_json(file: &RemoteFile) -> Value {
    json!({
        "messageId": file.message_id,
        "objectId": file.object_id,
        "name": file.name,
        "size": file.size,
        "kind": kind_name(file.kind),
        "date": file.date.map(|date| date.to_rfc3339()),
        "parent": file.parent,
        "encrypted": file.encrypted,
        "plaintextSize": file.plaintext_size,
        "revision": file.revision,
        "derivative": file
            .renditions
            .iter()
            .find(|entry| entry.is_thumbnail())
            .map(|entry| entry.message_id),
    })
}

/// The kind of a file, as the phone reads it.
fn kind_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Folder => "folder",
        FileKind::Document => "document",
        FileKind::Image => "image",
        FileKind::Video => "video",
        FileKind::Audio => "audio",
        FileKind::Sheet => "sheet",
        FileKind::Code => "code",
        FileKind::Archive => "archive",
        FileKind::Other => "other",
    }
}

/// The trash, as the phone reads it.
fn trash_json(engine: &Engine) -> Vec<Value> {
    engine
        .state
        .trash
        .iter()
        .map(|entry| {
            json!({
                "name": entry.file.name,
                "size": entry.file.size,
                "deletedAt": entry.deleted_at.to_rfc3339(),
                "origin": entry.origin,
                "objectId": entry.file.object_id,
            })
        })
        .collect()
}

// --- The vault --------------------------------------------------------------

/// Unlock the drive.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeUnlock<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    password: JString<'local>,
) -> jstring {
    let password = match read_string(&mut env, &password) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => {
            let Some(vault) = engine.state.vault.as_ref() else {
                return json_string(&mut env, &failure("this drive is not encrypted"));
            };
            match crypto::unlock(&vault.material, &password) {
                Ok(key) => {
                    if let Some(vault) = engine.state.vault.as_mut() {
                        vault.key = Some(key);
                    }
                    success()
                }
                Err(_) => failure("That encryption password is incorrect."),
            }
        }
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

// --- The files --------------------------------------------------------------

/// Download a file to a place on the phone.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeDownload<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    message_id: jint,
    dest: JString<'local>,
) -> jstring {
    let dest = match read_string(&mut env, &dest) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => download(engine, message_id as i32, &dest),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Fetch a file and write it where the phone asked.
fn download(engine: &mut Engine, message_id: i32, dest: &str) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(drive) = engine.drive.as_ref() else {
        return failure("no drive is open");
    };
    let Some(file) = engine.state.files.iter().find(|file| file.message_id == message_id) else {
        return failure("no such file");
    };
    let messages = content_messages(file);
    let key = engine.state.vault.as_ref().and_then(|vault| vault.key);
    let key = if file.encrypted { key } else { None };
    match tele::download_file(
        client,
        drive.peer_ref,
        &messages,
        Path::new(dest),
        key,
    ) {
        Ok(()) => success(),
        Err(message) => failure(&message),
    }
}

/// The messages a file's bytes live in.
fn content_messages(file: &RemoteFile) -> Vec<i32> {
    match &file.content {
        ContentRef::OpMessage => vec![file.message_id],
        ContentRef::Message(id) => vec![*id],
        ContentRef::Parts(ids) => ids.clone(),
    }
}

/// Upload a file from the phone.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeUpload<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    parent: JString<'local>,
    encrypt: jboolean,
) -> jstring {
    let path = match read_string(&mut env, &path) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let parent = match read_string(&mut env, &parent) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => upload(engine, &path, &parent, encrypt != 0),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Send a file to the drive.
fn upload(engine: &mut Engine, path: &str, parent: &str, encrypt: bool) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(drive) = engine.drive.as_ref() else {
        return failure("no drive is open");
    };
    let source = Path::new(path);
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload".to_string());
    let key = if encrypt {
        engine.state.vault.as_ref().and_then(|vault| vault.key)
    } else {
        None
    };
    if encrypt && key.is_none() {
        return failure("this drive is locked");
    }
    match tele::upload_file(client, drive.peer_ref, source, parent, &name, key) {
        Ok(_) => success(),
        Err(message) => failure(&message),
    }
}

/// Move a file to the trash, and put it back.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeTrash<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    object_id: JString<'local>,
    revision: jint,
    restore: jboolean,
) -> jstring {
    let object_id = match read_string(&mut env, &object_id) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => trash(engine, &object_id, revision as i64, restore != 0),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Move a file to the trash, and put it back.
fn trash(engine: &mut Engine, object_id: &str, revision: i64, restore: bool) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(drive) = engine.drive.as_ref() else {
        return failure("no drive is open");
    };
    let now = chrono::Utc::now().timestamp();
    let op = if restore {
        ops::untrash_op(object_id, revision, now)
    } else {
        ops::trash_op(object_id, revision, now, tdrive_rs::models::PURGE_WINDOW_DAYS * 86_400)
    };
    match tele::send_op(client, drive.peer_ref, &op) {
        Ok(_) => success(),
        Err(message) => failure(&message),
    }
}

/// Create a folder.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeMkdir<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    parent: JString<'local>,
    name: JString<'local>,
) -> jstring {
    let parent = match read_string(&mut env, &parent) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let name = match read_string(&mut env, &name) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = match engine() {
        Ok(engine) => mkdir(engine, &parent, &name),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Create a folder.
fn mkdir(engine: &mut Engine, parent: &str, name: &str) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(drive) = engine.drive.as_ref() else {
        return failure("no drive is open");
    };
    let object_id = ops::new_folder_id();
    let op = ops::mkdir_op(&object_id, parent, name);
    match tele::send_op(client, drive.peer_ref, &op) {
        Ok(_) => success(),
        Err(message) => failure(&message),
    }
}

/// The picture published beside a file, if it has one.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeDerivative<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    message_id: jint,
) -> jstring {
    let result = match engine() {
        Ok(engine) => derivative(engine, message_id as i32),
        Err(message) => failure(&message),
    };
    json_string(&mut env, &result)
}

/// Fetch the small picture published beside a file.
fn derivative(engine: &mut Engine, message_id: i32) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let Some(drive) = engine.drive.as_ref() else {
        return failure("no drive is open");
    };
    let Some(file) = engine.state.files.iter().find(|file| file.message_id == message_id) else {
        return failure("no such file");
    };
    let Some(entry) = file.renditions.iter().find(|entry| entry.is_thumbnail()) else {
        return failure("this file has no picture beside it");
    };
    let key = engine.state.vault.as_ref().and_then(|vault| vault.key);
    if file.encrypted && key.is_none() {
        return failure("this drive is locked");
    }
    let bytes = match tele::download_rendition(client, drive.peer_ref, entry.message_id) {
        Ok(bytes) => bytes,
        Err(message) => return failure(&message),
    };
    let picture = if file.encrypted {
        let Some(key) = key else {
            return failure("this drive is locked");
        };
        match rendition::decrypt(&bytes, &key, &entry.descriptor) {
            Ok(picture) => picture,
            Err(message) => return failure(&message),
        }
    } else {
        bytes
    };
    // The picture travels as a byte array, which the phone turns into a
    // bitmap; it is far too small to be worth a file.
    json!({
        "ok": true,
        "bytes": picture.iter().map(|byte| *byte as u64).collect::<Vec<u64>>(),
    })
}

// --- The appearance ---------------------------------------------------------

/// A colour as the phone reads it: the number the palette holds, which is
/// `0xRRGGBB`, or `0xRRGGBBAA` when the token is translucent.
fn colour(value: u32) -> u64 {
    u64::from(value)
}

/// The theme catalogue, as the phone reads it.
#[no_mangle]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeThemes<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let list: Vec<Value> = themes::THEMES
        .iter()
        .map(|theme| {
            // The whole palette travels, not only the four colours the
            // original's card shows: the phone draws with the same tokens the
            // desktop shell does.
            let palette = themes::palette_of(theme.id);
            let mut tokens = serde_json::Map::new();
            if let Some(palette) = palette {
                tokens.insert("canvas".into(), colour(palette.canvas));
                tokens.insert("surface0".into(), colour(palette.surface_0));
                tokens.insert("surface1".into(), colour(palette.surface_1));
                tokens.insert("surface2".into(), colour(palette.surface_2));
                tokens.insert("surface3".into(), colour(palette.surface_3));
                tokens.insert("text".into(), colour(palette.text));
                tokens.insert("textSoft".into(), colour(palette.text_soft));
                tokens.insert("textMuted".into(), colour(palette.text_muted));
                tokens.insert("textSubtle".into(), colour(palette.text_subtle));
                tokens.insert("border".into(), colour(palette.border));
                tokens.insert("borderSoft".into(), colour(palette.border_soft));
                tokens.insert("accent".into(), colour(palette.accent));
                tokens.insert("accentHover".into(), colour(palette.accent_hover));
                tokens.insert("onAccent".into(), colour(palette.on_accent));
                tokens.insert("danger".into(), colour(palette.danger));
                tokens.insert("warning".into(), colour(palette.warning));
                tokens.insert("success".into(), colour(palette.success));
            }
            json!({
                "id": theme.id,
                "name": theme.name,
                "light": theme.appearance == themes::Appearance::Light,
                "preview": theme
                    .preview
                    .iter()
                    .map(|value| colour(*value))
                    .collect::<Vec<u64>>(),
                "palette": Value::Object(tokens),
            })
        })
        .collect();
    let result = json!({ "ok": true, "themes": list, "active": themes::active_id() });
    json_string(&mut env, &result)
}
