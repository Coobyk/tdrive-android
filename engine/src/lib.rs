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
use std::sync::Mutex;

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring};
use jni::JNIEnv;
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

/// The engine, for as long as the call needs it.
///
/// There is exactly one engine, and the phone's calls are short, so it lives
/// behind a lock rather than in a place that could be reached twice.
fn with_engine<T>(block: impl FnOnce(&mut Engine) -> T) -> Option<T> {
    static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
    let mut guard = ENGINE.lock().ok()?;
    if guard.is_none() {
        *guard = Some(Engine::new());
    }
    guard.as_mut().map(block)
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
    let value = env
        .get_string(value)
        .map_err(|_| "the string could not be read".to_string())?;
    Ok(value.into())
}

/// The error the phone is told about.
fn failure(message: &str) -> Value {
    json!({ "ok": false, "error": message })
}

/// Run a call, and turn a panic into an error the phone can show.
///
/// The engine is the application's own code, so a panic in it is a bug rather
/// than a reason to take the phone down with it: the phone is told what
/// happened, and carries on.
fn guarded(block: impl FnOnce() -> Value) -> Value {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(block)) {
        Ok(value) => value,
        Err(_) => failure("the engine stopped unexpectedly"),
    }
}

/// The result the phone is told about.
fn success() -> Value {
    json!({ "ok": true })
}


/// The engine's own calls, without the JNI around them.
///
/// The phone's bridge is a thin layer over these: the same answers and the
/// same documents, so the calls can be walked without a phone in the room.
pub mod bridge {
    use super::*;

    /// Set the engine up.
    pub fn init() -> Value {
        with_engine(|_| success()).unwrap_or_else(|| failure("the engine is not there"))
    }

    /// The credentials the application has stored, if it has any.
    ///
    /// The phone asks for them the first time, and remembers them after that;
    /// this is how the sign-in screen comes up filled in.
    pub fn credentials() -> Value {
        match tdrive_rs::config::load_credentials() {
            Some(credentials) => json!({
                "ok": true,
                "apiId": credentials.api_id,
                "apiHash": credentials.api_hash,
            }),
            None => json!({ "ok": true, "apiId": 0, "apiHash": "" }),
        }
    }

    /// Whether the account is signed in.
    pub fn session_state() -> String {
        with_engine(|engine| {
            if engine.client.is_some() {
                "ready".to_string()
            } else {
                "none".to_string()
            }
        })
        .unwrap_or_else(|| "none".to_string())
    }

