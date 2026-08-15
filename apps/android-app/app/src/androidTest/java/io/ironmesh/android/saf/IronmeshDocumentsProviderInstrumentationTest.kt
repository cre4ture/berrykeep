package io.ironmesh.android.saf

import android.content.Context
import android.provider.DocumentsContract
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.ironmesh.android.data.DeviceAuthState
import io.ironmesh.android.data.IronmeshPreferences
import io.ironmesh.android.data.RustClientTestBridge
import io.ironmesh.android.data.RustPreferencesBridge
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class IronmeshDocumentsProviderInstrumentationTest {
    private val appContext by lazy { ApplicationProvider.getApplicationContext<Context>() }

    @Before
    fun setUp() {
        RustPreferencesBridge.initialize(appContext)
        IronmeshPreferences.clearDeviceAuthState(appContext)
        RustClientTestBridge.stopRendezvousRenewalScenario()
    }

    @After
    fun tearDown() {
        RustClientTestBridge.stopRendezvousRenewalScenario()
        IronmeshPreferences.clearDeviceAuthState(appContext)
    }

    @Test
    fun openDocument_downloadsRemoteFileThroughProductionProvider() {
        val scenario = JSONObject(RustClientTestBridge.startRendezvousRenewalScenario())
        val connectionBootstrapJson = scenario.getString("connectionBootstrapJson")
        val clientIdentityJson = scenario.getString("expiredClientIdentityJson")
        val remoteDocumentPath = scenario.getString("remoteDocumentPath")
        val remoteDocumentSizeBytes = scenario.getInt("remoteDocumentSizeBytes")
        val expectedContents = ByteArray(remoteDocumentSizeBytes) { index -> (index % 251).toByte() }
        val identity = JSONObject(clientIdentityJson)

        IronmeshPreferences.setDeviceAuthState(
            appContext,
            DeviceAuthState(
                clusterId = identity.getString("cluster_id"),
                deviceId = identity.getString("device_id"),
                connectionInput = connectionBootstrapJson,
                clientIdentityJson = clientIdentityJson,
            ),
        )

        val documentUri = DocumentsContract.buildDocumentUri(
            "${appContext.packageName}.documents",
            "file:$remoteDocumentPath",
        )
        val input = appContext.contentResolver.openInputStream(documentUri)
        assertNotNull(
            "production documents provider should open $documentUri",
            input,
        )

        val actualContents = requireNotNull(input).use { it.readBytes() }

        assertArrayEquals(expectedContents, actualContents)
        val capturedPaths = jsonArrayStrings(RustClientTestBridge.getCapturedRequestPaths())
        val expectedRequestPath = "/api/v1/store/${remoteDocumentPath.replace("/", "%2F")}"
        assertTrue(
            "expected HEAD and two ranged GET requests for $expectedRequestPath, got $capturedPaths",
            capturedPaths.count { it == expectedRequestPath } >= 3,
        )
    }

    private fun jsonArrayStrings(raw: String): List<String> {
        val array = JSONArray(raw)
        return List(array.length()) { index -> array.getString(index) }
    }
}
