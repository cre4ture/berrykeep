package io.berrykeep.android.ui

import kotlinx.coroutines.sync.Mutex

/** The title latency monitor belongs to the process-global native mobile client. */
internal object ProcessTitleLatencyNativeControl {
    val mutex = Mutex()
    val gate = LatestOperationGate()
}
