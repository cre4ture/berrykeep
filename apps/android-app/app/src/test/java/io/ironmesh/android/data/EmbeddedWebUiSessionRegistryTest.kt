package io.ironmesh.android.data

import org.junit.After
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EmbeddedWebUiSessionRegistryTest {
    @After
    fun clearSession() {
        EmbeddedWebUiSessionRegistry.clear()
    }

    @Test
    fun onlyTheActiveSessionIsAccepted() {
        val active = EmbeddedWebUiSession("http://127.0.0.1:18080/", "active")
        val stale = EmbeddedWebUiSession("http://127.0.0.1:18081/", "stale")

        EmbeddedWebUiSessionRegistry.activate(active)

        assertTrue(EmbeddedWebUiSessionRegistry.isActive(active))
        assertFalse(EmbeddedWebUiSessionRegistry.isActive(stale))
    }

    @Test
    fun clearingSessionInvalidatesTheHostedWebView() {
        val session = EmbeddedWebUiSession("http://127.0.0.1:18080/", "active")
        EmbeddedWebUiSessionRegistry.activate(session)

        EmbeddedWebUiSessionRegistry.clear()

        assertFalse(EmbeddedWebUiSessionRegistry.isActive(session))
    }
}
