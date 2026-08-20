package io.ironmesh.android.saf

import android.content.Context
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.system.Os
import android.system.OsConstants
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.ironmesh.android.data.DeviceAuthState
import io.ironmesh.android.data.IronmeshPreferences
import io.ironmesh.android.data.RustClientTestBridge
import io.ironmesh.android.data.RustPreferencesBridge
import io.ironmesh.android.share.OriginalShareCapabilityStore
import io.ironmesh.android.share.OriginalShareRequest
import io.ironmesh.android.share.originalShareDocumentUri
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
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
        OriginalShareCapabilityStore(appContext).clearForTesting()
    }

    @After
    fun tearDown() {
        RustClientTestBridge.stopRendezvousRenewalScenario()
        IronmeshPreferences.clearDeviceAuthState(appContext)
        OriginalShareCapabilityStore(appContext).clearForTesting()
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
            capturedPaths.count { it.startsWith(expectedRequestPath) } >= 3,
        )
    }

    @Test
    fun openDocument_supportsRandomAccess() {
        val scenario = configureProviderDownloadScenario()
        val descriptor = requireNotNull(
            appContext.contentResolver.openFileDescriptor(scenario.documentUri, "r"),
        )

        descriptor.use {
            val tailOffset = scenario.expectedContents.size - RANDOM_ACCESS_READ_SIZE
            assertEquals(
                tailOffset.toLong(),
                Os.lseek(it.fileDescriptor, tailOffset.toLong(), OsConstants.SEEK_SET),
            )
            assertArrayEquals(
                scenario.expectedContents.copyOfRange(
                    tailOffset,
                    tailOffset + RANDOM_ACCESS_READ_SIZE,
                ),
                readExactly(it, RANDOM_ACCESS_READ_SIZE),
            )

            assertEquals(0L, Os.lseek(it.fileDescriptor, 0, OsConstants.SEEK_SET))
            assertArrayEquals(
                scenario.expectedContents.copyOfRange(0, RANDOM_ACCESS_READ_SIZE),
                readExactly(it, RANDOM_ACCESS_READ_SIZE),
            )
        }
    }

    @Test
    fun sharedOriginal_usesCapabilityMetadataAndPinnedVersion() {
        val scenario = configureProviderDownloadScenario()
        val capability = OriginalShareCapabilityStore(appContext).create(
            OriginalShareRequest(
                requestId = "instrumentation-share",
                remotePath = scenario.remoteDocumentPath,
                snapshotId = null,
                versionId = "v1",
                displayName = "shared-readme.txt",
                mimeType = "text/plain",
                sizeBytes = scenario.expectedContents.size.toLong(),
            ),
        )
        val shareUri = originalShareDocumentUri(appContext, capability.token)

        appContext.contentResolver.query(
            shareUri,
            arrayOf(
                DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                DocumentsContract.Document.COLUMN_MIME_TYPE,
                DocumentsContract.Document.COLUMN_SIZE,
            ),
            null,
            null,
            null,
        ).use { cursor ->
            assertNotNull(cursor)
            requireNotNull(cursor)
            assertTrue(cursor.moveToFirst())
            assertEquals("shared-readme.txt", cursor.getString(0))
            assertEquals("text/plain", cursor.getString(1))
            assertEquals(scenario.expectedContents.size.toLong(), cursor.getLong(2))
        }

        val contents = requireNotNull(appContext.contentResolver.openInputStream(shareUri)).use {
            it.readBytes()
        }
        assertArrayEquals(scenario.expectedContents, contents)
        assertTrue(
            jsonArrayStrings(RustClientTestBridge.getCapturedRequestPaths()).any { path ->
                path.contains("version=v1")
            },
        )
    }

    @Test
    fun queryDocument_reportsRemoteObjectSize() {
        val scenario = configureProviderDownloadScenario()
        val cursor = requireNotNull(
            appContext.contentResolver.query(
                scenario.documentUri,
                arrayOf(DocumentsContract.Document.COLUMN_SIZE),
                null,
                null,
                null,
            ),
        )

        cursor.use {
            assertTrue(it.moveToFirst())
            assertEquals(
                scenario.expectedContents.size.toLong(),
                it.getLong(it.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_SIZE)),
            )
        }
    }

    @Test
    fun queryChildDocuments_treeViewFolderMarkersAreNamedWithoutDuplicates() {
        // The mock server collapses trailing-slash/"prefix" duplicates into a single
        // canonical "prefix" entry per directory when view=tree is requested, matching the
        // production server contract (see collapse_store_index_entries_for_tree_view). The
        // provider trusts that contract rather than re-deriving directory-ness itself.
        configureProviderDownloadScenario()
        val childrenUri = DocumentsContract.buildChildDocumentsUri(
            "${appContext.packageName}.documents",
            "dir:",
        )
        val cursor = requireNotNull(
            appContext.contentResolver.query(
                childrenUri,
                arrayOf(
                    DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                    DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                ),
                null,
                null,
                null,
            ),
        )

        val rows = cursor.use {
            buildList {
                while (it.moveToNext()) {
                    add(
                        it.getString(0) to it.getString(1),
                    )
                }
            }
        }

        assertEquals(
            listOf(
                "dir:docs" to "docs",
                "dir:photos" to "photos",
            ),
            rows,
        )
        assertTrue(
            "SAF directory rows must always have a display name: $rows",
            rows.all { (_, displayName) -> displayName.isNotBlank() },
        )
        assertTrue(
            "SAF directory listing should request the deduplicated tree view",
            jsonArrayStrings(RustClientTestBridge.getCapturedRequestPaths()).any { path ->
                path.startsWith("/api/v1/store/index?") && path.contains("view=tree")
            },
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

    private fun readExactly(
        descriptor: ParcelFileDescriptor,
        byteCount: Int,
    ): ByteArray {
        val result = ByteArray(byteCount)
        var offset = 0
        while (offset < result.size) {
            val bytesRead = Os.read(
                descriptor.fileDescriptor,
                result,
                offset,
                result.size - offset,
            )
            assertTrue("unexpected end of document", bytesRead > 0)
            offset += bytesRead
        }
        return result
    }

    private data class ProviderDownloadScenario(
        val documentUri: android.net.Uri,
        val remoteDocumentPath: String,
        val expectedContents: ByteArray,
    )

    private companion object {
        const val CONCURRENT_OPEN_COUNT = 2
        const val CONCURRENT_OPEN_TIMEOUT_SECONDS = 30L
        const val RANDOM_ACCESS_READ_SIZE = 64
    }
}
