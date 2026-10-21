import org.gradle.api.tasks.Exec
import java.util.Base64

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// The version the phone shows.
//
// The number is what Android compares when an update is offered, so it has to
// grow with every build; the build passes the number it wants, and the run's
// own count is what the workflow hands it.
// The names are not the extension's own: inside the block below, the
// extension's property would shadow them, and the version would be set to
// nothing.
val buildVersionCode = (project.findProperty("tdrive.versionCode") as String?)?.toIntOrNull() ?: 1
val buildVersionName = (project.findProperty("tdrive.versionName") as String?) ?: "0.1.0"

android {
    namespace = "tdrive.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "tdrive.app"
        // The phone app is the same application as the desktop one, so it
        // asks for the same permissions.
        minSdk = 26
        targetSdk = 35
        versionCode = buildVersionCode
        versionName = buildVersionName

        // The engine is built for the phone's own architectures, and for the
        // emulator's.
        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
    }

    signingConfigs {
        // The application's own key, when the build has one.
        //
        // A key that changes with every build is a key that cannot be updated
        // over: Android refuses an update whose signature differs from the
        // installed one's, so the reader would have to uninstall and sign in
        // again for every build. The key lives in the build's secrets, and a
        // build without them falls back to the debug key.
        create("tdrive") {
            val encoded = System.getenv("TDrive_KEYSTORE")
            if (encoded != null) {
                val store = file("tdrive-release.keystore")
                store.writeBytes(Base64.getDecoder().decode(encoded))
                this.storeFile = store
                this.storePassword = System.getenv("TDrive_KEYSTORE_PASSWORD")
                this.keyAlias = System.getenv("TDrive_KEY_ALIAS")
                this.keyPassword = System.getenv("TDrive_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // The key the build has, or the debug key when it has none.
            signingConfig = if (System.getenv("TDrive_KEYSTORE") != null) {
                signingConfigs.getByName("tdrive")
            } else {
                signingConfigs.getByName("debug")
            }
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

/** The phone architecture, and the Rust target it is built for. */
val engineTargets = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "armeabi-v7a" to "armv7-linux-androideabi",
    "x86_64" to "x86_64-linux-android",
)

val engineDir = rootProject.file(providers.gradleProperty("tdrive.engine.directory").get())
val engineOutput = file("src/main/jniLibs")
val ndkHome = System.getenv("ANDROID_NDK_HOME")
    ?: System.getenv("ANDROID_NDK_ROOT")
    ?: "${System.getenv("ANDROID_HOME") ?: "/opt/android"}/ndk/27.2.12479018"

/**
 * The native library, built by cargo through cargo-ndk.
 *
 * The engine is a Rust crate compiled for each of the phone's architectures
 * and dropped into the application's library folder, where the packager picks
 * it up. It is inside the application's own .so: there is no separate library
 * to load, and no web view anywhere in the application.
 */
val buildEngine = engineTargets.map { (abi, target) ->
    tasks.register<Exec>("buildEngine${abi.replace("-", "")}") {
        group = "build"
        description = "Builds the Rust engine for $abi."

        workingDir = engineDir
        commandLine(
            "cargo", "ndk",
            "-t", abi,
            "-o", engineOutput.absolutePath,
            "build", "--release",
            "-p", "tdrive-engine",
            "--target", target,
        )
        environment("CARGO_NDK_TARGET_TOOLCHAIN", "llvm")
        environment("ANDROID_NDK_HOME", ndkHome)

        inputs.dir(File(engineDir, "src"))
        inputs.file(File(engineDir, "Cargo.toml"))
        inputs.file(File(engineDir, "Cargo.lock"))
        outputs.dir(engineOutput)
    }
}

tasks.named("preBuild") { dependsOn(buildEngine) }
