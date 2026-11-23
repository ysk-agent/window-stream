plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "net.windowstream.viewer"
    compileSdk = 34

    defaultConfig {
        // Separate package from the host app (net.windowstream.android):
        // the viewer needs no capture/accessibility permissions and no
        // foreground service, so it ships as its own app.
        applicationId = "net.windowstream.viewer"
        minSdk = 29
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
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

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }
}

dependencies {
    implementation("org.jetbrains.kotlin:kotlin-stdlib:2.0.21")
    // GameActivity: the native-activity glue the Rust `android_main`
    // entry point (winit android-game-activity backend) plugs into.
    implementation("androidx.games:games-activity:2.0.2")
}
