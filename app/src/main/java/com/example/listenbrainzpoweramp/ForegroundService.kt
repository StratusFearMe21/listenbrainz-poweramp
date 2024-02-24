package com.example.listenbrainzpoweramp

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.SharedPreferences
import android.net.Uri
import android.os.Bundle
import android.os.IBinder
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.Settings
import android.util.Log
import androidx.preference.PreferenceManager
import de.justjanne.bitflags.Flag
import de.justjanne.bitflags.Flags
import de.justjanne.bitflags.none
import de.justjanne.bitflags.toBits
import de.justjanne.bitflags.toEnumSet
import java.io.File
import java.io.FileInputStream
import java.io.InputStream

enum class MetadataReqFlag(
    override val value: Byte,
) : Flag<Byte> {
    Artist(1),
    Title(2),
    Album(4),
    ReleaseMBID(8),
    ArtistMBIDS(16),
    RecordingMBID(32);
    companion object : Flags<Byte, MetadataReqFlag> {
        override val all: Set<MetadataReqFlag> = values().toEnumSet()
    }
}

class ForegroundService : Service(), SharedPreferences.OnSharedPreferenceChangeListener {
    var mTrackIntent: Intent? = null
    var mStatusIntent: Intent? = null
    var errNotifyNum: Int = 1
    // var mPlayingModeIntent: Intent? = null
    private var isStarted: Boolean = false

    init {
        System.loadLibrary("lbp_native")
        initrs(this)
    }

    private external fun mTrackFunction(
        path: String,
        ext: String,
        dur: Int,
        pos: Int,
        metadataReqs: Byte
    )

    private external fun mStatusFunction(state: Int)

    private external fun initrs(self: ForegroundService)

    private external fun setToken(token: String)

    override fun onDestroy() {
        super.onDestroy()
        isStarted = false
        PreferenceManager.getDefaultSharedPreferences(this)
            .unregisterOnSharedPreferenceChangeListener(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!isStarted) {
            isStarted = true
            PreferenceManager.getDefaultSharedPreferences(this)
                .registerOnSharedPreferenceChangeListener(this)

            val chan: NotificationChannel = NotificationChannel(
                "ForegroundServiceChannel",
                "Foreground Service Channel", NotificationManager.IMPORTANCE_NONE
            )

            val error_chan: NotificationChannel = NotificationChannel(
                "ErrorChannel",
                "Foreground Service Error Channel", NotificationManager.IMPORTANCE_DEFAULT
            )

            val service = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            service.createNotificationChannel(chan)
            service.createNotificationChannel(error_chan)
            threadStopped()

            val mTrackReceiver: BroadcastReceiver = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) {
                    mTrackIntent = intent
                    val mCurrentTrack: Bundle? = intent.getBundleExtra("track")
                    if (mCurrentTrack != null) {
                        var mPath = mCurrentTrack.getString("path").orEmpty()
                        mPath = mPath.substring(
                            0,
                            mPath.indexOf("/")
                        ) + ":" + mPath.substring(mPath.indexOf("/") + 1)
                        Log.v("ForegroundService", mPath)
                        applicationContext.contentResolver.persistedUriPermissions.forEach {
                            val path = openContentFd(
                                DocumentsContract.buildChildDocumentsUriUsingTree(it.uri, mPath)
                            )
                            val sharedPreferences = PreferenceManager.getDefaultSharedPreferences(applicationContext)
                            val reqs = MetadataReqFlag.none()

                            if (sharedPreferences.getBoolean("artist_req", false)) {
                                reqs.add(MetadataReqFlag.Artist)
                            }
                            if (sharedPreferences.getBoolean("title_req", false)) {
                                reqs.add(MetadataReqFlag.Title)
                            }
                            if (sharedPreferences.getBoolean("album_req", false)) {
                                reqs.add(MetadataReqFlag.Album)
                            }
                            if (sharedPreferences.getBoolean("release_mbid_req", false)) {
                                reqs.add(MetadataReqFlag.ReleaseMBID)
                            }
                            if (sharedPreferences.getBoolean("artist_mbid_req", false)) {
                                reqs.add(MetadataReqFlag.ArtistMBIDS)
                            }
                            if (sharedPreferences.getBoolean("recording_mbid_req", false)) {
                                reqs.add(MetadataReqFlag.RecordingMBID)
                            }
                            if (path != null) {
                                val ext = mPath.substring(mPath.lastIndexOf(".") + 1)
                                val dur = mCurrentTrack.getInt("durMs", -1)
                                val pos = intent.getIntExtra("pos", 0)
                                Log.v("ForegroundService", "Pos: $pos")
                                mTrackFunction(path, ext, dur, pos, reqs.toBits())
                                return
                            }
                        }
                        notScrobbling()
                    }

                    // processTrackIntent()
                    Log.w("ForegroundService", "mTrackReceiver $intent")
                }
            }

