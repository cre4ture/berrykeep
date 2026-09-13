package io.berrykeep.android.data

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.berrykeep.android.BerryKeepApplication
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class AndroidIrohDnsContextInstrumentationTest {
    @Test
    fun applicationInstallsUsableSystemDnsContextIdempotently() {
        val application = ApplicationProvider.getApplicationContext<Application>()
        assertTrue(application is BerryKeepApplication)

        RustClientBridge.initialize(application)
        RustClientBridge.initialize(application)

        assertTrue(
            "Android system DNS must contain at least one nameserver",
            RustClientTestBridge.getAndroidSystemDnsServerCount() > 0,
        )
    }
}
