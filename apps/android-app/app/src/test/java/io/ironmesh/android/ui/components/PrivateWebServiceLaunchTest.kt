package io.ironmesh.android.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PrivateWebServiceLaunchTest {
    private val clientUrl = "http://127.0.0.1:43179/?embedded_client=android"

    @Test
    fun acceptsAnIssuedPrivateServiceLaunchForTheSameListener() {
        val launch = parsePrivateWebServiceLaunch(
            clientUrl,
            "http://home-nas-a1b2.localhost:43179/_ironmesh/open?token=signed-token",
        )

        requireNotNull(launch)
        assertEquals(PrivateWebServiceOpenTarget.IN_APP, launch.target)
    }

    @Test
    fun recognisesTheExplicitExternalBrowserHandoff() {
        val launch = parsePrivateWebServiceLaunch(
            clientUrl,
            "http://home-nas-a1b2.localhost:43179/_ironmesh/open?token=signed-token&ironmesh_open=browser",
        )

        requireNotNull(launch)
        assertEquals(PrivateWebServiceOpenTarget.BROWSER, launch.target)
    }

    @Test
    fun rejectsAnythingOtherThanAnIssuedServiceLaunch() {
        val invalidCandidates = listOf(
            "http://home-nas-a1b2.localhost:43180/_ironmesh/open?token=signed-token",
            "http://home-nas-a1b2.example.test:43179/_ironmesh/open?token=signed-token",
            "http://attacker:43179/_ironmesh/open?token=signed-token",
            "http://home-nas-a1b2.localhost:43179/open?token=signed-token",
            "http://home-nas-a1b2.localhost:43179/_ironmesh/other?token=signed-token",
            "http://home-nas-a1b2.localhost:43179/_ironmesh/open",
            "http://home-nas-a1b2.localhost:43179/_ironmesh/open?token=one&token=two",
        )

        invalidCandidates.forEach { candidate ->
            assertNull(candidate, parsePrivateWebServiceLaunch(clientUrl, candidate))
        }
    }
}
