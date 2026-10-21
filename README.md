# TDrive for Android

The phone application, in Kotlin and Jetpack Compose, over the same engine the
desktop application runs.

There is no web view anywhere in it. The screen is drawn by Compose, the files
are read by Rust, and the two speak over JNI.

## The shape of it

| Piece | What it is |
| --- | --- |
| `engine/` | The Rust bridge. It links the engine from [`Coobyk/tdrive-rs`](https://github.com/Coobyk/tdrive-rs) — the drive's format, its Telegram transport and its cryptography — and answers the phone with JSON documents. |
| `app/` | The Gradle project: the Compose application, and the task that builds the engine for the phone's architectures. |

Because the engine is shared, the phone and the desktop are the same
application: the same operations, the same encryption, and the same small
pictures published beside a file, so a gallery costs a few kilobytes on either.

## Building it

The build needs a JDK 17 or newer, the Android SDK (platform 35 and the build
tools), and the NDK (r27). The engine is built by cargo through
[cargo-ndk](https://github.com/cross-rs/cargo-ndk), for `arm64-v8a`,
`armeabi-v7a` and `x86_64`.

```bash
cd app
ANDROID_HOME=/path/to/android-sdk ./gradlew assembleDebug
```

The APK lands in `app/app/build/outputs/apk/debug/`. The engine's libraries
are in the application itself: there is nothing to install beside it, and
nothing is loaded at run time.

## What it does

- Sign in to Telegram, with the code and the two-factor password when the
  account has one.
- Choose the drive — a broadcast channel this account created — and open it.
- Browse the folders, with the drive's own trash beside them.
- Download a file, or send one to the drive.
- Unlock an encrypted drive, and see the pictures the drive publishes beside
  its files.
- Change the appearance: the same seventeen palettes the desktop has, from the
  same catalogue.
