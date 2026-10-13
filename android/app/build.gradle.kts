import java.util.Properties

// AGP 9 applies the Kotlin plugin itself, so only the Compose compiler plugin
// is listed here.
plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

// Release signing comes from a local keystore.properties, which is not
// committed. Without it the release build is left unsigned.
val keystoreProperties = Properties().apply {
    val file = rootProject.file("keystore.properties")
    if (file.exists()) file.inputStream().use { load(it) }
}

/** Lowest Android version supported: playback capture and low-latency Wi-Fi need 10. */
val minApi = 29

/**
 * ABIs to build the native engine for, as `abi to Rust target`. Override with
 * `-PrustAbis=arm64-v8a` to build fewer, or add `x86_64` for an emulator
 * (after `rustup target add x86_64-linux-android`).
 */
val rustTargets = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "armeabi-v7a" to "armv7-linux-androideabi",
    "x86" to "i686-linux-android",
    "x86_64" to "x86_64-linux-android",
)
val rustAbis = ((findProperty("rustAbis") as String?) ?: "arm64-v8a,armeabi-v7a")
    .split(',').map { it.trim() }.filter { it.isNotEmpty() }

android {
    namespace = "dev.ccptr.soundbridge"
    compileSdk = 37
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "dev.ccptr.soundbridge"
        minSdk = minApi
        targetSdk = 37
        versionCode = 2
        versionName = "0.2.0"
        ndk { abiFilters += rustAbis }
    }

    signingConfigs {
        if (keystoreProperties.containsKey("storeFile")) {
            create("release") {
                storeFile = rootProject.file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.findByName("release")
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
        debug {
            // A different id so a test build sits alongside a release install.
            applicationIdSuffix = ".test"
        }
    }

    buildFeatures {
        compose = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // The engine is built by cargo (below) into this directory.
    sourceSets["main"].jniLibs.directories.add(layout.buildDirectory.dir("rustJniLibs").get().asFile.path)
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
    }
}

// ---- native engine ---------------------------------------------------------
//
// crates/android is compiled once per ABI with the NDK's clang as the linker.
// It is always built optimized: an unoptimized audio path glitches, and a
// debug APK that glitches tells you nothing.

val workspaceDir = rootProject.projectDir.parentFile
val ndkBin = androidComponents.sdkComponents.ndkDirectory.map {
    val host = when {
        org.gradle.internal.os.OperatingSystem.current().isMacOsX -> "darwin-x86_64"
        org.gradle.internal.os.OperatingSystem.current().isWindows -> "windows-x86_64"
        else -> "linux-x86_64"
    }
    it.asFile.resolve("toolchains/llvm/prebuilt/$host/bin")
}

val cargoTasks = rustAbis.map { abi ->
    val target = rustTargets[abi] ?: error("Unknown ABI '$abi'; known: ${rustTargets.keys}")
    val outDir = layout.buildDirectory.dir("rustJniLibs/$abi")
    tasks.register<Exec>("cargoBuild${abi.replace("-", "").replaceFirstChar(Char::uppercase)}") {
        group = "build"
        description = "Builds the SoundBridge engine for $abi"
        workingDir = workspaceDir
        // cargo does its own up-to-date checking, and far better than a
        // file-tree snapshot of the workspace would.
        outputs.upToDateWhen { false }

        val clangPrefix = if (target.startsWith("armv7")) "armv7a-linux-androideabi" else target
        val envName = target.uppercase().replace('-', '_')
        doFirst {
            val bin = ndkBin.get()
            val suffix = if (org.gradle.internal.os.OperatingSystem.current().isWindows) ".cmd" else ""
            environment("CARGO_TARGET_${envName}_LINKER", bin.resolve("$clangPrefix$minApi-clang$suffix").path)
            environment("CC_${target.replace('-', '_')}", bin.resolve("$clangPrefix$minApi-clang$suffix").path)
            environment("AR_${target.replace('-', '_')}", bin.resolve("llvm-ar").path)
            // Android 15 devices may use 16 KB pages; NDK r27 still links for
            // 4 KB unless told otherwise, and such a library fails to load.
            environment("CARGO_TARGET_${envName}_RUSTFLAGS", "-C link-arg=-Wl,-z,max-page-size=16384")
        }
        commandLine("cargo", "build", "--release", "-p", "soundbridge-android", "--target", target)
        doLast {
            copy {
                from(workspaceDir.resolve("target/$target/release/libsoundbridge_android.so"))
                into(outDir)
            }
        }
    }
}

tasks.named("preBuild") { dependsOn(cargoTasks) }

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    debugImplementation(libs.androidx.compose.ui.tooling)
}
