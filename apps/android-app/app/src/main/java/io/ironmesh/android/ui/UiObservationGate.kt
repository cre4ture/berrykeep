package io.ironmesh.android.ui

internal enum class UiObservationTransition {
    NONE,
    START,
    STOP,
}

internal class UiObservationGate {
    private var foreground = false
    private var persistedStateLoaded = false

    var observationJobsActive: Boolean = false
        private set

    fun enterForeground(): UiObservationTransition {
        foreground = true
        return startIfReady()
    }

    fun leaveForeground(): UiObservationTransition {
        foreground = false
        if (!observationJobsActive) {
            return UiObservationTransition.NONE
        }
        observationJobsActive = false
        return UiObservationTransition.STOP
    }

    fun markPersistedStateLoaded(): UiObservationTransition {
        persistedStateLoaded = true
        return startIfReady()
    }

    private fun startIfReady(): UiObservationTransition {
        if (!foreground || !persistedStateLoaded || observationJobsActive) {
            return UiObservationTransition.NONE
        }
        observationJobsActive = true
        return UiObservationTransition.START
    }
}
