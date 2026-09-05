package io.ironmesh.android.ui.components

import org.junit.Assert.assertEquals
import org.junit.Test

class EmbeddedWebUiUrlTest {
    @Test
    fun embeddedWebUiUrl_preservesApplicationParametersAndUsesNativeAccent() {
        val url = embeddedWebUiUrl(
            url = "http://127.0.0.1:41873/?page=gallery&embedded_client=ios&accent_color=%23ff0000",
            client = "android",
            accentColorHex = "#2563eb",
        )

        assertEquals(
            "http://127.0.0.1:41873/?page=gallery&embedded_client=android&accent_color=%232563EB",
            url,
        )
    }

    @Test
    fun embeddedWebUiUrl_fallsBackToDefaultAccentForInvalidInput() {
        val url = embeddedWebUiUrl(
            url = "http://127.0.0.1:41873/",
            client = "android",
            accentColorHex = "not-a-color",
        )

        assertEquals(
            "http://127.0.0.1:41873/?embedded_client=android&accent_color=%2314B8A6",
            url,
        )
    }
}
