package io.ironmesh.android.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class UiObservationGateTest {
    @Test
    fun foregroundWaitsForPersistedStateBeforeStartingObservation() {
        val gate = UiObservationGate()

        assertEquals(UiObservationTransition.NONE, gate.enterForeground())
        assertFalse(gate.observationJobsActive)
        assertEquals(UiObservationTransition.START, gate.markPersistedStateLoaded())
        assertTrue(gate.observationJobsActive)
    }

    @Test
    fun loadedBackgroundStateWaitsForForegroundBeforeStartingObservation() {
        val gate = UiObservationGate()

        assertEquals(UiObservationTransition.NONE, gate.markPersistedStateLoaded())
        assertEquals(UiObservationTransition.START, gate.enterForeground())
        assertTrue(gate.observationJobsActive)
    }

    @Test
    fun backgroundStopsObservationAndNextForegroundRestartsItOnce() {
        val gate = UiObservationGate()
        gate.markPersistedStateLoaded()
        assertEquals(UiObservationTransition.START, gate.enterForeground())

        assertEquals(UiObservationTransition.NONE, gate.enterForeground())
        assertEquals(UiObservationTransition.STOP, gate.leaveForeground())
        assertFalse(gate.observationJobsActive)
        assertEquals(UiObservationTransition.NONE, gate.leaveForeground())
        assertEquals(UiObservationTransition.START, gate.enterForeground())
        assertTrue(gate.observationJobsActive)
    }
}