            val mStatusReceiver: BroadcastReceiver = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) {
                    mStatusIntent = intent
                    mStatusFunction(intent.getIntExtra("state", -1))
                    Log.w("ForegroundService", "mStatusReceiver $intent")
                }
            }

            /*
            val mPlayingModeReceiver: BroadcastReceiver = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) {
                    mPlayingModeIntent = intent
                    Log.w("ForegroundService", "mPlayingModeReceiver $intent")
                }
            }
            */


            mTrackIntent =
                registerReceiver(
                    mTrackReceiver,
                    IntentFilter("com.maxmpz.audioplayer.TRACK_CHANGED")
                )
            mStatusIntent =
                registerReceiver(
                    mStatusReceiver,
                    IntentFilter("com.maxmpz.audioplayer.STATUS_CHANGED")
                )
            /*
            mPlayingModeIntent = registerReceiver(
                mPlayingModeReceiver,
                IntentFilter("com.maxmpz.audioplayer.PLAYING_MODE_CHANGED")
            )
            */
        }

        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? {
        return null
    }

    fun threadStopped() {
        val notificationIntent = Intent(this, SettingsActivity::class.java)
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            notificationIntent,
            PendingIntent.FLAG_IMMUTABLE
        )

        val notification: Notification = Notification.Builder(this, "ForegroundServiceChannel")
            .setContentTitle("PowerAmp ListenBrainz")
            .setContentText("The service is sleeping")
            .setOngoing(true)
            .setSmallIcon(R.drawable.baseline_close)
            .setContentIntent(pendingIntent)
            .build()

        startForeground(1, notification)
    }

    fun isScrobbling() {
        val notificationIntent = Intent(this, SettingsActivity::class.java)
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            notificationIntent,
            PendingIntent.FLAG_IMMUTABLE
        )

        val notification: Notification = Notification.Builder(this, "ForegroundServiceChannel")
            .setContentTitle("PowerAmp ListenBrainz")
            .setContentText("The service is running")
            .setOngoing(true)
            .setSmallIcon(R.drawable.baseline_book)
            .setContentIntent(pendingIntent)
            .build()

        startForeground(1, notification)
    }

    fun notScrobbling() {
        val notificationIntent = Intent(this, SettingsActivity::class.java)
        val pendingIntent = PendingIntent.getActivity(
            this,
            0,
            notificationIntent,
            PendingIntent.FLAG_IMMUTABLE
        )

        val notification: Notification = Notification.Builder(this, "ForegroundServiceChannel")
            .setContentTitle("PowerAmp ListenBrainz")
            .setContentText("The service is not scrobbling this song")
            .setOngoing(true)
            .setSmallIcon(R.drawable.baseline_block)
            .setContentIntent(pendingIntent)
            .build()

        startForeground(1, notification)
    }

    fun crashNotify(error: String) {
        val notificationIntent = Intent(this, SettingsActivity::class.java)
        notificationIntent.putExtra("error", error)
        val pendingIntent = PendingIntent.getActivity(
            this,
            1,
            notificationIntent,
            PendingIntent.FLAG_IMMUTABLE
        )

        val notification: Notification = Notification.Builder(this, "ErrorChannel")
            .setContentTitle("Error in Poweramp ListenBrainz")
            .setContentText(error)
            .setSmallIcon(R.drawable.baseline_bug)
            .setContentIntent(pendingIntent)
            .build()

        val manager = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
        errNotifyNum += 1
        manager.notify(errNotifyNum, notification)
    }

    fun getToken(): String {
        val sharedPreferences = PreferenceManager.getDefaultSharedPreferences(this)
        return "Token " + sharedPreferences.getString("token", "")
    }

    fun getCache(): String {
        return cacheDir.absolutePath.toString()
    }

    /*
    private fun parsePathFromIntent(intent: Intent): String? {
        val filepath: String?
        filepath = when (intent.action) {
            Intent.ACTION_VIEW -> intent.data?.let { openContentFd(it) }
            Intent.ACTION_SEND -> intent.getStringExtra(Intent.EXTRA_TEXT)?.let {
                val uri = Uri.parse(it.trim())
                if (uri.isHierarchical && !uri.isRelative) openContentFd(uri) else null
            }
            else -> intent.getStringExtra("filepath")
        }
        return filepath
    }
    */

    private fun openContentFd(uri: Uri): String? {
        val resolver = applicationContext.contentResolver
        Log.v("ForegroundService", "Resolving content URI: $uri")

        val fd = try {
            val desc = resolver.openFileDescriptor(uri, "r")
            desc!!.detachFd()
        } catch (e: Exception) {
            Log.e("ForegroundService", "Failed to open content fd: $e")
            return null
        }
        // See if we skip the indirection and read the real file directly
        val path = findRealPath(fd)
        if (path != null) {
            Log.v("ForegroundService", "Found real file path: $path")
            ParcelFileDescriptor.adoptFd(fd).close() // we don't need that anymore
            return path
        }
        // Else, pass the fd to mpv
        return "fd://${fd}"
    }

    private fun findRealPath(fd: Int): String? {
        var ins: InputStream? = null
        try {
            val path = File("/proc/self/fd/${fd}").canonicalPath
            if (!path.startsWith("/proc") && File(path).canRead()) {
                // Double check that we can read it
                ins = FileInputStream(path)
                ins.read()
                return path
            }
        } catch (e: Exception) {
        } finally {
            ins?.close()
        }
        return null
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (key == "token") {
            setToken(getToken())
        }
    }
}