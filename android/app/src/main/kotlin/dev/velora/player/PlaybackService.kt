package dev.velora.player

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.drawable.Icon
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock

/**
 * Foreground "media playback" service: keeps the process alive while music plays and shows the
 * standard media notification (previous / play-pause / next, seek bar, cover) + lock-screen controls.
 * Audio itself is produced by Rust (rodio); this service only mirrors state and forwards button presses.
 */
class PlaybackService : Service() {

    companion object {
        const val ACTION_UPDATE = "dev.velora.UPDATE"
        const val ACTION_PLAY_PAUSE = "dev.velora.PLAY_PAUSE"
        const val ACTION_NEXT = "dev.velora.NEXT"
        const val ACTION_PREV = "dev.velora.PREV"
        const val ACTION_STOP = "dev.velora.STOP"
        private const val CHANNEL = "velora_playback"
        private const val NOTIF_ID = 7001

        @Volatile var instance: PlaybackService? = null
            private set
    }

    private lateinit var session: MediaSession
    private lateinit var nm: NotificationManager
    private lateinit var audio: AudioManager
    private val main = Handler(Looper.getMainLooper())
    private var inForeground = false
    private var focusRequest: AudioFocusRequest? = null
    private var resumeOnFocusGain = false
    private var coverPathShown: String? = null
    private var cover: Bitmap? = null

