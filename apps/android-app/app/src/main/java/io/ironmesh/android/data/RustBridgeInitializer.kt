package io.ironmesh.android.data

import android.content.Context

/**
 * Initializes every process-wide bridge used by the Rust client runtime.
 *
 * Android can enter the process through an activity, worker, service, or
 * documents provider. Keeping the initialization in one application-level
 * entry point prevents those paths from acquiring different native context.
 */
object RustBridgeInitializer {
    fun initialize(context: Context) {
        val applicationContext = context.applicationContext
        RustClientBridge.initialize(applicationContext)
        RustSafBridge.initialize(applicationContext)
        RustPreferencesBridge.initialize(applicationContext)
    }
}
