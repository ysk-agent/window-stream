package net.windowstream.android

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.widget.Button
import android.widget.TextView

/**
 * Diagnostics shell.
 *
 * Responsibilities (Kotlin shell, by design):
 *  - run the MediaProjection consent flow,
 *  - start / stop the capture foreground service,
 *  - show MediaCodec VP9 diagnostics and the Rust core version.
 *
 * Everything protocol/transport related belongs in the Rust core
 * behind [NativeBridge]; this activity deliberately contains none.
 *
 * MediaProjection consent notes:
 *  - Consent is per-session: the system dialog must be shown and
 *    approved every time capture starts. It cannot be granted once
 *    persistently, pre-approved, or obtained silently.
 *  - The returned Intent is single-use. It is forwarded straight to
 *    [CaptureService] and never stored for reuse.
 */
class MainActivity : Activity() {

    private lateinit var nativeVersionText: TextView
    private lateinit var captureStatusText: TextView
    private lateinit var diagnosticsText: TextView

    private val projectionManager: MediaProjectionManager by lazy {
        getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        nativeVersionText = findViewById(R.id.nativeVersionText)
        captureStatusText = findViewById(R.id.captureStatusText)
        diagnosticsText = findViewById(R.id.diagnosticsText)

        findViewById<Button>(R.id.startCaptureButton).setOnClickListener { requestCaptureConsent() }
        findViewById<Button>(R.id.stopCaptureButton).setOnClickListener { stopCapture() }
        findViewById<Button>(R.id.refreshDiagnosticsButton).setOnClickListener { refreshDiagnostics() }

        nativeVersionText.text = "Rust core: ${NativeBridge.versionOrUnavailable()}"
        refreshCaptureStatus()
        refreshDiagnostics()
    }

    override fun onResume() {
        super.onResume()
        refreshCaptureStatus()
    }

    private fun requestCaptureConsent() {
        // createScreenCaptureIntent() shows the system consent dialog.
        // On Android 14+ the user can also choose single-app capture;
        // the result is still a whole-display-equivalent token for our
        // purposes and the choice is the user's, not ours.
        val intent = if (Build.VERSION.SDK_INT >= 34) {
            // MediaProjectionManager.createScreenCaptureIntent with
            // MediaProjectionConfig is available on API 34+; the plain
            // intent keeps behaviour identical across versions here.
            projectionManager.createScreenCaptureIntent()
        } else {
            projectionManager.createScreenCaptureIntent()
        }
        @Suppress("DEPRECATION")
        startActivityForResult(intent, REQUEST_CAPTURE)
    }

    @Deprecated("Activity result API kept intentionally: no AndroidX dependency in the shell")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_CAPTURE) return
        if (resultCode != RESULT_OK || data == null) {
            captureStatusText.text = "Capture: consent denied or cancelled"
            return
        }
        // Forward the single-use consent result to the foreground
        // service immediately. The service must be started as a
        // foreground service (startForegroundService) so it can call
        // startForeground() before consuming the projection.
        val serviceIntent = CaptureService.startIntent(this, resultCode, data)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(serviceIntent)
        } else {
            startService(serviceIntent)
        }
        captureStatusText.text = "Capture: starting (consent granted, service starting)"
    }

    private fun stopCapture() {
        startService(CaptureService.stopIntent(this))
        captureStatusText.text = "Capture: stopping"
    }

    private fun refreshCaptureStatus() {
        captureStatusText.text =
            if (CaptureService.isRunning) "Capture: running (MediaProjection held by service)"
            else "Capture: stopped"
    }

    private fun refreshDiagnostics() {
        diagnosticsText.text = try {
            CodecDiagnostics.collectReport()
        } catch (t: Throwable) {
            "Diagnostics failed: ${t.javaClass.simpleName}: ${t.message}"
        }
    }

    companion object {
        private const val REQUEST_CAPTURE = 1001
    }
}
