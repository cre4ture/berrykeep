package io.ironmesh.android.ui

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LatestOperationGateTest {
    @Test
    fun advancingGenerationInvalidatesEarlierWork() {
        val gate = LatestOperationGate()
        val first = gate.next()

        assertTrue(gate.isCurrent(first))

        val second = gate.next()

        assertFalse(gate.isCurrent(first))
        assertTrue(gate.isCurrent(second))
    }
}
