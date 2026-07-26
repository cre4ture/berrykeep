package io.ironmesh.android.data

import org.junit.Assert.assertEquals
import org.junit.Test

class TitleLatencyMonitorTest {
    @Test
    fun normalizedSettingsAllowOneSecondPeriod() {
        val settings = TitleLatencyMonitorSettings(periodSeconds = 1)

        assertEquals(1L, settings.normalized().periodSeconds)
    }

    @Test
    fun normalizedSettingsClampPeriodsBelowOneSecond() {
        val settings = TitleLatencyMonitorSettings(periodSeconds = 0)

        assertEquals(1L, settings.normalized().periodSeconds)
    }
}
