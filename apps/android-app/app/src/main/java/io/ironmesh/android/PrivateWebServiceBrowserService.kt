package io.ironmesh.android

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import io.ironmesh.android.data.PrivateWebServiceBrowserSession

/** Keeps the process-local private-service proxy alive for a system browser. */
class PrivateWebServiceBrowserService : Service() {
    override fun onCreate() {
        super.onCreate()
        ensureNotificationChannel()
        startForeground(NOTIFICATION_ID, notification())
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_NOT_STICKY

    override fun onDestroy() {
        PrivateWebServiceBrowserSession.serviceStopped()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun notification() =
        NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle("BerryKeep private service active")
            .setContentText("Keeping the local private-service proxy available until you return to BerryKeep.")
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build()

    private fun ensureNotificationChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }
        val notificationManager = getSystemService(NotificationManager::class.java)
        notificationManager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                "BerryKeep private services",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps a local private web service available in your browser"
            },
        )
    }

    private companion object {
        const val CHANNEL_ID = "ironmesh-private-web-service"
        const val NOTIFICATION_ID = 4002
    }
}
