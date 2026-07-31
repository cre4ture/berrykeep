package io.ironmesh.android

import android.app.Application
import io.ironmesh.android.data.RustBridgeInitializer

class IronmeshApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        RustBridgeInitializer.initialize(this)
    }
}
