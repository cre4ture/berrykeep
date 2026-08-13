package io.ironmesh.android.work

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class ConflatedNetworkChangeNotifierTest {
    @Test
    fun conflatesCallbackBurstsAndUsesTheLatestReason() = runTest {
        val processed = mutableListOf<String>()
        val notifier = ConflatedNetworkChangeNotifier(
            scope = backgroundScope,
            debounceMs = 300L,
            onNetworkChange = { reason -> processed += reason },
            onFailure = { _, error -> throw error },
        )

        notifier.submit("network available")
        notifier.submit("first capabilities")
        notifier.submit("latest capabilities")
        runCurrent()
        advanceTimeBy(299L)
        runCurrent()
        assertEquals(emptyList<String>(), processed)

        advanceTimeBy(1L)
        runCurrent()
        assertEquals(listOf("latest capabilities"), processed)

        notifier.close()
    }

    @Test
    fun aFailedHintDoesNotStopLaterNetworkChanges() = runTest {
        val processed = mutableListOf<String>()
        val failures = mutableListOf<String>()
        val notifier = ConflatedNetworkChangeNotifier(
            scope = backgroundScope,
            debounceMs = 0L,
            onNetworkChange = { reason ->
                if (reason == "broken") {
                    error("native failure")
                }
                processed += reason
            },
            onFailure = { reason, _ -> failures += reason },
        )

        notifier.submit("broken")
        runCurrent()
        notifier.submit("recovered")
        runCurrent()

        assertEquals(listOf("broken"), failures)
        assertEquals(listOf("recovered"), processed)

        notifier.close()
    }
}
