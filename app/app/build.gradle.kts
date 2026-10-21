import com.android.build.api.tasks.Exec

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "tdrive.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "tdrive.app"
        // The phone app is the same application as the desktop one, so it
        // asks for the same permissions.
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        // The engine is built for the phone's own architectures, and for the
        // emulator's.
        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.exifinterface:exifinterface:1.3.7")
}

// --- The engine --------------------------------------------------------------

/** The Rust target each phone architecture is built for. */
val engineTargets = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "armeabi-v7a" to "armv7-linux-androideabi",
    "x86_64" to "x86_64-linux-android",
)

/**
 * The native library, built by cargo through cargo-ndk.
 *
 * The engine is a Rust crate compiled for each of the phone's architectures
 * and dropped into the application's library folder, where the packager picks
 * it up. It is inside the application's own .so: there is no separate library
 * to load, and no web view anywhere in the application.
 */
val buildEngine by tasks.registering {
    group = "build"
    description = "Builds the Rust engine for the phone's architectures."

    val engineDir = file(providers.gradleProperty("tdrive.engine.directory").get())
    val outputDir = file("src/main/jniLibs")
    val ndk = System.getenv("ANDROID_NDK_HOME")
        ?: System.getenv("ANDROID_NDK_ROOT")
        ?: "${System.getenv("ANDROID_HOME") ?: "/opt/android"}/ndk/27.2.12479018"

    inputs.dir(File(engineDir, "src"))
    inputs.file(File(engineDir, "Cargo.toml"))
    inputs.file(File(engineDir, "Cargo.lock"))
    outputs.dir(outputDir)

    doLast {
        require(File(ndk).isDirectory) {
            "The Android NDK is not where it was expected: $ndk"
        }
        engineTargets.forEach { (abi, target) ->
            project.exec {
                workingDir = engineDir
                commandLine(
                    "cargo", "ndk",
                    "-t", ndk,
                    "-o", outputDir.absolutePath,
                    "build", "--release",
                    "-p", "tdrive-engine",
                    "--target", target,
                )
                environment("CARGO_NDK_TARGET_TOOLCHAIN", "llvm")
                environment("ANDROID_NDK_HOME", ndk)
            }
        }
    }
}

tasks.named("preBuild") { dependsOn(buildEngine) }
