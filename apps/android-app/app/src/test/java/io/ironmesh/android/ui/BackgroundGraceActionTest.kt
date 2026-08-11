package io.ironmesh.android.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BackgroundGraceActionTest {
    @Test
    fun cancellationPreventsPendingAction() {
        val scheduler = FakeScheduler()
        var runs = 0
        val grace = BackgroundGraceAction(
            scheduler,
            TITLE_LATENCY_BACKGROUND_GRACE_PERIOD_MILLIS,
        ) { runs++ }

        grace.schedule()
        grace.cancel()

        assertEquals(
            TITLE_LATENCY_BACKGROUND_GRACE_PERIOD_MILLIS,
            scheduler.tasks.single().delayMillis,
        )
        assertTrue(scheduler.tasks.single().cancelled)
        assertEquals(0, runs)
    }

    @Test
    fun onlyLatestScheduledActionCanRun() {
        val scheduler = FakeScheduler()
        var runs = 0
        val grace = BackgroundGraceAction(scheduler, 5_000) { runs++ }

        grace.schedule()
        grace.schedule()

        assertTrue(scheduler.tasks.first().cancelled)
        assertFalse(scheduler.tasks.last().cancelled)
        scheduler.tasks.last().run()
        assertEquals(1, runs)
    }

    private class FakeScheduler : DelayedActionScheduler {
        val tasks = mutableListOf<FakeTask>()

        override fun schedule(
            delayMillis: Long,
            action: () -> Unit,
        ): ScheduledDelayedAction {
            return FakeTask(delayMillis, action).also(tasks::add)
        }
    }

    private class FakeTask(
        val delayMillis: Long,
        private val action: () -> Unit,
    ) : ScheduledDelayedAction {
        var cancelled = false
            private set

        override fun cancel() {
            cancelled = true
        }

        fun run() {
            check(!cancelled) { "cancelled tasks must not run" }
            action()
        }
    }
}