    private val noisyReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            // headphones unplugged / bluetooth disconnected -> pause like every player does
            if (intent?.action == AudioManager.ACTION_AUDIO_BECOMING_NOISY) MediaBridge.sendCommand(MediaBridge.CMD_PAUSE, 0)
        }
    }

    private val focusListener = AudioManager.OnAudioFocusChangeListener { change ->
        when (change) {
            AudioManager.AUDIOFOCUS_LOSS -> { resumeOnFocusGain = false; MediaBridge.sendCommand(MediaBridge.CMD_PAUSE, 0) }
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT -> { resumeOnFocusGain = MediaBridge.state.playing; MediaBridge.sendCommand(MediaBridge.CMD_PAUSE, 0) }
            AudioManager.AUDIOFOCUS_GAIN -> if (resumeOnFocusGain) { resumeOnFocusGain = false; MediaBridge.sendCommand(MediaBridge.CMD_PLAY, 0) }
        }
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        nm = getSystemService(NotificationManager::class.java)
        audio = getSystemService(AudioManager::class.java)
        val ch = NotificationChannel(CHANNEL, "Playback", NotificationManager.IMPORTANCE_LOW).apply {
            setShowBadge(false)
            lockscreenVisibility = Notification.VISIBILITY_PUBLIC
            description = "Music playback controls"
        }
        nm.createNotificationChannel(ch)

        session = MediaSession(this, "Velora").apply {
            setCallback(object : MediaSession.Callback() {
                override fun onPlay() = MediaBridge.sendCommand(MediaBridge.CMD_PLAY, 0)
                override fun onPause() = MediaBridge.sendCommand(MediaBridge.CMD_PAUSE, 0)
                override fun onSkipToNext() = MediaBridge.sendCommand(MediaBridge.CMD_NEXT, 0)
                override fun onSkipToPrevious() = MediaBridge.sendCommand(MediaBridge.CMD_PREV, 0)
                override fun onSeekTo(pos: Long) = MediaBridge.sendCommand(MediaBridge.CMD_SEEK, pos)
                override fun onStop() = MediaBridge.sendCommand(MediaBridge.CMD_STOP, 0)
            })
            isActive = true
        }
        registerReceiver(noisyReceiver, IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY))
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_PLAY_PAUSE -> MediaBridge.sendCommand(MediaBridge.CMD_TOGGLE, 0)
            ACTION_NEXT -> MediaBridge.sendCommand(MediaBridge.CMD_NEXT, 0)
            ACTION_PREV -> MediaBridge.sendCommand(MediaBridge.CMD_PREV, 0)
            ACTION_STOP -> { stopFromApp(); return START_NOT_STICKY }
        }
        render() // must call startForeground within 5 s of startForegroundService
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    fun renderOnMain() { main.post { render() } }

    fun stopFromApp() {
        main.post {
            abandonFocus()
            if (Build.VERSION.SDK_INT >= 24) stopForeground(STOP_FOREGROUND_REMOVE) else stopForeground(true)
            inForeground = false
            stopSelf()
        }
    }

    // ------------------------------------------------------------------------------------------
    private fun pi(action: String, code: Int): PendingIntent =
        PendingIntent.getService(
            this, code, Intent(this, PlaybackService::class.java).setAction(action),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

    private fun requestFocus() {
        if (Build.VERSION.SDK_INT < 26 || focusRequest != null) return
        val req = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA).setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
            .setOnAudioFocusChangeListener(focusListener)
            .build()
        focusRequest = req
        audio.requestAudioFocus(req)
    }

    private fun abandonFocus() {
        if (Build.VERSION.SDK_INT >= 26) focusRequest?.let { audio.abandonAudioFocusRequest(it) }
        focusRequest = null
    }

    private fun loadCover(path: String?): Bitmap? {
        if (path == null) { coverPathShown = null; cover = null; return null }
        if (path != coverPathShown) {
            coverPathShown = path
            cover = try { BitmapFactory.decodeFile(path) } catch (e: Exception) { null }
        }
        return cover
    }

    private fun render() {
        val st = MediaBridge.state
        val art = loadCover(st.coverPath)

        session.setMetadata(
            MediaMetadata.Builder()
                .putString(MediaMetadata.METADATA_KEY_TITLE, st.title)
                .putString(MediaMetadata.METADATA_KEY_ARTIST, st.artist)
                .putString(MediaMetadata.METADATA_KEY_ALBUM, st.album)
                .putLong(MediaMetadata.METADATA_KEY_DURATION, st.durationMs)
                .apply { if (art != null) putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, art) }
                .build(),
        )
        val actions = PlaybackState.ACTION_PLAY or PlaybackState.ACTION_PAUSE or PlaybackState.ACTION_PLAY_PAUSE or
            PlaybackState.ACTION_SKIP_TO_NEXT or PlaybackState.ACTION_SKIP_TO_PREVIOUS or PlaybackState.ACTION_SEEK_TO or PlaybackState.ACTION_STOP
        session.setPlaybackState(
            PlaybackState.Builder()
                .setActions(actions)
                .setState(
                    if (st.playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED,
                    st.positionMs, if (st.playing) 1f else 0f, SystemClock.elapsedRealtime(),
                )
                .build(),
        )
        if (st.playing) requestFocus()

        val open = PendingIntent.getActivity(
            this, 0, Intent(this, VeloraActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_music)
            .setContentTitle(st.title.ifEmpty { "Velora" })
            .setContentText(st.artist)
            .setSubText(st.album)
            .setLargeIcon(art)
            .setContentIntent(open)
            .setOngoing(st.playing)
            .setShowWhen(false)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .addAction(Notification.Action.Builder(Icon.createWithResource(this, R.drawable.ic_prev), "Previous", pi(ACTION_PREV, 1)).build())
            .addAction(
                Notification.Action.Builder(
                    Icon.createWithResource(this, if (st.playing) R.drawable.ic_pause else R.drawable.ic_play),
                    if (st.playing) "Pause" else "Play", pi(ACTION_PLAY_PAUSE, 2),
                ).build(),
            )
            .addAction(Notification.Action.Builder(Icon.createWithResource(this, R.drawable.ic_next), "Next", pi(ACTION_NEXT, 3)).build())
            .setStyle(Notification.MediaStyle().setMediaSession(session.sessionToken).setShowActionsInCompactView(0, 1, 2))
            .build()

        if (st.playing || !inForeground) {
            if (Build.VERSION.SDK_INT >= 29) startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
            else startForeground(NOTIF_ID, n)
            inForeground = true
        } else {
            // paused: leave foreground mode so the notification can be swiped away, but keep it visible
            if (Build.VERSION.SDK_INT >= 24) stopForeground(STOP_FOREGROUND_DETACH) else stopForeground(false)
            inForeground = false
            nm.notify(NOTIF_ID, n)
        }
    }

    override fun onDestroy() {
        instance = null
        try { unregisterReceiver(noisyReceiver) } catch (_: Exception) {}
        abandonFocus()
        session.isActive = false
        session.release()
        super.onDestroy()
    }
}
