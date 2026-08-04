package io.ironmesh.android.work

import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class FolderSyncExecutionCoordinatorTest {
    @Test
    fun syncRetryDelayUsesBoundedExponentialBackoff() {
        assertEquals(2_000L, nextFolderSyncRetryDelayMs(1))
        assertEquals(60_000L, nextFolderSyncRetryDelayMs(8))
    }

    @Before
    fun setUp() {
        FolderSyncExecutionCoordinator.resetForTest()
    }

    @After
    fun tearDown() {
        FolderSyncExecutionCoordinator.resetForTest()
    }

    @Test
    fun continuousRequestAtomicallyBlocksOneShotClaim() {
        FolderSyncExecutionCoordinator.requestContinuous()

        assertFalse(
            FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false),
        )
        assertTrue(FolderSyncExecutionCoordinator.snapshot().continuousRequested)
    }

    @Test
    fun activeOneShotBlocksEverySecondClaim() {
        assertTrue(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
        FolderSyncExecutionCoordinator.updateOneShotProfile("Photos")

        assertFalse(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
        assertTrue(FolderSyncExecutionCoordinator.snapshot().oneShotRunning)

        FolderSyncExecutionCoordinator.finishOneShot()
        assertFalse(FolderSyncExecutionCoordinator.snapshot().oneShotRunning)
    }

    @Test
    fun nativeContinuousRuntimeAlsoBlocksOneShotClaim() {
        assertFalse(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = true))
    }

    @Test
    fun continuousStartWaitsForClaimedOneShotToFinish() = runBlocking {
        assertTrue(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
        FolderSyncExecutionCoordinator.requestContinuous()

        val waiter = async {
            FolderSyncExecutionCoordinator.awaitOneShotCompletion()
        }
        yield()
        assertFalse(waiter.isCompleted)

        FolderSyncExecutionCoordinator.finishOneShot()
        waiter.await()
        assertTrue(waiter.isCompleted)
    }
}
