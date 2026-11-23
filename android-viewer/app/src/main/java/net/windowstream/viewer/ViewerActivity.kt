package net.windowstream.viewer

import com.google.androidgamesdk.GameActivity

/**
 * The display surface. GameActivity's native glue loads
 * libwindowstream_viewer.so (the manifest's `android.app.lib_name`
 * meta-data) and calls its `android_main`, where the Rust viewer runs
 * the winit event loop and the shared Compositor renders the remote
 * windows. All logic lives on the Rust side; this class is only the
 * Android entry point.
 */
class ViewerActivity : GameActivity()
