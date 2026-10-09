package io.berrykeep.android.ui

import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class WebUiStartLoadingOwnershipTest {
    @Test
    fun releaseIfOwner_clearsASupersededStartBeforeItReachesNativeCode() = runBlocking {
        val lifecycle = NativeLifecycleOperationCoordinator()
        val ownership = WebUiStartLoadingOwnership()
        val generation = lifecycle.nextGeneration()
        ownership.begin(generation)

        lifecycle.nextGeneration()
        var nativeStartRan = false
        val nativeStartRanForCurrentGeneration = lifecycle.runIfCurrent(generation) {
            nativeStartRan = true
        }

        assertFalse(nativeStartRanForCurrentGeneration)
        assertFalse(nativeStartRan)
        assertTrue(ownership.releaseIfOwner(generation))
        assertFalse(ownership.hasOwner())
    }

    @Test
    fun releaseIfOwner_clearsASupersededStartAfterNativeCodeReturns() = runBlocking {
        val lifecycle = NativeLifecycleOperationCoordinator()
        val ownership = WebUiStartLoadingOwnership()
        val generation = lifecycle.nextGeneration()
        ownership.begin(generation)
        var nativeStartRan = false

        assertTrue(lifecycle.runIfCurrent(generation) { nativeStartRan = true })
        lifecycle.nextGeneration()

        assertTrue(nativeStartRan)
        assertTrue(ownership.releaseIfOwner(generation))
        assertFalse(ownership.hasOwner())
    }

    @Test
    fun releaseIfOwner_doesNotClearANewerStartsLoadingState() {
        val ownership = WebUiStartLoadingOwnership()
        ownership.begin(generation = 5)
        ownership.begin(generation = 6)

        assertFalse(ownership.releaseIfOwner(generation = 5))
        assertTrue(ownership.hasOwner())
        assertTrue(ownership.releaseIfOwner(generation = 6))
        assertFalse(ownership.hasOwner())
    }
}
