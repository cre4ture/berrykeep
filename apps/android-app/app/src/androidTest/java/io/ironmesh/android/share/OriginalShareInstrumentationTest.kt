package io.ironmesh.android.share

import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.content.IntentCompat
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.FileNotFoundException

@RunWith(AndroidJUnit4::class)
class OriginalShareInstrumentationTest {
    private val appContext by lazy { ApplicationProvider.getApplicationContext<Context>() }

    @After
    fun tearDown() {
        OriginalShareCapabilityStore(appContext).clearForTesting()
    }

    @Test
    fun shareIntent_grantsReadAccessThroughClipDataAndExtraStream() {
        val capability = sampleCapability()
        val uri = originalShareDocumentUri(appContext, capability.token)
        val intent = buildOriginalShareIntent(appContext, capability, uri)

        assertEquals(Intent.ACTION_SEND, intent.action)
        assertEquals("video/mp4", intent.type)
        assertTrue(intent.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION != 0)
        assertEquals(
            uri,
            IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java),
        )
        assertNotNull(intent.clipData)
        assertEquals(uri, intent.clipData?.getItemAt(0)?.uri)
    }

    @Test
    fun capabilityStore_expiresAndRemovesOldShares() {
        var now = 1_000L
        val token = "00000000-0000-4000-8000-000000000001"
        val store = OriginalShareCapabilityStore(
            context = appContext,
            clock = { now },
            tokenFactory = { token },
        )
        store.clearForTesting()
        store.create(sampleRequest())
        assertEquals("videos/large.mp4", store.resolve(token).remotePath)

        now += 24L * 60L * 60L * 1_000L
        assertThrows(FileNotFoundException::class.java) { store.resolve(token) }
    }

    private fun sampleCapability() = OriginalShareCapability(
        token = "00000000-0000-4000-8000-000000000001",
        remotePath = "videos/large.mp4",
        snapshotId = null,
        versionId = "version-video-1",
        displayName = "large.mp4",
        mimeType = "video/mp4",
        sizeBytes = 8_000_000_000L,
        createdAtUnixMs = 1_000L,
        expiresAtUnixMs = Long.MAX_VALUE,
    )

    private fun sampleRequest() = OriginalShareRequest(
        requestId = "request-1",
        remotePath = "videos/large.mp4",
        snapshotId = null,
        versionId = "version-video-1",
        displayName = "large.mp4",
        mimeType = "video/mp4",
        sizeBytes = 8_000_000_000L,
    )
}
