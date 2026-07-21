package io.ironmesh.android.data

/**
 * Tracks the embedded Web UI session that is currently backed by the process-local server.
 *
 * The session is deliberately not persisted: after a process restart the loopback listener is
 * gone, so a restored WebUiActivity must return to the native UI instead of loading a stale URL.
 */
object EmbeddedWebUiSessionRegistry {
    @Volatile
    private var activeAuthorization: String? = null

    fun activate(session: EmbeddedWebUiSession) {
        activeAuthorization = session.authorization
    }

    fun isActive(session: EmbeddedWebUiSession): Boolean =
        activeAuthorization == session.authorization

    fun clear() {
        activeAuthorization = null
    }
}
