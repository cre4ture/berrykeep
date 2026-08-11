package io.ironmesh.android.ui

import android.os.Handler

internal fun interface ScheduledDelayedAction {
    fun cancel()
}

internal fun interface DelayedActionScheduler {
    fun schedule(
        delayMillis: Long,
        action: () -> Unit,
    ): ScheduledDelayedAction
}

internal class HandlerDelayedActionScheduler(
    private val handler: Handler,
) : DelayedActionScheduler {
    override fun schedule(
        delayMillis: Long,
        action: () -> Unit,
    ): ScheduledDelayedAction {
        val runnable = Runnable(action)
        handler.postDelayed(runnable, delayMillis)
        return ScheduledDelayedAction { handler.removeCallbacks(runnable) }
    }
}

internal class BackgroundGraceAction(
    private val scheduler: DelayedActionScheduler,
    private val gracePeriodMillis: Long,
    private val onGraceExpired: () -> Unit,
) {
    private var pendingAction: ScheduledDelayedAction? = null

    fun schedule() {
        cancel()
        pendingAction = scheduler.schedule(gracePeriodMillis) {
            pendingAction = null
            onGraceExpired()
        }
    }

    fun cancel() {
        pendingAction?.cancel()
        pendingAction = null
    }
}
