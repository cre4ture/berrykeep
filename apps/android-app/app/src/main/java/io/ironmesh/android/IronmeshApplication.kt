package io.ironmesh.android

import android.app.Application
import android.os.StrictMode
import io.ironmesh.android.data.RustBridgeInitializer

class IronmeshApplication : Application() {
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
