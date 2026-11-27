import java.io.FileInputStream
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

val signingProps = Properties().apply {
    val homeCfg = file("${System.getProperty("user.home")}/.config/remote-control/android-signing.properties")
    if (homeCfg.isFile) {
        FileInputStream(homeCfg).use { load(it) }
    }
}

fun signingValue(envKey: String, propKey: String): String? =
    System.getenv(envKey)?.takeIf { it.isNotBlank() }
        ?: signingProps.getProperty(propKey)?.takeIf { it.isNotBlank() }

val releaseStorePath = signingValue("RC_ANDROID_KEYSTORE_PATH", "storeFile")
val releaseStorePass = signingValue("RC_ANDROID_KEYSTORE_PASSWORD", "storePassword")
val releaseKeyAlias = signingValue("RC_ANDROID_KEY_ALIAS", "keyAlias")
val releaseKeyPass = signingValue("RC_ANDROID_KEY_PASSWORD", "keyPassword")
val hasReleaseSigning = !releaseStorePath.isNullOrBlank() &&
    !releaseStorePass.isNullOrBlank() &&
    !releaseKeyAlias.isNullOrBlank() &&
    !releaseKeyPass.isNullOrBlank() &&
    file(releaseStorePath).isFile

val appVersionName = System.getenv("RC_BUILD_VERSION")
    ?.trim()
    ?.removePrefix("v")
    ?.takeIf { it.isNotEmpty() }
    ?: "0.1.0"

// 0.1.2 -> 1002, 1.4.20 -> 1004020: grows with the semver, so Android accepts each update.
val appVersionCode = appVersionName.substringBefore('-').split('.')
    .map { it.toIntOrNull() ?: 0 }
    .let { p -> p.getOrElse(0) { 0 } * 1_000_000 + p.getOrElse(1) { 0 } * 1_000 + p.getOrElse(2) { 0 } }
    .coerceAtLeast(1)

// Where the app looks for the shared release manifest `latest.json` (comma-separated, tried in
// order; the PC updater reads the same files). RC_UPDATE_URL overrides it for local tests.
val updateManifestUrls = System.getenv("RC_UPDATE_URL")?.trim()?.takeIf { it.isNotEmpty() }
    ?: listOf(
        "https://cnb.cool/peteroooooooo/remote-control/-/releases/download/latest/latest.json",
        "https://github.com/Peteroooooooo/remote-control/releases/latest/download/latest.json",
    ).joinToString(",")

android {
    namespace = "com.rc.android"
    compileSdk = 35

    base {
        archivesName.set("remote-control")
    }

    defaultConfig {
        applicationId = "com.rc.android"
        minSdk = 29
        targetSdk = 35
        versionCode = appVersionCode
        versionName = appVersionName
        buildConfigField("String", "UPDATE_MANIFEST_URLS", "\"$updateManifestUrls\"")

        ndk {
            abiFilters += listOf("arm64-v8a")
        }
        resourceConfigurations += listOf("zh", "en")
    }

    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = file(releaseStorePath!!)
                storePassword = releaseStorePass
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPass
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
        debug {
            isMinifyEnabled = false
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
        buildConfig = true
    }

    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
        resources {
            excludes += listOf("/META-INF/{AL2.0,LGPL2.1}", "DebugProbesKt.bin")
        }
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2025.02.00")
    implementation(composeBom)

    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.activity:activity-compose:1.10.0")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material:material-icons-extended")
    testImplementation("junit:junit:4.13.2")
}