    /// Sign in, and ask for the code.
    pub fn sign_in(api_id: i32, api_hash: &str, phone: &str) -> Value {
        with_engine(|engine| super::sign_in(engine, api_id, api_hash, phone))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Check the code the account was sent.
    pub fn verify_code(code: &str) -> Value {
        with_engine(|engine| super::verify_code(engine, code))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Check the password the account has.
    pub fn verify_password(password: &str) -> Value {
        with_engine(|engine| super::verify_password(engine, password))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Forget the account.
    pub fn sign_out() -> Value {
        with_engine(|engine| {
            engine.client = None;
            engine.drive = None;
            success()
        })
        .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// The channels this account may use as a drive.
    pub fn list_drives() -> Value {
        with_engine(|engine| {
            let Some(client) = engine.client.as_ref() else {
                return failure("not signed in");
            };
            match tele::list_owned_channels(client) {
                Ok(channels) => {
                    let list: Vec<Value> = channels.iter().map(super::channel_json).collect();
                    json!({ "ok": true, "drives": list })
                }
                Err(message) => failure(&message),
            }
        })
        .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Open a drive and read it.
    pub fn open_drive(channel_id: i64, access_hash: i64) -> Value {
        with_engine(|engine| super::open_drive(engine, channel_id, access_hash))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// The drive that is open.
    pub fn drive_state() -> Value {
        with_engine(|engine| super::drive_json(engine))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// The files of the drive, and its trash.
    pub fn files_json() -> Value {
        with_engine(|engine| {
            let files: Vec<Value> = engine.state.files.iter().map(super::file_json).collect();
            let trash = super::trash_json(engine);
            json!({ "ok": true, "files": files, "trash": trash })
        })
        .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Unlock the drive.
    pub fn unlock(password: &str) -> Value {
        with_engine(|engine| super::unlock(engine, password))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Fetch a file and write it where the phone asked.
    pub fn download(message_id: i32, dest: &str) -> Value {
        with_engine(|engine| super::download(engine, message_id, dest))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Send a file to the drive.
    pub fn upload(path: &str, parent: &str, encrypt: bool) -> Value {
        with_engine(|engine| super::upload(engine, path, parent, encrypt))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Move a file to the trash, and put it back.
    pub fn trash(object_id: &str, revision: i64, restore: bool) -> Value {
        with_engine(|engine| super::trash(engine, object_id, revision, restore))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// Create a folder.
    pub fn mkdir(parent: &str, name: &str) -> Value {
        with_engine(|engine| super::mkdir(engine, parent, name))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// The small picture published beside a file.
    pub fn derivative(message_id: i32) -> Value {
        with_engine(|engine| super::derivative(engine, message_id))
            .unwrap_or_else(|| failure("the engine is not there"))
    }

    /// The theme catalogue.
    pub fn themes_json() -> Value {
        super::themes_json()
    }
}

// --- The session ------------------------------------------------------------

/// Set the engine up.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeInit<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    data_dir: JString<'local>,
) -> jstring {
    let data_dir = match read_string(&mut env, &data_dir) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    // The application says where its files live: a phone has one place it may
    // write to and no platform place of its own, and a relative path is one
    // the phone cannot open.
    let result = guarded(|| {
        tdrive_rs::config::set_data_dir(data_dir.clone());
        if let Err(message) = tdrive_rs::config::ensure_data_dir() {
            return failure(&message);
        }
        bridge::init()
    });
    json_string(&mut env, &result)
}

/// The credentials the application has stored, if it has any.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeCredentials<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = guarded(|| bridge::credentials());
    json_string(&mut env, &result)
}

/// Whether the account is signed in.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeSessionState<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let state = bridge::session_state();
    string(&mut env, &state)
}

/// Sign in: the code is sent to the account's Telegram.
#[unsafe(no_mangle)]
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
    let result = guarded(|| bridge::sign_in(api_id as i32, &api_hash, &phone));
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeVerifyCode<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    code: JString<'local>,
) -> jstring {
    let code = match read_string(&mut env, &code) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = guarded(|| bridge::verify_code(&code));
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeVerifyPassword<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    password: JString<'local>,
) -> jstring {
    let password = match read_string(&mut env, &password) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = guarded(|| bridge::verify_password(&password));
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeSignOut<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = guarded(|| bridge::sign_out());
    json_string(&mut env, &result)
}

// --- The drive --------------------------------------------------------------

/// The channels this account may use as a drive.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeListDrives<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = guarded(|| bridge::list_drives());
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeOpenDrive<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    channel_id: jint,
    access_hash: jint,
) -> jstring {
    let result = guarded(|| bridge::open_drive(channel_id as i64, access_hash as i64));
    json_string(&mut env, &result)
}

/// Open the drive, and read it.
fn open_drive(engine: &mut Engine, channel_id: i64, access_hash: i64) -> Value {
    let Some(client) = engine.client.as_ref() else {
        return failure("not signed in");
    };
    let peer_ref = tele::peer_ref(channel_id, access_hash);
    // The snapshot is a cache, never the authority: it is only used to read
    // less of the drive's history, and the drive itself is what is read.
    let resume = tdrive_rs::cache::load(channel_id).map(|snapshot| {
        (snapshot.newest_message_id, snapshot.state)
    });
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
        title: title.clone(),
        peer_ref,
    });
    // The choice is remembered, so the next launch opens the same drive.
    let mut config = DriveConfig::load().unwrap_or_default();
    config.remember_personal(StoredDrive {
        id: channel_id,
        access_hash,
        title,
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeDriveState<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = guarded(|| bridge::drive_state());
    json_string(&mut env, &result)
}

/// The files of the drive, as the phone reads them.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeFilesJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = guarded(|| bridge::files_json());
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeUnlock<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    password: JString<'local>,
) -> jstring {
    let password = match read_string(&mut env, &password) {
        Ok(value) => value,
        Err(message) => return json_string(&mut env, &failure(&message)),
    };
    let result = guarded(|| bridge::unlock(&password));
    json_string(&mut env, &result)
}

/// Unlock the drive.
fn unlock(engine: &mut Engine, password: &str) -> Value {
    let Some(vault) = engine.state.vault.as_ref() else {
        return failure("this drive is not encrypted");
    };
    match crypto::unlock(&vault.material, password) {
        Ok(key) => {
            if let Some(vault) = engine.state.vault.as_mut() {
                vault.key = Some(key);
            }
            success()
        }
        Err(_) => failure("That encryption password is incorrect."),
    }
}

// --- The files --------------------------------------------------------------

/// Download a file to a place on the phone.
#[unsafe(no_mangle)]
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
    let result = guarded(|| bridge::download(message_id as i32, &dest));
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
    let Some(file) = engine
        .state
        .files
        .iter()
        .find(|file| file.message_id == message_id)
    else {
        return failure("no such file");
    };
    let messages = content_messages(file);
    let key = engine.state.vault.as_ref().and_then(|vault| vault.key);
    let key = if file.encrypted { key } else { None };
    match tele::download_file(client, drive.peer_ref, &messages, Path::new(dest), key) {
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
#[unsafe(no_mangle)]
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
    let result = guarded(|| bridge::upload(&path, &parent, encrypt != 0));
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
#[unsafe(no_mangle)]
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
    let result = guarded(|| bridge::trash(&object_id, revision as i64, restore != 0));
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
        ops::Op::Untrash {
            op_id: ops::deterministic_op_id("untrash", object_id, revision),
            obj: object_id.to_string(),
            parent: String::new(),
            name: String::new(),
            expected_revision: revision,
        }
    } else {
        ops::trash_op(
            object_id,
            revision,
            now,
            tdrive_rs::models::PURGE_WINDOW_DAYS * 86_400,
        )
    };
    match tele::send_op(client, drive.peer_ref, &op) {
        Ok(_) => success(),
        Err(message) => failure(&message),
    }
}

/// Create a folder.
#[unsafe(no_mangle)]
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
    let result = guarded(|| bridge::mkdir(&parent, &name));
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeDerivative<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    message_id: jint,
) -> jstring {
    let result = guarded(|| bridge::derivative(message_id as i32));
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
    let Some(file) = engine
        .state
        .files
        .iter()
        .find(|file| file.message_id == message_id)
    else {
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
fn themes_json() -> Value {
    let list: Vec<Value> = themes::THEMES
        .iter()
        .map(|theme| {
            // The whole palette travels, not only the four colours the
            // original's card shows: the phone draws with the same tokens the
            // desktop shell does.
            let mut tokens = serde_json::Map::new();
            if let Some(palette) = themes::palette_of(theme.id) {
                tokens.insert("canvas".into(), colour(palette.canvas).into());
                tokens.insert("surface0".into(), colour(palette.surface_0).into());
                tokens.insert("surface1".into(), colour(palette.surface_1).into());
                tokens.insert("surface2".into(), colour(palette.surface_2).into());
                tokens.insert("surface3".into(), colour(palette.surface_3).into());
                tokens.insert("text".into(), colour(palette.text).into());
                tokens.insert("textSoft".into(), colour(palette.text_soft).into());
                tokens.insert("textMuted".into(), colour(palette.text_muted).into());
                tokens.insert("textSubtle".into(), colour(palette.text_subtle).into());
                tokens.insert("border".into(), colour(palette.border).into());
                tokens.insert("borderSoft".into(), colour(palette.border_soft).into());
                tokens.insert("accent".into(), colour(palette.accent).into());
                tokens.insert("accentHover".into(), colour(palette.accent_hover).into());
                tokens.insert("onAccent".into(), colour(palette.on_accent).into());
                tokens.insert("danger".into(), colour(palette.danger).into());
                tokens.insert("warning".into(), colour(palette.warning).into());
                tokens.insert("success".into(), colour(palette.success).into());
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
    json!({ "ok": true, "themes": list, "active": themes::active_id() })
}

/// The theme catalogue, as the phone reads it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tdrive_app_NativeBridge_nativeThemes<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = guarded(|| bridge::themes_json());
    json_string(&mut env, &result)
}
