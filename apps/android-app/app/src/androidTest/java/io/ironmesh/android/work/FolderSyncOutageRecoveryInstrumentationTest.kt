package io.ironmesh.android.work

import android.content.Context
import android.os.SystemClock
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.ironmesh.android.data.FolderSyncNetworkPolicy
import io.ironmesh.android.data.RustClientBridge
import io.ironmesh.android.data.RustClientTestBridge
import io.ironmesh.android.data.RustPreferencesBridge
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class FolderSyncOutageRecoveryInstrumentationTest {
    private val context = ApplicationProvider.getApplicationContext<Context>()
    private lateinit var localFolder: File

    @Before
    fun setUp() {
        RustClientBridge.initialize(context)
        RustPreferencesBridge.initialize(context)
        RustClientTestBridge.stopRendezvousRenewalScenario()
        localFolder = File(context.cacheDir, "folder-sync-outage-${UUID.randomUUID()}")
    }

    @After
    fun tearDown() {
        RustClientTestBridge.stopRendezvousRenewalScenario()
        localFolder.deleteRecursively()
    }

    /**
     * The test bootstrap plans both a direct node route and a Rendezvous relay route. The local
     * test server makes both routes return an unavailable response during the outage; it is not a
     * complete Rendezvous implementation, but it exercises the Android client's real configured
     * fallback route and confirms that all planned routes are unusable before recovery.
     */
    @Test
    fun folderSync_skipsBlockedNetwork_doesNotRetryAutonomously_andRecoversThroughCachedClient() {
        val scenario = JSONObject(RustClientTestBridge.startFolderSyncOutageScenario())
        val bootstrapJson = scenario.getString("connectionBootstrapJson")
        val clientIdentityJson = scenario.getString("clientIdentityJson")
        val remoteDocumentPath = scenario.getString("remoteDocumentPath")
        val expectedDocumentSize = scenario.getLong("remoteDocumentSizeBytes")

        val routes = JSONObject(
            RustClientBridge.getConnectionRouteSnapshot(
                bootstrapJson,
                null,
                clientIdentityJson,
                false,
            ),
        ).getJSONArray("endpoints")
        val routeKinds = buildSet {
            for (index in 0 until routes.length()) {
                add(routes.getJSONObject(index).getString("pathKind"))
            }
        }
        assertTrue("expected direct node route, got $routeKinds", routeKinds.contains("direct_https"))
        assertTrue("expected Rendezvous relay route, got $routeKinds", routeKinds.contains("relay_tunnel"))

        val directAttemptsBeforeBlockedGate =
            RustClientTestBridge.getFolderSyncOutageDirectConnectionAttemptCount()
        val rendezvousAttemptsBeforeBlockedGate =
            RustClientTestBridge.getFolderSyncOutageRendezvousContactAttemptCount()
        val blocked = FolderSyncNetworkGate.evaluate(
            policy = FolderSyncNetworkPolicy(),
            snapshot = FolderSyncNetworkSnapshot(connected = false),
        )

        assertFalse(blocked.allowed)
        assertEquals("No active internet connection", blocked.reason)
        // The production Worker and foreground service use this decision before JNI sync startup.
        // This assertion intentionally verifies the gate only; it does not start either component.
        assertEquals(
            directAttemptsBeforeBlockedGate,
            RustClientTestBridge.getFolderSyncOutageDirectConnectionAttemptCount(),
        )
        assertEquals(
            rendezvousAttemptsBeforeBlockedGate,
            RustClientTestBridge.getFolderSyncOutageRendezvousContactAttemptCount(),
        )

        RustClientTestBridge.setFolderSyncOutageScenarioAvailable(false)
        assertFolderSyncFails(bootstrapJson, clientIdentityJson)

        val directAttemptsAfterOutage =
            RustClientTestBridge.getFolderSyncOutageDirectConnectionAttemptCount()
        val rendezvousAttemptsAfterOutage =
            RustClientTestBridge.getFolderSyncOutageRendezvousContactAttemptCount()
        assertTrue(
            "the unavailable direct node route was not attempted",
            directAttemptsAfterOutage > directAttemptsBeforeBlockedGate,
        )
        assertTrue(
            "the unavailable Rendezvous relay route was not attempted",
            rendezvousAttemptsAfterOutage > rendezvousAttemptsBeforeBlockedGate,
        )

        // This is a one-shot sync entry point. After it returns, it must not leave an
        // autonomous retry loop behind while the app is idle. A user-initiated foreground
        // sync may select cooling routes again, so that behavior is deliberately not asserted
        // here.
        SystemClock.sleep(250L)
        assertEquals(
            "a failed one-shot sync started an unexpected direct-route retry",
            directAttemptsAfterOutage,
            RustClientTestBridge.getFolderSyncOutageDirectConnectionAttemptCount(),
        )
        assertEquals(
            "a failed one-shot sync started an unexpected Rendezvous retry",
            rendezvousAttemptsAfterOutage,
            RustClientTestBridge.getFolderSyncOutageRendezvousContactAttemptCount(),
        )

        // Wait for the first client route circuit window to expire before testing recovery.
        // This keeps the recovery attempt independent of the prior route-cooling state.
        SystemClock.sleep(1_600L)
        RustClientTestBridge.setFolderSyncOutageScenarioAvailable(true)
        RustClientBridge.notifyNetworkChanged(bootstrapJson, null, clientIdentityJson)

        RustClientBridge.runFolderSyncOnce(
            bootstrapJson,
            localFolder.absolutePath,
            null,
            null,
            8,
            null,
            clientIdentityJson,
        )

        val synchronizedDocument = File(localFolder, remoteDocumentPath)
        assertTrue(
            "folder sync did not recover the remote document after route availability returned",
            synchronizedDocument.isFile,
        )
        assertEquals(expectedDocumentSize, synchronizedDocument.length())
    }

    private fun assertFolderSyncFails(
        bootstrapJson: String,
        clientIdentityJson: String,
    ) {
        assertThrows(RuntimeException::class.java) {
            RustClientBridge.runFolderSyncOnce(
                bootstrapJson,
                localFolder.absolutePath,
                null,
                null,
                8,
                null,
                clientIdentityJson,
            )
        }
    }
}
