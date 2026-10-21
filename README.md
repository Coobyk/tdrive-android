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

There is a [build on the releases page](https://github.com/Coobyk/tdrive-android/releases),
signed with the debug key and meant for sideloading.

## What has been walked, and what has not

The engine's calls are a module of their own, so they can be walked without a
phone: the test signs in with the session, lists the drives, opens one,
unlocks it, takes the small picture published beside a file and brings a file
down whole. That passes against a real drive.

The application itself has **not been run on a device yet** — the machine this
was built on has no Android emulator and no hardware acceleration, so the
build is verified by its contents: the engine's libraries are real ARM and
x86 Android binaries, the bridge's seventeen entry points are exported from
them, the manifest asks for the network and nothing else, and there is no web
view anywhere in it. What that does not cover is the screen itself.

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
