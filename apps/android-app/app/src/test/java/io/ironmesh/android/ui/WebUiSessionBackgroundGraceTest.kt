package io.ironmesh.android.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class WebUiSessionBackgroundGraceTest {
    @Test
    fun foregroundReturnCancelsPendingStop() {
        val scheduler = FakeScheduler()
        var stops = 0
        val grace = WebUiSessionBackgroundGrace(scheduler, 10_000) { stops++ }

        grace.scheduleStop()
        grace.cancelScheduledStop()

        assertTrue(scheduler.tasks.single().cancelled)
        assertEquals(0, stops)
    }

    @Test
    fun onlyLatestBackgroundStopCanEndSession() {
        val scheduler = FakeScheduler()
        var stops = 0
        val grace = WebUiSessionBackgroundGrace(scheduler, 10_000) { stops++ }

        grace.scheduleStop()
        grace.scheduleStop()

        assertTrue(scheduler.tasks.first().cancelled)
        assertFalse(scheduler.tasks.last().cancelled)
        scheduler.tasks.last().run()
        assertEquals(1, stops)
    }

    private class FakeScheduler : WebUiSessionStopScheduler {
        val tasks = mutableListOf<FakeTask>()

        override fun schedule(
            delayMillis: Long,
            action: () -> Unit,
        ): ScheduledWebUiSessionStop {
            return FakeTask(action).also(tasks::add)
        }
    }

    private class FakeTask(
        private val action: () -> Unit,
    ) : ScheduledWebUiSessionStop {
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
