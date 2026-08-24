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
        FolderSyncExecutionCoordinator.requestContinuousStart()

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
    fun failedServiceStartReleasesPendingClaim() {
        FolderSyncExecutionCoordinator.requestContinuousStart()
        FolderSyncExecutionCoordinator.cancelContinuousStartRequest()

        assertTrue(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
    }

    @Test
    fun oneFailedStartDoesNotReleaseAnotherPendingContinuousStart() {
        FolderSyncExecutionCoordinator.requestContinuousStart()
        FolderSyncExecutionCoordinator.requestContinuousStart()

        FolderSyncExecutionCoordinator.cancelContinuousStartRequest()

        assertFalse(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
    }

    @Test
    fun failedRefreshDoesNotReleaseAnAlreadyActiveService() {
        FolderSyncExecutionCoordinator.markContinuousServiceActive()
        FolderSyncExecutionCoordinator.requestContinuousStart()
        FolderSyncExecutionCoordinator.cancelContinuousStartRequest()

        val snapshot = FolderSyncExecutionCoordinator.snapshot()
        assertTrue(snapshot.continuousRequested)
        assertTrue(snapshot.continuousServiceActive)
        assertFalse(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
    }

    @Test
    fun continuousStartWaitsForClaimedOneShotToFinish() = runBlocking {
        assertTrue(FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive = false))
        FolderSyncExecutionCoordinator.requestContinuousStart()

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
