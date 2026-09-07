package io.ironmesh.android.data

import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat
import io.ironmesh.android.PrivateWebServiceBrowserService

/**
 * Keeps the loopback Web UI alive while a private service is open in another
 * browser process. The lifecycle owner stops the foreground service as soon
 * as BerryKeep is foregrounded again.
 */
object PrivateWebServiceBrowserSession {
    private val lock = Any()

    @Volatile
    private var active = false
    private var appIsForegrounded = true
    private var onBackgroundSessionEnded: (() -> Unit)? = null

    fun registerBackgroundSessionEndedCallback(callback: () -> Unit) {
        synchronized(lock) {
            onBackgroundSessionEnded = callback
        }
    }

    fun clearBackgroundSessionEndedCallback() {
        synchronized(lock) {
            onBackgroundSessionEnded = null
        }
    }

    fun appForegrounded(context: Context) {
        val shouldStop = synchronized(lock) {
            appIsForegrounded = true
            active
        }
        if (shouldStop) {
            context.stopService(Intent(context, PrivateWebServiceBrowserService::class.java))
            serviceStopped()
        }
    }

    fun appBackgrounded() {
        synchronized(lock) {
            appIsForegrounded = false
        }
    }

    fun acquire(context: Context): Boolean {
        synchronized(lock) {
            active = true
        }
        return runCatching {
            ContextCompat.startForegroundService(
                context,
                Intent(context, PrivateWebServiceBrowserService::class.java),
            )
        }.onFailure {
            serviceStopped()
        }.isSuccess
    }

    fun release(context: Context) {
        context.stopService(Intent(context, PrivateWebServiceBrowserService::class.java))
        serviceStopped()
    }

    fun isActive(): Boolean = active

    fun serviceStopped() {
        val callback = synchronized(lock) {
            val wasActive = active
            active = false
            if (wasActive && !appIsForegrounded) onBackgroundSessionEnded else null
        }
        callback?.invoke()
    }
}
