import org.jetbrains.kotlin.gradle.dsl.JvmTarget
import org.jetbrains.kotlin.gradle.tasks.KotlinCompile

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

// ---- Rust core integration -------------------------------------------------
// Two Exec tasks run before every build:
//  1. cargo-ndk cross-compiles waypoint-ffi into build/rustJniLibs/<abi>/,
//     which is added as a jniLibs source dir.
//  2. The project-local uniffi-bindgen reads the metadata embedded in a host
//     build of the same crate and writes Kotlin into build/generated/uniffi,
//     which is added as a source dir. Nothing generated is committed.
val rustDir: File = rootProject.projectDir.resolve("../core").canonicalFile
val rustJniLibsDir: File = layout.buildDirectory.dir("rustJniLibs").get().asFile
val uniffiOutDir: File = layout.buildDirectory.dir("generated/uniffi/kotlin").get().asFile
val rustAbis: List<String> =
    (findProperty("waypoint.abis") as String? ?: "arm64-v8a,x86_64")
        .split(',').map { it.trim() }.filter { it.isNotEmpty() }
val hostLibName: String = System.getProperty("os.name").lowercase().let {
    when {
        it.contains("mac") -> "libwaypoint_ffi.dylib"
        it.contains("win") -> "waypoint_ffi.dll"
        else -> "libwaypoint_ffi.so"
    }
}
val rustSources = fileTree(rustDir) {
    include("**/*.rs", "**/Cargo.toml", "Cargo.lock", "**/uniffi.toml")
    exclude("target/**")
}

android {
    namespace = "io.github.pgallo.waypoint"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.pgallo.waypoint"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += rustAbis }
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

    buildFeatures { compose = true }

    sourceSets {
        getByName("main") {
            java.srcDir(uniffiOutDir)
            jniLibs.srcDir(rustJniLibsDir)
        }
    }
}

tasks.withType<KotlinCompile>().configureEach {
    compilerOptions.jvmTarget.set(JvmTarget.JVM_17)
}

val cargoNdkBuild by tasks.registering(Exec::class) {
    group = "rust"
    description = "Cross-compiles waypoint-ffi for ${rustAbis.joinToString()} with cargo-ndk."
    workingDir = rustDir
    val cmd = mutableListOf("cargo", "ndk", "--platform", "26")
    rustAbis.forEach { cmd += listOf("-t", it) }
    cmd += listOf("-o", rustJniLibsDir.absolutePath, "build", "--release", "-p", "waypoint-ffi")
    commandLine(cmd)
    inputs.files(rustSources)
    inputs.property("abis", rustAbis)
    outputs.dir(rustJniLibsDir)
}

val cargoHostBuild by tasks.registering(Exec::class) {
    group = "rust"
    description = "Builds waypoint-ffi for the host so uniffi-bindgen can read its metadata."
    workingDir = rustDir
    // Debug profile: bindgen only reads the embedded metadata, so the host
    // build does not need LTO.
    commandLine("cargo", "build", "-p", "waypoint-ffi", "-p", "uniffi-bindgen")
    inputs.files(rustSources)
    outputs.file(rustDir.resolve("target/debug/$hostLibName"))
}

val generateUniffiBindings by tasks.registering(Exec::class) {
    group = "rust"
    description = "Generates Kotlin bindings for waypoint-ffi into the build directory."
    dependsOn(cargoHostBuild)
    workingDir = rustDir
    commandLine(
        rustDir.resolve("target/debug/uniffi-bindgen").absolutePath,
        "generate",
        "--library", rustDir.resolve("target/debug/$hostLibName").absolutePath,
        "--language", "kotlin",
        "--no-format",
        "--out-dir", uniffiOutDir.absolutePath,
    )
    inputs.files(rustSources)
    outputs.dir(uniffiOutDir)
    doFirst { uniffiOutDir.deleteRecursively() }
}

tasks.named("preBuild") {
    dependsOn(generateUniffiBindings, cargoNdkBuild)
}
// Consumers of the generated directories also depend on the producers
// directly, so Gradle's implicit-dependency validation is satisfied even if
// a task does not reach preBuild.
tasks.withType<KotlinCompile>().configureEach { dependsOn(generateUniffiBindings) }
tasks.withType<JavaCompile>().configureEach { dependsOn(generateUniffiBindings) }
tasks.matching { it.name.startsWith("merge") && it.name.endsWith("JniLibFolders") }
    .configureEach { dependsOn(cargoNdkBuild) }

dependencies {
    // The bindings call into the native library through JNA; the AAR ships
    // libjnidispatch.so for every Android ABI.
    implementation("net.java.dev.jna:jna:${libs.versions.jna.get()}@aar")

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.annotation)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.kotlinx.coroutines.android)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    debugImplementation(libs.androidx.compose.ui.tooling)
}
