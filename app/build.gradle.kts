// The Android application.
//
// The native library is built by cargo, through cargo-ndk, and dropped into the
// application's own library folder: the engine is inside the application's
// .so, so nothing is loaded at run time and there is no web view anywhere in
// it.
plugins {
    id("com.android.application") version "8.7.3" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
}
