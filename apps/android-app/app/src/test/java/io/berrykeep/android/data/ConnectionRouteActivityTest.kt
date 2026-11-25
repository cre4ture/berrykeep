package io.berrykeep.android.data

import com.squareup.moshi.Moshi
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionRouteActivityTest {
    @Test
    fun backgroundProbeAfterHistoricalTrafficDoesNotReportActivity() {
        val route = route(lastUsed = 1_000L, lastSuccess = 9_900L, lastSuccessfulUse = 1_000L)

        assertFalse(route.hasRecentSuccessfulActivity(10_000L))
    }

    @Test
    fun requestInFlightKeepsRecentConfirmedTrafficActive() {
        val route = route(lastUsed = 9_950L, lastSuccess = 9_900L, lastSuccessfulUse = 9_900L)

        assertTrue(route.hasRecentSuccessfulActivity(10_000L))
    }

    @Test
    fun successfulProbeDoesNotHideAFailedTrafficAttempt() {
        val route = route(lastUsed = 9_950L, lastSuccess = 9_990L, lastSuccessfulUse = 9_900L)
            .copy(lastFailureUnixMs = 9_950L)

        assertFalse(route.hasRecentSuccessfulActivity(10_000L))
    }

    @Test
    fun legacySnapshotWithoutTrafficCompletionDoesNotInferActivityFromHealth() {
        val route = route(lastUsed = 9_900L, lastSuccess = 9_900L, lastSuccessfulUse = null)

        assertFalse(route.hasRecentSuccessfulActivity(10_000L))
    }

    private fun route(lastUsed: Long, lastSuccess: Long, lastSuccessfulUse: Long?): ConnectionRouteEndpointSnapshot {
        val trafficTimestamp = lastSuccessfulUse?.let { "\"last_successful_use_unix_ms\": $it," }.orEmpty()
        return requireNotNull(
            Moshi.Builder().add(KotlinJsonAdapterFactory()).build()
                .adapter(ConnectionRouteEndpointSnapshot::class.java)
                .fromJson(
                    """
                    {
                      "index": 0,
                      "path_kind": "direct_https",
                      "locator": "https://node.example",
                      "bootstrap_rank": 0,
                      "score": 0.0,
                      "consecutive_failures": 0,
                      "total_failures": 0,
                      "total_successes": 2,
                      "last_used_unix_ms": $lastUsed,
                      "last_success_unix_ms": $lastSuccess,
                      $trafficTimestamp
                      "background_probe_in_flight": false
                    }
                    """.trimIndent(),
                ),
        )
    }
}
