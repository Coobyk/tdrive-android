//! tdrive-android native core — the desktop worker's Telegram logic
//! (auth, drives, sync, transfers, vault) behind a thin JNI surface.
//! Kotlin owns all UI; the bridge speaks JSON commands in, JSON events
//! out, mirroring the desktop's Command/Event split.

mod crypto;
mod journal;
mod ops;
mod paths;
mod project;
mod remote;
mod tg;
mod wire;

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use std::path::PathBuf;
use std::sync::OnceLock;

static WORKER: OnceLock<tg::Handle> = OnceLock::new();

fn jstring(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn ok_json() -> String {
    "{\"ok\":true}".to_string()
}

fn err_json(e: impl std::fmt::Display) -> String {
    serde_json::json!({"ok": false, "err": e.to_string()}).to_string()
}

fn out(env: &mut JNIEnv, s: String) -> jni::sys::jstring {
    use jni::objects::JObject;
    env.new_string(s).expect("new_string").into_raw()
}

/// First call — hands the worker its data directory (app files dir).
#[no_mangle]
pub extern "system" fn Java_dev_tdrive_app_NativeCore_nativeInit(
    mut env: JNIEnv,
    _c: JClass,
    data_dir: JString,
) -> jni::sys::jstring {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("tdrive-core"),
    );
    let dir = jstring(&mut env, &data_dir);
    out(&mut env, match paths::init(PathBuf::from(dir)) {
        Ok(()) => ok_json(),
        Err(e) => err_json(e),
    })
}

/// Start the worker thread. Idempotent.
#[no_mangle]
pub extern "system" fn Java_dev_tdrive_app_NativeCore_nativeSpawn(
    mut env: JNIEnv,
    _c: JClass,
) -> jni::sys::jstring {
    WORKER.get_or_init(tg::spawn);
    out(&mut env, ok_json())
}

/// Dispatch a Command as JSON — `{"cmd":"Connect","args":{...}}`.
#[no_mangle]
pub extern "system" fn Java_dev_tdrive_app_NativeCore_nativeDispatch(
    mut env: JNIEnv,
    _c: JClass,
    json: JString,
) -> jni::sys::jstring {
    let Some(h) = WORKER.get() else {
        return out(&mut env, err_json("worker not running"));
    };
    let text = jstring(&mut env, &json);
    let res = serde_json::from_str::<tg::Command>(&text)
        .map_err(|e| anyhow::anyhow!("bad command {text}: {e}"))
        .and_then(|cmd| h.tx.send(cmd).map_err(|_| anyhow::anyhow!("worker gone")));
    out(&mut env, match res {
        Ok(()) => ok_json(),
        Err(e) => err_json(e),
    })
}

/// Non-blocking event fetch — Kotlin polls this from a coroutine.
/// Returns the Event JSON, "" when the queue is empty.
#[no_mangle]
pub extern "system" fn Java_dev_tdrive_app_NativeCore_nativePoll(
    mut env: JNIEnv,
    _c: JClass,
) -> jni::sys::jstring {
    let Some(h) = WORKER.get() else {
        return out(&mut env, err_json("worker not running"));
    };
    let ev = h.rx.try_recv().ok();
    match ev {
        Some(ev) => out(&mut env, serde_json::to_string(&ev).unwrap_or_else(|_| err_json("serialize"))),
        None => std::ptr::null_mut(),
    }
}
