package io.berrykeep.android

import android.app.Application
import android.os.StrictMode
import io.berrykeep.android.data.RustBridgeInitializer

class BerryKeepApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        enableDebugStrictMode()
        RustBridgeInitializer.initialize(this)
    }

    private fun enableDebugStrictMode() {
        if (!BuildConfig.DEBUG) {
            return
        }
        StrictMode.setThreadPolicy(
            StrictMode.ThreadPolicy.Builder()
                .detectDiskReads()
                .detectDiskWrites()
                .penaltyLog()
                .build(),
        )
    }
}
