package io.ironmesh.android.data

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class AppConnectionStatusStoreTest {
    @Test
    fun loadsPersistedStatusWithoutWritingItAgain() = runTest {
        val persisted = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            updatedUnixMs = 100L,
            lastSuccessfulConnectionUnixMs = 100L,
        )
        val persistence = FakePersistence(persisted)
        val store = newStore(persistence)

        assertNull(store.status.value)
        store.load()
        runCurrent()

        assertEquals(persisted, store.status.value)
        assertEquals(emptyList<AppConnectionStatus>(), persistence.saved)
    }

    @Test
    fun publishesImmediatelyAndPersistsConflatedBurstsAtTheConfiguredInterval() = runTest {
        val persistence = FakePersistence(AppConnectionStatus())
        val transitions = mutableListOf<Pair<AppConnectionStatus, AppConnectionStatus>>()
        val store = newStore(
            persistence = persistence,
            onTransition = { previous, next, _ -> transitions += previous to next },
        )
        store.load()
        runCurrent()

        store.submitDiagnosticsJson("100")
        runCurrent()
        assertEquals(100L, store.status.value?.updatedUnixMs)
        assertEquals(emptyList<AppConnectionStatus>(), persistence.saved)

        advanceTimeBy(500L)
        store.submitDiagnosticsJson("200")
        runCurrent()
        assertEquals(200L, store.status.value?.updatedUnixMs)

        advanceTimeBy(500L)
        runCurrent()
        assertEquals(listOf(200L), persistence.saved.map { it.updatedUnixMs })
        assertEquals(2, transitions.size)
    }

    @Test
    fun identicalUpdatesDoNotPublishOrSchedulePersistence() = runTest {
        val current = connectedStatus(100L)
        val persistence = FakePersistence(current)
        var transitionCount = 0
        val store = newStore(
            persistence = persistence,
            onTransition = { _, _, _ -> transitionCount += 1 },
        )
        store.load()
        runCurrent()

        store.submitDiagnosticsJson("100")
        runCurrent()
        advanceTimeBy(1_000L)
        runCurrent()

        assertEquals(current, store.status.value)
        assertEquals(0, transitionCount)
        assertEquals(emptyList<AppConnectionStatus>(), persistence.saved)
    }

    @Test
    fun malformedUpdateDoesNotStopLaterUpdates() = runTest {
        val persistence = FakePersistence(AppConnectionStatus())
        val failures = mutableListOf<String>()
        val store = newStore(persistence, failures = failures)
        store.load()
        runCurrent()

        store.submitDiagnosticsJson("invalid")
        store.submitDiagnosticsJson("300")
        runCurrent()

        assertEquals(listOf("decode"), failures)
        assertEquals(300L, store.status.value?.updatedUnixMs)
    }

    @Test
    fun flushPersistsTheLatestStatusWithoutWaitingForTheInterval() = runTest {
        val persistence = FakePersistence(AppConnectionStatus())
        val store = newStore(persistence)
        store.load()
        runCurrent()
        store.submitDiagnosticsJson("400")
        runCurrent()

        store.flush()
        runCurrent()

        assertEquals(listOf(400L), persistence.saved.map { it.updatedUnixMs })
        advanceTimeBy(1_000L)
        runCurrent()
        assertEquals(listOf(400L), persistence.saved.map { it.updatedUnixMs })
    }

    private fun kotlinx.coroutines.test.TestScope.newStore(
        persistence: FakePersistence,
        onTransition: (
            AppConnectionStatus,
            AppConnectionStatus,
            AppConnectionDiagnosticsUpdate,
        ) -> Unit = { _, _, _ -> },
        failures: MutableList<String> = mutableListOf(),
    ): AppConnectionStatusStore = AppConnectionStatusStore(
        scope = backgroundScope,
        persistence = persistence,
        decodeUpdate = { raw ->
            val timestamp = raw.toLongOrNull() ?: error("invalid update")
            AppConnectionDiagnosticsUpdate(
                lastSuccessfulConnectionUnixMs = timestamp,
                lastSuccessfulConnectionUrl = "https://example.test/api/v1/ping",
            )
        },
        mergeUpdate = { current, update ->
            val timestamp = requireNotNull(update.lastSuccessfulConnectionUnixMs)
            val next = connectedStatus(timestamp)
            if (current.updatedUnixMs == timestamp) current else next
        },
        onStatusChanged = onTransition,
        onFailure = { operation, _ -> failures += operation },
        persistenceIntervalMs = 1_000L,
    )

    private class FakePersistence(
        private val loaded: AppConnectionStatus,
    ) : AppConnectionStatusPersistence {
        val saved = mutableListOf<AppConnectionStatus>()

        override suspend fun load(): AppConnectionStatus = loaded

        override suspend fun save(status: AppConnectionStatus) {
            saved += status
        }
    }

    private companion object {
        fun connectedStatus(timestamp: Long) = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            updatedUnixMs = timestamp,
            lastSuccessfulConnectionUnixMs = timestamp,
            lastSuccessfulConnectionUrl = "https://example.test/api/v1/ping",
        )
    }
}
