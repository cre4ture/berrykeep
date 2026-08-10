package io.ironmesh.android.ui

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Test

class AppConnectionStatusFlushTest {
    @Test
    fun rethrowsCancellationWithoutReportingFailure() = runTest {
        val expected = CancellationException("cancelled")
        var reportedFailure: Throwable? = null
        var thrown: Throwable? = null

        try {
            runAppConnectionStatusFlush(
                flush = { throw expected },
                onFailure = { reportedFailure = it },
            )
        } catch (error: Throwable) {
            thrown = error
        }

        assertSame(expected, thrown)
        assertNull(reportedFailure)
    }

    @Test
    fun reportsNonCancellationFailure() = runTest {
        val expected = IllegalStateException("write failed")
        var reportedFailure: Throwable? = null

        runAppConnectionStatusFlush(
            flush = { throw expected },
            onFailure = { reportedFailure = it },
        )

        assertSame(expected, reportedFailure)
    }
}
