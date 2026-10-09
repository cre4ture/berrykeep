package io.berrykeep.android.ui.screens

import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class GalleryOriginalImageCacheTest {
    @Test
    fun fileFor_stagesDifferentImagesConcurrently() {
        val cache = GalleryOriginalImageCache()
        val bothDownloadsStarted = CountDownLatch(2)
        val allowDownloadsToFinish = CountDownLatch(1)
        val executor = Executors.newFixedThreadPool(2)
        try {
            val first = executor.submit<File> {
                cache.fileFor("content://gallery/first") {
                    bothDownloadsStarted.countDown()
                    assertTrue(allowDownloadsToFinish.await(5, TimeUnit.SECONDS))
                    File.createTempFile("gallery-first", ".img")
                }
            }
            val second = executor.submit<File> {
                cache.fileFor("content://gallery/second") {
                    bothDownloadsStarted.countDown()
                    assertTrue(allowDownloadsToFinish.await(5, TimeUnit.SECONDS))
                    File.createTempFile("gallery-second", ".img")
                }
            }

            assertTrue(bothDownloadsStarted.await(5, TimeUnit.SECONDS))
            allowDownloadsToFinish.countDown()
            first.get(5, TimeUnit.SECONDS).delete()
            second.get(5, TimeUnit.SECONDS).delete()
        } finally {
            executor.shutdownNow()
            cache.clear()
        }
    }

    @Test
    fun fileFor_coalescesConcurrentRequestsForTheSameImage() {
        val cache = GalleryOriginalImageCache()
        val downloadStarted = CountDownLatch(1)
        val allowDownloadToFinish = CountDownLatch(1)
        val stagedFiles = AtomicInteger()
        val executor = Executors.newFixedThreadPool(2)
        try {
            val first = executor.submit<File> {
                cache.fileFor("content://gallery/shared") {
                    stagedFiles.incrementAndGet()
                    downloadStarted.countDown()
                    assertTrue(allowDownloadToFinish.await(5, TimeUnit.SECONDS))
                    File.createTempFile("gallery-shared", ".img")
                }
            }
            assertTrue(downloadStarted.await(5, TimeUnit.SECONDS))
            val second = executor.submit<File> {
                cache.fileFor("content://gallery/shared") {
                    stagedFiles.incrementAndGet()
                    File.createTempFile("gallery-duplicate", ".img")
                }
            }

            allowDownloadToFinish.countDown()
            val firstFile = first.get(5, TimeUnit.SECONDS)
            val secondFile = second.get(5, TimeUnit.SECONDS)

            assertEquals(1, stagedFiles.get())
            assertSame(firstFile, secondFile)
        } finally {
            executor.shutdownNow()
            cache.clear()
        }
    }
}
