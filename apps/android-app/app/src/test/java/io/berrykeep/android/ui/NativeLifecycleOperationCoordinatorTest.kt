package io.berrykeep.android.ui

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NativeLifecycleOperationCoordinatorTest {
    @Test
    fun run_serializesStopThenImmediateStart() = runTest {
        val coordinator = NativeLifecycleOperationCoordinator()
        val stopEntered = CompletableDeferred<Unit>()
        val allowStopToFinish = CompletableDeferred<Unit>()
        val events = mutableListOf<String>()

        val stop = async {
            coordinator.run {
                events += "stop-started"
                stopEntered.complete(Unit)
                allowStopToFinish.await()
                events += "stop-finished"
            }
        }
        stopEntered.await()
        val start = async {
            coordinator.run {
                events += "start"
            }
        }

        assertFalse(start.isCompleted)
        allowStopToFinish.complete(Unit)
        stop.await()
        start.await()

        assertEquals(listOf("stop-started", "stop-finished", "start"), events)
    }

    @Test
    fun runIfCurrent_skipsQueuedStopAfterNewerStartIntent() = runBlocking {
        val coordinator = NativeLifecycleOperationCoordinator()
        val stopGeneration = coordinator.nextGeneration()
        val startGeneration = coordinator.nextGeneration()
        var stopRan = false
        var startRan = false

        val stopExecuted = coordinator.runIfCurrent(stopGeneration) {
            stopRan = true
        }
        val startExecuted = coordinator.runIfCurrent(startGeneration) {
            startRan = true
        }

        assertFalse(stopExecuted)
        assertTrue(startExecuted)
        assertFalse(stopRan)
        assertTrue(startRan)
    }

    @Test
    fun invalidatePendingOperations_preventsAnInFlightStartFromPublishingAfterForcedStop() =
        runTest {
            val coordinator = NativeLifecycleOperationCoordinator()
            val startGeneration = coordinator.nextGeneration()
            val startEntered = CompletableDeferred<Unit>()
            val allowStartToFinish = CompletableDeferred<Unit>()
            var forcedStopRan = false

            val start = async {
                coordinator.runIfCurrent(startGeneration) {
                    startEntered.complete(Unit)
                    allowStartToFinish.await()
                }
            }
            startEntered.await()

            coordinator.invalidatePendingOperations()
            val forcedStop = async {
                coordinator.run {
                    forcedStopRan = true
                }
            }
            assertFalse(forcedStop.isCompleted)
            allowStartToFinish.complete(Unit)
            assertTrue(start.await())
            forcedStop.await()

            val staleStartPublished = coordinator.isCurrent(startGeneration)

            assertTrue(forcedStopRan)
            assertFalse(staleStartPublished)
        }

    @Test
    fun sharedCoordinator_skipsAnOldOwnersQueuedStopBeforeANewOwnersStart() = runTest {
        val sharedLifecycle = NativeLifecycleOperationCoordinator()
        val oldViewModel = WebUiLifecycleOwner(sharedLifecycle)
        val newViewModel = WebUiLifecycleOwner(sharedLifecycle)
        val oldStopGeneration = oldViewModel.beginLifecycleOperation()
        val lifecycleCallEntered = CompletableDeferred<Unit>()
        val allowLifecycleCallToFinish = CompletableDeferred<Unit>()
        var oldStopRan = false
        var newStartRan = false

        val existingLifecycleCall = async {
            sharedLifecycle.run {
                lifecycleCallEntered.complete(Unit)
                allowLifecycleCallToFinish.await()
            }
        }
        lifecycleCallEntered.await()
        val oldStop = async {
            oldViewModel.stopIfCurrent(oldStopGeneration) {
                oldStopRan = true
            }
        }
        val newStartGeneration = newViewModel.beginLifecycleOperation()
        val newStart = async {
            newViewModel.startIfCurrent(newStartGeneration) {
                newStartRan = true
            }
        }

        allowLifecycleCallToFinish.complete(Unit)
        existingLifecycleCall.await()

        assertFalse(oldStop.await())
        assertTrue(newStart.await())
        assertFalse(oldStopRan)
        assertTrue(newStartRan)
    }

    private class WebUiLifecycleOwner(
        private val lifecycle: NativeLifecycleOperationCoordinator,
    ) {
        fun beginLifecycleOperation(): Long = lifecycle.nextGeneration()

        suspend fun stopIfCurrent(
            generation: Long,
            stop: suspend () -> Unit,
        ): Boolean = lifecycle.runIfCurrent(generation, stop)

        suspend fun startIfCurrent(
            generation: Long,
            start: suspend () -> Unit,
        ): Boolean = lifecycle.runIfCurrent(generation, start)
    }
}
