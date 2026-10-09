package dev.velora.player

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log

/**
 * Static API that Rust calls through JNI (see src/android.rs) and the native callback Rust exports.
 * Signatures here MUST match the strings in src/android.rs.
 */
object MediaBridge {
    const val REQ_AUDIO = 4101
    const val REQ_NOTIF = 4102

    // command codes understood by `nativeCommand` (src/android.rs)
    const val CMD_PLAY = 0
    const val CMD_PAUSE = 1
    const val CMD_TOGGLE = 2
    const val CMD_NEXT = 3
    const val CMD_PREV = 4
    const val CMD_SEEK = 5
    const val CMD_STOP = 6
    const val CMD_RESUME = 7
    const val CMD_PERMISSION_GRANTED = 8

    data class NowPlaying(
        val title: String = "",
        val artist: String = "",
        val album: String = "",
        val durationMs: Long = 0,
        val positionMs: Long = 0,
        val playing: Boolean = false,
        val coverPath: String? = null,
    )

    @Volatile private var activity: Activity? = null
    @Volatile var state: NowPlaying = NowPlaying()
        private set

    fun attach(a: Activity) {
        activity = a
    }

    private val ctx: Context? get() = activity?.applicationContext

    // ---- implemented by Rust (libvelora.so) --------------------------------------------------
    @JvmStatic external fun nativeCommand(cmd: Int, arg: Long)

    fun sendCommand(cmd: Int, arg: Long) {
        try {
            nativeCommand(cmd, arg)
        } catch (e: UnsatisfiedLinkError) {
            Log.w("Velora", "native library not ready yet: ${e.message}")
        }
    }

    // ---- called from Rust --------------------------------------------------------------------
    @JvmStatic
    fun hasAudioPermission(): Boolean {
        val c = ctx ?: return false
        val perm = if (Build.VERSION.SDK_INT >= 33) Manifest.permission.READ_MEDIA_AUDIO else Manifest.permission.READ_EXTERNAL_STORAGE
        return c.checkSelfPermission(perm) == PackageManager.PERMISSION_GRANTED
    }

    @JvmStatic
    fun requestAudioPermission() {
        val a = activity ?: return
        val perm = if (Build.VERSION.SDK_INT >= 33) Manifest.permission.READ_MEDIA_AUDIO else Manifest.permission.READ_EXTERNAL_STORAGE
        a.runOnUiThread { a.requestPermissions(arrayOf(perm), REQ_AUDIO) }
    }

    @JvmStatic
    fun requestNotificationPermission() {
        val a = activity ?: return
        if (Build.VERSION.SDK_INT >= 33 &&
            a.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            a.runOnUiThread { a.requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQ_NOTIF) }
        }
    }

    @JvmStatic
    fun updateNowPlaying(
        title: String, artist: String, album: String,
        durationMs: Long, positionMs: Long, playing: Boolean, coverPath: String?,
    ) {
        state = NowPlaying(title, artist, album, durationMs, positionMs, playing, coverPath)
        val svc = PlaybackService.instance
        if (svc != null) {
            svc.renderOnMain()
        } else {
            val c = ctx ?: return
            try {
                val i = Intent(c, PlaybackService::class.java).setAction(PlaybackService.ACTION_UPDATE)
                if (Build.VERSION.SDK_INT >= 26) c.startForegroundService(i) else c.startService(i)
            } catch (e: Exception) {
                Log.w("Velora", "cannot start playback service: ${e.message}")
            }
        }
    }

    @JvmStatic
    fun stopPlayback() {
        PlaybackService.instance?.stopFromApp()
    }

    /** [top, bottom] system bar insets in dp (the UI keeps clear of the status / gesture bars). */
    @JvmStatic
    fun insetsDp(): FloatArray {
        val a = activity ?: return floatArrayOf(0f, 0f)
        val d = a.resources.displayMetrics.density
        return try {
            val w = a.window.decorView.rootWindowInsets
            if (w == null) floatArrayOf(0f, 0f)
            else floatArrayOf(w.systemWindowInsetTop / d, w.systemWindowInsetBottom / d)
        } catch (e: Exception) {
            floatArrayOf(0f, 0f)
        }
    }
}
