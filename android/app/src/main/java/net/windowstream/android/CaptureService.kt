package net.windowstream.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * Foreground service that owns the MediaProjection.
 *
 * Android requires the consent result to be consumed by a foreground
 * service of type `mediaProjection`:
 *  - the service must call startForeground() (with the mediaProjection
 *    type on API 29+) before / while obtaining the projection,
 *  - on Android 14+ the MediaProjection.Callback must be registered
 *    before createVirtualDisplay() is ever called, and the consent
 *    Intent may be used only once.
 *
 * This diagnostics build obtains and holds the projection only; it
 * does not yet create a VirtualDisplay or encode. That keeps the
 * permission flow verifiable in isolation. See
 * `android/docs/PERMISSIONS.md` for the full constraint list.
 */
class CaptureService : Service() {

    private var mediaProjection: MediaProjection? = null

    private val projectionCallback = object : MediaProjection.Callback() {
        override fun onStop() {
            Log.i(TAG, "MediaProjection stopped by system/user")
            stopSelf()
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopCapture()
            stopSelf()
            return START_NOT_STICKY
        }

        val resultCode = intent?.getIntExtra(EXTRA_RESULT_CODE, Int.MIN_VALUE) ?: Int.MIN_VALUE
        @Suppress("DEPRECATION")
        val data: Intent? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent?.getParcelableExtra(EXTRA_RESULT_DATA, Intent::class.java)
        } else {
            intent?.getParcelableExtra(EXTRA_RESULT_DATA)
        }

        if (resultCode == Int.MIN_VALUE || data == null) {
            Log.e(TAG, "Started without a MediaProjection consent result; stopping")
            stopSelf()
            return START_NOT_STICKY
        }

        startForegroundWithNotification()

        val manager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        try {
            val projection = manager.getMediaProjection(resultCode, data)
            // Required on Android 14+ before any createVirtualDisplay call.
            projection.registerCallback(projectionCallback, null)
            mediaProjection = projection
            isRunning = true
            Log.i(TAG, "MediaProjection obtained and held by foreground service")
        } catch (t: Throwable) {
            Log.e(TAG, "getMediaProjection failed", t)
            stopSelf()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    override fun onDestroy() {
        stopCapture()
        super.onDestroy()
    }

    private fun stopCapture() {
        try {
            mediaProjection?.unregisterCallback(projectionCallback)
        } catch (t: Throwable) {
            Log.w(TAG, "unregisterCallback failed", t)
        }
        try {
            mediaProjection?.stop()
        } catch (t: Throwable) {
            Log.w(TAG, "MediaProjection.stop failed", t)
        }
        mediaProjection = null
        isRunning = false
    }

    private fun startForegroundWithNotification() {
        val channelId = "capture"
        val nm = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                channelId,
                getString(R.string.capture_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ).apply { description = getString(R.string.capture_channel_desc) }
            nm.createNotificationChannel(channel)
        }
        val notification: Notification = Notification.Builder(this, channelId)
            .setContentTitle(getString(R.string.capture_notification_title))
            .setContentText(getString(R.string.capture_notification_text))
            .setSmallIcon(android.R.drawable.presence_video_online)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    companion object {
        private const val TAG = "CaptureService"
        private const val NOTIFICATION_ID = 1
        const val ACTION_START = "net.windowstream.android.action.START_CAPTURE"
        const val ACTION_STOP = "net.windowstream.android.action.STOP_CAPTURE"
        const val EXTRA_RESULT_CODE = "extra_result_code"
        const val EXTRA_RESULT_DATA = "extra_result_data"

        @Volatile
        var isRunning: Boolean = false
            private set

        fun startIntent(context: Context, resultCode: Int, data: Intent): Intent {
            return Intent(context, CaptureService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_RESULT_CODE, resultCode)
                putExtra(EXTRA_RESULT_DATA, data)
            }
        }

        fun stopIntent(context: Context): Intent {
            return Intent(context, CaptureService::class.java).apply { action = ACTION_STOP }
        }
    }
}
