package io.ironmesh.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Test

class DeviceAuthStateTest {
    @Test
    fun nodePriorityOverrideRoundTripsInsideBootstrap() {
        val nodeId = "018f1f74-7b65-7c09-9d13-3a6644d0d999"
        val initial = DeviceAuthState(
            connectionInput = """{"version":1,"cluster_id":"018f1f74-7b65-7c09-9d13-3a6644d0d111"}""",
        )

        val updated = initial.withNodePriorityOverride(nodeId, 7)

        assertEquals(mapOf(nodeId to 7), updated.nodePriorityOverrides())
        val automatic = updated.withNodePriorityOverride(nodeId, null)
        assertEquals(emptyMap<String, Int>(), automatic.nodePriorityOverrides())
        assertFalse(automatic.connectionInput.contains("node_priority_overrides"))
    }

    @Test
    fun nodePriorityOverrideRejectsOutOfRangeValues() {
        val initial = DeviceAuthState(connectionInput = """{"version":1}""")

        assertThrows(IllegalArgumentException::class.java) {
            initial.withNodePriorityOverride("node-1", MAX_NODE_CONNECTION_PRIORITY + 1)
        }
    }
}
