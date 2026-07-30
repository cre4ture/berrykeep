package io.ironmesh.android.data

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.ironmesh.android.IronmeshApplication
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class AndroidIrohDnsContextInstrumentationTest {
    @Test
    fun applicationInstallsUsableSystemDnsContextIdempotently() {
        val application = ApplicationProvider.getApplicationContext<Application>()
        assertTrue(application is IronmeshApplication)

        RustClientBridge.initialize(application)
        RustClientBridge.initialize(application)

        assertTrue(
            "Android system DNS must contain at least one nameserver",
            RustClientTestBridge.getAndroidSystemDnsServerCount() > 0,
        )
    }
}
