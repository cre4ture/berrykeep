package io.ironmesh.android.ui.components

import io.ironmesh.android.data.TitleLatencyProbeStatus
import org.junit.Assert.assertEquals
import org.junit.Test

class TitleLatencyIndicatorTest {
    @Test
    fun successfulRelayProbeUsesRelayPrefix() {
        val text = titleLatencyIndicatorText(
            TitleLatencyProbeStatus(
                state = "success",
                connectionType = "relay",
                latencyMs = 579.9,
            ),
        )

        assertEquals("R 580 ms", text)
    }

    @Test
    fun failedProbeDoesNotShowAStaleTransportPrefix() {
        val text = titleLatencyIndicatorText(
            TitleLatencyProbeStatus(
                state = "failed",
                connectionType = "direct",
            ),
        )

        assertEquals("--", text)
    }
}
