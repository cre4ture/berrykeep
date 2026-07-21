package io.ironmesh.android.ui

import android.os.Handler

internal fun interface ScheduledWebUiSessionStop {
    fun cancel()
}

internal fun interface WebUiSessionStopScheduler {
    fun schedule(
        delayMillis: Long,
        action: () -> Unit,
    ): ScheduledWebUiSessionStop
}

internal class HandlerWebUiSessionStopScheduler(
    private val handler: Handler,
) : WebUiSessionStopScheduler {
    override fun schedule(
        delayMillis: Long,
        action: () -> Unit,
    ): ScheduledWebUiSessionStop {
        val runnable = Runnable(action)
        handler.postDelayed(runnable, delayMillis)
        return ScheduledWebUiSessionStop { handler.removeCallbacks(runnable) }
    }
}

internal class WebUiSessionBackgroundGrace(
    private val scheduler: WebUiSessionStopScheduler,
    private val gracePeriodMillis: Long,
    private val onGraceExpired: () -> Unit,
) {
    private var pendingStop: ScheduledWebUiSessionStop? = null

    fun scheduleStop() {
        cancelScheduledStop()
        pendingStop = scheduler.schedule(gracePeriodMillis) {
            pendingStop = null
            onGraceExpired()
        }
    }

    fun cancelScheduledStop() {
        pendingStop?.cancel()
        pendingStop = null
    }
}
