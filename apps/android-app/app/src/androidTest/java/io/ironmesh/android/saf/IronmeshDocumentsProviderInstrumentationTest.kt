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
import java.util.concurrent.CyclicBarrier
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

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
        val scenario = configureProviderDownloadScenario()
        val input = appContext.contentResolver.openInputStream(scenario.documentUri)
        assertNotNull(
            "production documents provider should open ${scenario.documentUri}",
            input,
        )

        val actualContents = requireNotNull(input).use { it.readBytes() }

        assertArrayEquals(scenario.expectedContents, actualContents)
        val capturedPaths = jsonArrayStrings(RustClientTestBridge.getCapturedRequestPaths())
        val expectedRequestPath =
            "/api/v1/store/${scenario.remoteDocumentPath.replace("/", "%2F")}"
        assertTrue(
            "expected HEAD and two ranged GET requests for $expectedRequestPath, got $capturedPaths",
            capturedPaths.count { it == expectedRequestPath } >= 3,
        )
    }

    @Test
    fun openDocument_concurrentReadsOfSameRemoteFileReturnCompleteContents() {
        val scenario = configureProviderDownloadScenario()
        val startBarrier = CyclicBarrier(CONCURRENT_OPEN_COUNT)
        val executor = Executors.newFixedThreadPool(CONCURRENT_OPEN_COUNT)

        try {
            val downloads = List(CONCURRENT_OPEN_COUNT) {
                executor.submit<ByteArray> {
                    startBarrier.await(CONCURRENT_OPEN_TIMEOUT_SECONDS, TimeUnit.SECONDS)
                    val input =
                        requireNotNull(
                            appContext.contentResolver.openInputStream(scenario.documentUri),
                        )
                    input.use { it.readBytes() }
                }
            }

            downloads.forEachIndexed { index, download ->
                assertArrayEquals(
                    "concurrent SAF download $index should return the complete object",
                    scenario.expectedContents,
                    download.get(CONCURRENT_OPEN_TIMEOUT_SECONDS, TimeUnit.SECONDS),
                )
            }
        } finally {
            executor.shutdownNow()
        }
    }

    private fun configureProviderDownloadScenario(): ProviderDownloadScenario {
        val scenario = JSONObject(RustClientTestBridge.startRendezvousRenewalScenario())
        val connectionBootstrapJson = scenario.getString("connectionBootstrapJson")
        val clientIdentityJson = scenario.getString("expiredClientIdentityJson")
        val remoteDocumentPath = scenario.getString("remoteDocumentPath")
        val remoteDocumentSizeBytes = scenario.getInt("remoteDocumentSizeBytes")
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

        return ProviderDownloadScenario(
            documentUri = DocumentsContract.buildDocumentUri(
                "${appContext.packageName}.documents",
                "file:$remoteDocumentPath",
            ),
            remoteDocumentPath = remoteDocumentPath,
            expectedContents = ByteArray(remoteDocumentSizeBytes) { index -> (index % 251).toByte() },
        )
    }

    private fun jsonArrayStrings(raw: String): List<String> {
        val array = JSONArray(raw)
        return List(array.length()) { index -> array.getString(index) }
    }

    private data class ProviderDownloadScenario(
        val documentUri: android.net.Uri,
        val remoteDocumentPath: String,
        val expectedContents: ByteArray,
    )

    private companion object {
        const val CONCURRENT_OPEN_COUNT = 2
        const val CONCURRENT_OPEN_TIMEOUT_SECONDS = 30L
    }
}
