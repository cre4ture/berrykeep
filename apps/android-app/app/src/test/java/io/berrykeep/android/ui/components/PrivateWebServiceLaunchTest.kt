package io.berrykeep.android.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PrivateWebServiceLaunchTest {
    private val clientUrl = "http://127.0.0.1:43179/?embedded_client=android"

    @Test
    fun acceptsAnIssuedPrivateServiceLaunchForTheSameListener() {
        val launch = parsePrivateWebServiceLaunch(
            clientUrl,
            "http://home-nas-a1b2.localhost:43179/_berrykeep/open?token=signed-token",
        )

        requireNotNull(launch)
        assertEquals(PrivateWebServiceOpenTarget.IN_APP, launch.target)
    }

    @Test
    fun recognisesTheExplicitExternalBrowserHandoff() {
        val launch = parsePrivateWebServiceLaunch(
            clientUrl,
            "http://home-nas-a1b2.localhost:43179/_berrykeep/open?token=signed-token&berrykeep_open=browser",
        )

        requireNotNull(launch)
        assertEquals(PrivateWebServiceOpenTarget.BROWSER, launch.target)
    }

    @Test
    fun rejectsAnythingOtherThanAnIssuedServiceLaunch() {
        val invalidCandidates = listOf(
            "http://home-nas-a1b2.localhost:43180/_berrykeep/open?token=signed-token",
            "http://home-nas-a1b2.example.test:43179/_berrykeep/open?token=signed-token",
            "http://attacker:43179/_berrykeep/open?token=signed-token",
            "http://home-nas-a1b2.localhost:43179/open?token=signed-token",
            "http://home-nas-a1b2.localhost:43179/_berrykeep/other?token=signed-token",
            "http://home-nas-a1b2.localhost:43179/_berrykeep/open",
            "http://home-nas-a1b2.localhost:43179/_berrykeep/open?token=one&token=two",
        )

        invalidCandidates.forEach { candidate ->
            assertNull(candidate, parsePrivateWebServiceLaunch(clientUrl, candidate))
        }
    }

    @Test
    fun identifiesRejectedPrivateServiceCandidatesForVisibleDiagnostics() {
        assertTrue(
            isPotentialPrivateWebServiceLaunchUrl(
                "http://home-nas-a1b2.localhost:43180/_berrykeep/open?token=signed-token",
            ),
        )
        assertTrue(
            isPotentialPrivateWebServiceLaunchUrl(
                "http://bad.alias.localhost:43179/_berrykeep/open/extra?token=signed-token",
            ),
        )
        assertFalse(isPotentialPrivateWebServiceLaunchUrl("http://home-nas-a1b2.localhost:43179/"))
    }
}
